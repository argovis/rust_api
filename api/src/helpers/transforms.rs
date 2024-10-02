use super::schema;
use chrono::{DateTime, Utc};

pub fn print_query_params(params: serde_json::Value) {
    println!("{:?}", params);
}

pub fn transform_timeseries(params: serde_json::Value, results: Vec<schema::DataSchema>) -> Vec<schema::DataSchema> {
    
    // extract query parameters //////////////////////////////////////
    let start_date = params.get("startDate")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let end_date = params.get("endDate")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    // // transform results ////////////////////////////////////////////
    // if start_date.is_some() || end_date.is_some() {
    //     let timeseries_meta = collection.find_one(doc! { "_id": results[0].metadata[0] }, None).unwrap();
    // }

    return results;
}