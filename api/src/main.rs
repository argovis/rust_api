/*
todo:

demo critical
 -- done --

production critical
rate limiting
unit testing
legacy search parameters: mostrecent

nice to have someday
transform logic as traits?
*/

use api::helpers::transforms;
use api::helpers::schema;
use api::helpers::helpers;
use api::helpers::dataset_config;
use api::helpers::tile_generator;
use api::helpers::filter_composer;
use api::helpers::pagination;

use mongodb::{options::FindOptions, bson::Document, error::Result};
use actix_web::{get, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use once_cell::sync::Lazy;
use std::sync::Mutex;
use futures::stream::StreamExt;
use std::env;
use std::convert::Infallible;
use serde::de::DeserializeOwned;
use mongodb::bson::DateTime;
use std::collections::HashSet;
use async_stream::stream;
use serde_json::{json, Value};

static CLIENT: Lazy<Mutex<Option<mongodb::Client>>> = Lazy::new(|| Mutex::new(None));
static TIMESERIES: Lazy<Mutex<Option<Vec<DateTime>>>> = Lazy::new(|| Mutex::new(None));

#[get("/timeseries/bsose")]
async fn search_data_schema(
    req: HttpRequest,
    query_params: web::Query<serde_json::Value>,
) -> impl Responder {
    let params = query_params.into_inner();

    // Dataset-specific request-size policy: tile size, level set, radius cap.
    let config = &dataset_config::BSOSE_CONFIG;

    // The next_url we emit on success uses this request's own path, so the
    // generated URL stays correct even if the route is re-mounted later.
    let path = req.path().to_string();

    // ---- validation ---------------------------------------------------
    if let Err(response) = helpers::validate_query_params(&params) {
        return response;
    }

    let start_idx = match pagination::parse_tile_index(&params) {
        Ok(i) => i,
        Err(e) => return HttpResponse::BadRequest().json(json!({"error": e})),
    };

    // ---- tile sequence + cached startup data --------------------------
    let tiles = tile_generator::generate_tiles(&params, config);

    let timeseries = {
        let ts = TIMESERIES.lock().unwrap();
        ts.clone().unwrap()
    };

    let compression: Option<String> = params
        .get("compression")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let batchmeta: Option<String> = params
        .get("batchmeta")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let is_minimal = matches!(compression.as_deref(), Some("minimal"));

    // ---- probe-forward loop -------------------------------------------
    //
    // For each candidate tile (starting at the caller-supplied tile_index),
    // open a cursor and look for output. The flavour of "look" differs by
    // branch: streaming peeks for the first doc that survives transformation
    // (and keeps the cursor so the rest can be streamed straight through);
    // batchmeta drains the entire cursor to collect unique metadata IDs
    // (no streaming, but per-tile bounded by tile size). Either way, an
    // empty tile drops through to the next iteration. We plod through
    // empty tiles one at a time; a future land-mask short-circuit could
    // replace this with a smarter skip.
    for tile_idx in start_idx..tiles.len() {
        let tile = &tiles[tile_idx];
        let filter = filter_composer::compose_filter_with_tile(params.clone(), tile, config);
        let options = FindOptions::builder().build();

        let mut cursor = match generate_cursor::<schema::BsoseSchema>(
            "argo", "bsose", filter, Some(options),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error opening cursor for tile {}: {}", tile_idx, e);
                return HttpResponse::InternalServerError().finish();
            }
        };

        if batchmeta.is_some() {
            // ---- batchmeta branch --------------------------------------
            let mut unique_metadata: HashSet<String> = HashSet::new();
            while let Some(result) = cursor.next().await {
                match result {
                    Ok(doc) => {
                        if let Some(t) =
                            transforms::transform_timeseries(&params, &timeseries, doc)
                        {
                            for m in t.metadata.iter() {
                                unique_metadata.insert(m.clone());
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Cursor error during batchmeta drain: {}", e);
                        break;
                    }
                }
            }

            if unique_metadata.is_empty() {
                continue; // tile produced no metadata — try the next one.
            }

            let meta_filter = mongodb::bson::doc! {
                "_id": { "$in": unique_metadata.into_iter().collect::<Vec<_>>() }
            };
            let meta_cursor = match generate_cursor::<Document>(
                "argo", "timeseriesMeta", meta_filter, None,
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error opening metadata cursor: {}", e);
                    return HttpResponse::InternalServerError().finish();
                }
            };
            let docs: Vec<_> = meta_cursor.map(|d| d.unwrap()).collect().await;

            return HttpResponse::Ok().json(json!({
                "docs": docs,
                "next_url": next_url_value(&path, &params, tile_idx, tiles.len()),
                "message": format!("page {}", tile_idx),
            }));
        }

        // ---- streaming branch ------------------------------------------
        //
        // Peek-ahead until we find a doc that survives transformation.
        // If none, advance to the next tile. If found, keep the cursor —
        // we'll continue draining it from inside the response body.
        let mut first_doc: Option<schema::BsoseSchema> = None;
        while let Some(result) = cursor.next().await {
            match result {
                Ok(doc) => {
                    if let Some(t) =
                        transforms::transform_timeseries(&params, &timeseries, doc)
                    {
                        first_doc = Some(t);
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Cursor error during peek for tile {}: {}", tile_idx, e);
                    return HttpResponse::InternalServerError().finish();
                }
            }
        }

        let first_doc = match first_doc {
            Some(d) => d,
            None => continue, // tile contributed no surviving docs — try next.
        };

        let next_url = next_url_value(&path, &params, tile_idx, tiles.len());
        let page_message = format!("page {}", tile_idx);
        // Each of these gets moved into the stream! generator. params and
        // timeseries are needed to transform each subsequent doc; the rest
        // are emitted at the end of the response.
        let params_for_stream = params.clone();
        let ts_for_stream = timeseries.clone();

        let body = stream! {
            yield Ok::<_, Infallible>(web::Bytes::from_static(b"{\"docs\":["));

            // Serialize the peeked first doc.
            let first_bytes = if is_minimal {
                let stub = transforms::timeseries_stub(&first_doc);
                serde_json::to_vec(&stub).expect("serializing one stub should not fail")
            } else {
                serde_json::to_vec(&first_doc)
                    .expect("serializing one bsose doc should not fail")
            };
            yield Ok(web::Bytes::from(first_bytes));

            while let Some(result) = cursor.next().await {
                match result {
                    Ok(doc) => {
                        if let Some(t) = transforms::transform_timeseries(
                            &params_for_stream, &ts_for_stream, doc,
                        ) {
                            let bytes = if is_minimal {
                                let stub = transforms::timeseries_stub(&t);
                                serde_json::to_vec(&stub)
                                    .expect("serializing one stub should not fail")
                            } else {
                                serde_json::to_vec(&t)
                                    .expect("serializing one bsose doc should not fail")
                            };
                            yield Ok(web::Bytes::from_static(b","));
                            yield Ok(web::Bytes::from(bytes));
                        }
                    }
                    Err(e) => {
                        // Mid-stream error: status is already 200, so we
                        // can only close the JSON cleanly and stop.
                        eprintln!("Cursor error during stream: {}", e);
                        break;
                    }
                }
            }

            // Close the docs array and emit the trailer fields. The
            // serialize calls only fail on non-finite floats inside the
            // value; for the strings/Null we use here that's impossible,
            // but we fall back to safe bytes if for some reason it does.
            yield Ok(web::Bytes::from_static(b"],\"next_url\":"));
            yield Ok(web::Bytes::from(
                serde_json::to_vec(&next_url).unwrap_or_else(|_| b"null".to_vec()),
            ));
            yield Ok(web::Bytes::from_static(b",\"message\":"));
            yield Ok(web::Bytes::from(
                serde_json::to_vec(&page_message)
                    .unwrap_or_else(|_| b"\"\"".to_vec()),
            ));
            yield Ok(web::Bytes::from_static(b"}"));
        };

        return HttpResponse::Ok()
            .content_type("application/json")
            .streaming(body);
    }

    // ---- no non-empty tile in the requested range ---------------------
    //
    // Empty results no longer return 404 — the paginated contract is that
    // `next_url: null` means "no more data", so empty must remain 200.
    HttpResponse::Ok().json(json!({
        "docs": [],
        "next_url": Value::Null,
        "message": format!("no non-empty tiles from index {}", start_idx),
    }))
}

/// Build the JSON value emitted as `next_url`. Returns `Value::Null` when
/// the just-served tile was the last one (no further pages exist).
fn next_url_value(
    path: &str,
    params: &serde_json::Value,
    current_idx: usize,
    total_tiles: usize,
) -> Value {
    if current_idx + 1 < total_tiles {
        Value::String(pagination::build_next_url(path, params, current_idx + 1))
    } else {
        Value::Null
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {

    // Initialize the MongoDB client
    let client_options = mongodb::options::ClientOptions::parse(env::var("MONGODB_URI").unwrap()).await.unwrap();
    let client = mongodb::Client::with_options(client_options).unwrap(); 
    *CLIENT.lock().unwrap() = Some(client);

    // some generic data useful to have on hand
    let mut filter = mongodb::bson::doc! {"data_type": "BSOSE-profile"};
    let mut options = FindOptions::builder().limit(1).build();
    let mut metacursor = generate_cursor::<schema::BsoseMeta>("argo", "timeseriesMeta", filter, Some(options)).await.unwrap();
    let mut metadata = Vec::new();
    while let Some(result) = metacursor.next().await {
        match result {
            Ok(document) => {
                metadata.push(document);
            },  
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    }
    *TIMESERIES.lock().unwrap() = Some(metadata[0].timeseries.clone());

    HttpServer::new(|| {
        App::new()
            .service(search_data_schema)
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}

async fn generate_cursor<T: DeserializeOwned>(db_name: &str, collection_name: &str, filter: Document, options: Option<FindOptions>) -> Result<mongodb::Cursor<T>> {
    let client = {
        let guard = match CLIENT.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.as_ref() {
            Some(client) => client.clone(),
            None => return Err(mongodb::error::Error::from(std::io::Error::new(std::io::ErrorKind::Other, "Client is None"))),
        }
    };
    client.database(db_name).collection::<T>(collection_name).find(filter, options).await
}