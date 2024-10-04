use api::helpers::filters;
use api::helpers::transforms;
use api::helpers::schema;

use mongodb::{options::FindOptions, bson::Document, error::Result};
use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use once_cell::sync::Lazy;
use std::sync::Mutex;
use futures::stream::StreamExt;
use std::env;
use serde::de::DeserializeOwned;
use mongodb::bson::DateTime;

static CLIENT: Lazy<Mutex<Option<mongodb::Client>>> = Lazy::new(|| Mutex::new(None));
static TIMESERIES: Lazy<Mutex<Option<Vec<DateTime>>>> = Lazy::new(|| Mutex::new(None));
static BSOSE_DATA_INFO: Lazy<Mutex<Option<(Vec<String>, Vec<String>, Vec<Vec<String>>)>>> = Lazy::new(|| Mutex::new(None));

#[get("/search")]
async fn search_data_schema(query_params: web::Query<serde_json::Value>) -> impl Responder {
    let params = query_params.into_inner();

    // todo: validate query params /////////////////////////////////

    // construct filter from query params //////////////////////////
    let filter = filters::filter_timeseries(params.clone());

    // Search for documents with matching filters //////////////////
    let options_builder = {
        FindOptions::builder()
            //.sort(mongodb::bson::doc! { "JULD": -1 })
            //.skip(page * (page_size as u64))
            //.limit(page_size)
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

    HttpResponse::Ok().json(munged_results)
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
            .service(get_query_params)
            .service(search_data_schema)
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}

// async fn generate_cursor<T: DeserializeOwned>(db_name: &str, collection_name: &str, filter: Document, options: Option<FindOptions>) -> Result<mongodb::Cursor<T>> {
//     let guard = match CLIENT.lock() {
//         Ok(guard) => guard,
//         Err(poisoned) => poisoned.into_inner(),
//     };
//     let client = match guard.as_ref() {
//         Some(client) => client,
//         None => return Err(mongodb::error::Error::from(std::io::Error::new(std::io::ErrorKind::Other, "Client is None"))),
//     };
//     client.database(db_name).collection::<T>(collection_name).find(filter, options).await
// }

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