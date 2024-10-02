use api::helpers::filters;
use api::helpers::transforms;
use api::helpers::schema;

use mongodb::{options::{FindOptions}};
use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use once_cell::sync::Lazy;
use std::sync::Mutex;
use futures::stream::StreamExt;
use std::env;

static CLIENT: Lazy<Mutex<Option<mongodb::Client>>> = Lazy::new(|| Mutex::new(None));

#[get("/query_params")]
async fn get_query_params(query_params: web::Query<serde_json::Value>) -> impl Responder {
    let params = query_params.into_inner();

    transforms::print_query_params(params.clone());

    HttpResponse::Ok().json(params)
}

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

    let mut cursor = {
        let options = options_builder.build(); 
        let guard = match CLIENT.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let client = guard.as_ref().unwrap();
        client.database("argo").collection::<schema::DataSchema>("bsose").find(filter, options).await.unwrap()
    }; // in theory the mutex is unlocked here, holding it as little as possible

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
    let munged_results = transforms::transform_timeseries(params.clone(), results);

    HttpResponse::Ok().json(munged_results)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {

    // Initialize the MongoDB client
    let client_options = mongodb::options::ClientOptions::parse(env::var("MONGODB_URI").unwrap()).await.unwrap();
    let client = mongodb::Client::with_options(client_options).unwrap(); 
    *CLIENT.lock().unwrap() = Some(client);

    HttpServer::new(|| {
        App::new()
            .service(get_query_params)
            .service(search_data_schema)
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
