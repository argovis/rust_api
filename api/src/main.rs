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
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use async_stream::stream;
use serde_json::{json, Value};

use dataset_config::{DatasetConfig, DatasetSource};

static CLIENT: Lazy<Mutex<Option<mongodb::Client>>> = Lazy::new(|| Mutex::new(None));

// Per-dataset source-of-truth: identity strings + values loaded from the
// dataset's meta doc at startup. Same pattern as CLIENT above — the
// `Lazy<Mutex<Option<T>>>` is what lets us declare a static that's
// initialized once, after main() reads the value out of Mongo. Handlers
// clone the inner DatasetSource out of the lock at the top of each
// request and use it locally, so the mutex is never held across `.await`.
//
// One static per dataset. Adding a new dataset is one more static here
// plus one more load_dataset_source/ load-and-set block in main().
static BSOSE_SOURCE: Lazy<Mutex<Option<DatasetSource>>> = Lazy::new(|| Mutex::new(None));

// ---- route handlers --------------------------------------------------------
//
// Each dataset gets its own one-route handler that resolves the dataset
// generic and hands off to `serve_timeseries`. Keeps Actix's `#[get(...)]`
// attribute discoverable per-dataset and lets the routing table grow
// without touching the generic core. Adding a new dataset is: define its
// `*_CONFIG` in `dataset_config.rs`, define its schema + meta in
// `schema.rs`, add a `*_SOURCE` static + a `load_dataset_source` call
// in `main()`, and add a 4-line handler here.

#[get("/timeseries/bsose")]
async fn bsose_handler(
    req: HttpRequest,
    query_params: web::Query<serde_json::Value>,
) -> impl Responder {
    // Snapshot the source out of the lock before any `.await`. Holding
    // the mutex guard across an await would let other tasks on the same
    // worker deadlock on it, so the idiom is "lock, clone, unlock, then
    // do async work with the local copy."
    let source = BSOSE_SOURCE
        .lock()
        .unwrap()
        .as_ref()
        .expect("BSOSE_SOURCE not initialized at startup")
        .clone();

    serve_timeseries::<schema::BsoseSchema>(
        req,
        query_params.into_inner(),
        &dataset_config::BSOSE_CONFIG,
        &source,
    )
    .await
}

// ---- generic timeseries handler --------------------------------------------

/// Generic body of the `/timeseries/{dataset}` endpoint. Parameterized by
/// `S`, the per-dataset data-doc schema (e.g. `BsoseSchema`). The dataset's
/// request-size policy comes in as `config` (tile size, level set, radius
/// cap, coverage); its Mongo identity and startup caches come in as
/// `source` (db/collection names, the cached `timeseries` axis and the
/// cached meta-level `data_info` default).
///
/// Behaviour is the same as the previous BSOSE-specific handler: validate
/// the query, generate the tile sequence, then probe-forward through
/// tiles serving at most one non-empty tile per request. The two branches
/// (streaming docs vs. `batchmeta` metadata lookup) are unchanged from
/// the original implementation.
async fn serve_timeseries<S>(
    req: HttpRequest,
    params: serde_json::Value,
    config: &DatasetConfig,
    source: &DatasetSource,
) -> HttpResponse
where
    S: schema::IsTimeseries
        + DeserializeOwned
        + Serialize
        + Send
        + Sync
        + Unpin
        + 'static,
{
    // The next_url we emit on success uses this request's own path, so the
    // generated URL stays correct even if the route is re-mounted later.
    let path = req.path().to_string();

    // ---- validation ---------------------------------------------------
    if let Err(response) = helpers::validate_query_params(&params) {
        return response;
    }
    if let Err(response) = helpers::validate_radius_cap(&params, config) {
        return response;
    }

    let start_idx = match pagination::parse_tile_index(&params) {
        Ok(i) => i,
        Err(e) => return HttpResponse::BadRequest().json(json!({"error": e})),
    };

    // ---- tile sequence + startup-loaded data --------------------------
    let tiles = tile_generator::generate_tiles(&params, config);

    // Clone the cached values out of `source` locally so the streaming
    // branch can move owned copies into its async generator without
    // juggling lifetimes — both are small (timeseries is a few KB of
    // dates; data_info is a few strings), so cloning per request is
    // cheap relative to the actual query work.
    let timeseries = source.timeseries.clone();
    let cached_data_info = source.data_info.clone();

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

        let mut cursor = match generate_cursor::<S>(
            source.db_name,
            source.collection,
            filter,
            Some(options),
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
                        if let Some(t) = transforms::transform_timeseries(
                            &params,
                            &timeseries,
                            &cached_data_info,
                            doc,
                        ) {
                            for m in t.metadata().into_iter() {
                                unique_metadata.insert(m);
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
                source.db_name,
                source.meta_collection,
                meta_filter,
                None,
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
        let mut first_doc: Option<S> = None;
        while let Some(result) = cursor.next().await {
            match result {
                Ok(doc) => {
                    if let Some(t) = transforms::transform_timeseries(
                        &params,
                        &timeseries,
                        &cached_data_info,
                        doc,
                    ) {
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
        // Each of these gets moved into the stream! generator. params,
        // timeseries, and the data_info cache are needed to transform
        // each subsequent doc; the rest are emitted at the end of the
        // response.
        let params_for_stream = params.clone();
        let ts_for_stream = timeseries.clone();
        let data_info_for_stream = cached_data_info.clone();

        let body = stream! {
            yield Ok::<_, Infallible>(web::Bytes::from_static(b"{\"docs\":["));

            // Serialize the peeked first doc.
            let first_bytes = if is_minimal {
                let stub = transforms::timeseries_stub(&first_doc);
                serde_json::to_vec(&stub).expect("serializing one stub should not fail")
            } else {
                serde_json::to_vec(&first_doc)
                    .expect("serializing a timeseries doc should not fail")
            };
            yield Ok(web::Bytes::from(first_bytes));

            while let Some(result) = cursor.next().await {
                match result {
                    Ok(doc) => {
                        if let Some(t) = transforms::transform_timeseries(
                            &params_for_stream,
                            &ts_for_stream,
                            &data_info_for_stream,
                            doc,
                        ) {
                            let bytes = if is_minimal {
                                let stub = transforms::timeseries_stub(&t);
                                serde_json::to_vec(&stub)
                                    .expect("serializing one stub should not fail")
                            } else {
                                serde_json::to_vec(&t)
                                    .expect("serializing a timeseries doc should not fail")
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

/// Build a `DatasetSource` by reading the dataset's meta doc out of Mongo.
///
/// Called once per dataset at server startup. The four `'static str`
/// arguments are the dataset's Mongo identity; the resulting struct
/// bundles those identity strings with the values we read out of the
/// meta doc (`timeseries`, `data_info`). `main()` then stashes the
/// returned struct into the dataset's `*_SOURCE` static for handlers
/// to read on each request.
///
/// Panics if the meta doc can't be found or the cursor errors. Both
/// indicate startup misconfiguration that should fail loudly rather
/// than serve stale or partial data.
async fn load_dataset_source<M>(
    db_name: &'static str,
    collection: &'static str,
    meta_collection: &'static str,
    meta_data_type: &'static str,
) -> Result<DatasetSource>
where
    M: schema::IsTimeseriesMeta + DeserializeOwned + Unpin + Send + Sync,
{
    let filter = mongodb::bson::doc! {"data_type": meta_data_type};
    let options = FindOptions::builder().limit(1).build();
    let mut cursor =
        generate_cursor::<M>(db_name, meta_collection, filter, Some(options)).await?;

    let meta = match cursor.next().await {
        Some(Ok(m)) => m,
        Some(Err(e)) => panic!(
            "Error reading meta doc for {}.{} (data_type={}): {}",
            db_name, meta_collection, meta_data_type, e
        ),
        None => panic!(
            "No meta doc found in {}.{} matching data_type={}",
            db_name, meta_collection, meta_data_type
        ),
    };

    Ok(DatasetSource {
        db_name,
        collection,
        meta_collection,
        meta_data_type,
        timeseries: meta.timeseries(),
        data_info: meta.data_info(),
    })
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {

    // Initialize the MongoDB client
    let client_options = mongodb::options::ClientOptions::parse(env::var("MONGODB_URI").unwrap()).await.unwrap();
    let client = mongodb::Client::with_options(client_options).unwrap();
    *CLIENT.lock().unwrap() = Some(client);

    // Load each dataset's source-of-truth and stash it in the static for
    // handlers to read. The Mongo identity strings live here at the call
    // site — one place per dataset — and the returned struct bundles
    // them with the values read from the meta doc. Adding a new dataset
    // is one more load-and-set block here.
    let bsose = load_dataset_source::<schema::BsoseMeta>(
        "argo",
        "bsose",
        "timeseriesMeta",
        "BSOSE-profile",
    )
    .await
    .expect("failed to load BSOSE dataset source at startup");
    *BSOSE_SOURCE.lock().unwrap() = Some(bsose);

    HttpServer::new(|| {
        App::new()
            .service(bsose_handler)
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