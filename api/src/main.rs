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

use mongodb::{options::FindOptions, bson::Document, error::Result};
use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use once_cell::sync::Lazy;
use std::sync::Mutex;
use futures::stream::StreamExt;
use std::env;
use serde::de::DeserializeOwned;
use mongodb::bson::DateTime;
use std::collections::HashSet;

static CLIENT: Lazy<Mutex<Option<mongodb::Client>>> = Lazy::new(|| Mutex::new(None));
static TIMESERIES: Lazy<Mutex<Option<Vec<DateTime>>>> = Lazy::new(|| Mutex::new(None));
static BSOSE_DATA_INFO: Lazy<Mutex<Option<(Vec<String>, Vec<String>, Vec<Vec<String>>)>>> = Lazy::new(|| Mutex::new(None));

#[get("/search")]
async fn search_data_schema(query_params: web::Query<serde_json::Value>) -> impl Responder {
    let params = query_params.into_inner();

    let page: i64 = params.get("page")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let page_size = 1000;

    // validate query params ////////////////////////////////////////
    match helpers::validate_query_params(&params) {
        Ok(_) => {},
        Err(response) => return response,
    }

    // construct filter from query params //////////////////////////
    let filter = filters::filter_timeseries(params.clone());

    // Search for documents with matching filters //////////////////
    let options_builder = {
        FindOptions::builder()
            .sort(mongodb::bson::doc! { "_id": 1 })
            .skip(Some((page * page_size) as u64))
            .limit(page_size)
    };

    let mut cursor = generate_cursor::<schema::BsoseSchema>("argo", "bsose", filter, Some(options_builder.build())).await.unwrap();

    // extract results from db //////////////////////////////////////
    let mut results = Vec::new();
        
    while let Some(result) = cursor.next().await {
        match result {
            Ok(document) => {
                results.push(document);
            },  
            Err(e) => {
                eprintln!("Error: {}", e);
                return HttpResponse::InternalServerError().finish();
            }
        }
    }

    // transform results ////////////////////////////////////////////
    let timeseries = {
        let ts = TIMESERIES.lock().unwrap();
        ts.clone().unwrap()
    };
    let data_info = {
        let di = BSOSE_DATA_INFO.lock().unwrap();
        di.clone().unwrap()
    };
    let munged_results = transforms::transform_timeseries(params.clone(), timeseries, data_info, results);

    // return results ///////////////////////////////////////////////
    let compression: Option<String> = params.get("compression")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let batchmeta: Option<String> = params.get("batchmeta")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if let Some(compression) = compression {
        if compression == "minimal" {
            let r = transforms::timeseries_stub(munged_results.clone());
            helpers::create_response(r)
        } else {
            helpers::create_response(munged_results)
        }
    } else if let Some(_batchmeta) = batchmeta {
        let unique_metadata: HashSet<_> = munged_results.iter()
            .flat_map(|item| item.metadata.clone())
            .collect();

        let filter = mongodb::bson::doc! {
            "_id": {
                "$in": unique_metadata.into_iter().collect::<Vec<_>>()
            }
        };

        let cursor = generate_cursor::<Document>("argo", "timeseriesMeta", filter, None).await.unwrap();
        let results: Vec<_> = cursor.map(|doc| doc.unwrap()).collect().await;

        helpers::create_response(results)
    } else {
        helpers::create_response(munged_results)
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
    *BSOSE_DATA_INFO.lock().unwrap() = Some(metadata[0].data_info.clone());

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