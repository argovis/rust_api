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

use api::helpers::filters;
use api::helpers::transforms;
use api::helpers::schema;
use api::helpers::helpers;
use api::helpers::dataset_config;

use mongodb::{options::FindOptions, bson::Document, error::Result};
use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use once_cell::sync::Lazy;
use std::sync::Mutex;
use futures::stream::StreamExt;
use std::env;
use std::convert::Infallible;
use serde::de::DeserializeOwned;
use mongodb::bson::DateTime;
use std::collections::HashSet;
use async_stream::stream;

static CLIENT: Lazy<Mutex<Option<mongodb::Client>>> = Lazy::new(|| Mutex::new(None));
static TIMESERIES: Lazy<Mutex<Option<Vec<DateTime>>>> = Lazy::new(|| Mutex::new(None));

#[get("/timeseries/bsose")]
async fn search_data_schema(query_params: web::Query<serde_json::Value>) -> impl Responder {
    let params = query_params.into_inner();

    // Dataset-specific request-size policy. Step 1 of the pagination work
    // just binds this; later steps will consume `tile_degrees` (for tile
    // generation) and `max_radius_meters` (for center+radius caps).
    let _config = &dataset_config::BSOSE_CONFIG;

    // validate query params ////////////////////////////////////////
    match helpers::validate_query_params(&params) {
        Ok(_) => {},
        Err(response) => return response,
    }

    // construct filter from query params //////////////////////////
    let filter = filters::filter_timeseries(params.clone());

    // open the cursor //////////////////////////////////////////////
    let options = FindOptions::builder().build();
    let mut cursor = match generate_cursor::<schema::BsoseSchema>("argo", "bsose", filter, Some(options)).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error opening cursor: {}", e);
            return HttpResponse::InternalServerError().finish();
        }
    };

    // grab the cached timeseries vector once
    let timeseries = {
        let ts = TIMESERIES.lock().unwrap();
        ts.clone().unwrap()
    };

    let compression: Option<String> = params.get("compression")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let batchmeta: Option<String> = params.get("batchmeta")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // -------------------------------------------------------------------
    // batchmeta: drain the bsose cursor, but only keep the (small) set of
    // unique metadata IDs in memory. Then fetch the metadata documents and
    // return them as a normal JSON array. Worst-case memory is bounded by
    // the number of distinct metadata ids, not the number of bsose hits.
    // -------------------------------------------------------------------
    if batchmeta.is_some() {
        let mut unique_metadata: HashSet<String> = HashSet::new();
        while let Some(result) = cursor.next().await {
            match result {
                Ok(doc) => {
                    if let Some(t) = transforms::transform_timeseries(&params, &timeseries, doc) {
                        for m in t.metadata.iter() {
                            unique_metadata.insert(m.clone());
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Cursor error: {}", e);
                    return HttpResponse::InternalServerError().finish();
                }
            }
        }
        if unique_metadata.is_empty() {
            return helpers::create_response::<Document>(vec![]);
        }
        let meta_filter = mongodb::bson::doc! {
            "_id": { "$in": unique_metadata.into_iter().collect::<Vec<_>>() }
        };
        let meta_cursor = match generate_cursor::<Document>("argo", "timeseriesMeta", meta_filter, None).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error opening metadata cursor: {}", e);
                return HttpResponse::InternalServerError().finish();
            }
        };
        let results: Vec<_> = meta_cursor.map(|doc| doc.unwrap()).collect().await;
        return helpers::create_response(results);
    }

    // -------------------------------------------------------------------
    // Default and compression=minimal both stream the bsose cursor through
    // the per-document transforms straight to the HTTP response, never
    // materializing the full result set in memory.
    //
    // We do still need to peek ahead until we've found at least one doc
    // that survives transformation, so we can preserve the existing
    // 404-on-empty contract. Once we have one survivor in hand, we open
    // the streamed JSON array `[`, emit it, and continue draining the
    // cursor doc-by-doc.
    // -------------------------------------------------------------------
    let is_minimal = matches!(compression.as_deref(), Some("minimal"));

    let mut first_doc: Option<schema::BsoseSchema> = None;
    while let Some(result) = cursor.next().await {
        match result {
            Ok(doc) => {
                if let Some(t) = transforms::transform_timeseries(&params, &timeseries, doc) {
                    first_doc = Some(t);
                    break;
                }
            }
            Err(e) => {
                eprintln!("Cursor error: {}", e);
                return HttpResponse::InternalServerError().finish();
            }
        }
    }

    let first_doc = match first_doc {
        Some(d) => d,
        None => return helpers::create_response::<schema::BsoseSchema>(vec![]),
    };

    // Stream owns: cursor, params, timeseries, first_doc, is_minimal.
    // Cursor errors mid-stream are logged and end the stream; we cannot
    // change the HTTP status after bytes have been sent, so we close the
    // JSON array cleanly and let the caller see whatever they already got.
    let body = stream! {
        yield Ok::<_, Infallible>(web::Bytes::from_static(b"["));

        // Project + serialize the buffered first doc.
        let first_bytes = if is_minimal {
            let stub = transforms::timeseries_stub(&first_doc);
            serde_json::to_vec(&stub).expect("serializing one stub should not fail")
        } else {
            serde_json::to_vec(&first_doc).expect("serializing one bsose doc should not fail")
        };
        yield Ok(web::Bytes::from(first_bytes));

        while let Some(result) = cursor.next().await {
            match result {
                Ok(doc) => {
                    if let Some(t) = transforms::transform_timeseries(&params, &timeseries, doc) {
                        let bytes = if is_minimal {
                            let stub = transforms::timeseries_stub(&t);
                            serde_json::to_vec(&stub).expect("serializing one stub should not fail")
                        } else {
                            serde_json::to_vec(&t).expect("serializing one bsose doc should not fail")
                        };
                        yield Ok(web::Bytes::from_static(b","));
                        yield Ok(web::Bytes::from(bytes));
                    }
                }
                Err(e) => {
                    eprintln!("Cursor error during stream: {}", e);
                    break;
                }
            }
        }

        yield Ok(web::Bytes::from_static(b"]"));
    };

    HttpResponse::Ok()
        .content_type("application/json")
        .streaming(body)
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