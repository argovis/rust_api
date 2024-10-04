use super::schema;
use chrono::{DateTime, Utc};
use mongodb::bson::DateTime as BsonDateTime;

pub fn print_query_params(params: serde_json::Value) {
    println!("{:?}", params);
}

pub fn transform_timeseries<T: schema::IsTimeseries>(params: serde_json::Value, ts: Vec<BsonDateTime>, results: Vec<T>) -> Vec<T> {
    
    // extract query parameters //////////////////////////////////////
    let start_date = params.get("startDate")
        .and_then(|v| v.as_str())  // Extract as a string
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())  // Try parsing as DateTime<Utc>
        .map(|dt| BsonDateTime::from_millis(dt.timestamp_millis()));  // Convert to BSON DateTime

    let end_date = params.get("endDate")
        .and_then(|v| v.as_str())  // Extract as a string
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())  // Try parsing as DateTime<Utc>
        .map(|dt| BsonDateTime::from_millis(dt.timestamp_millis()));  // Convert to BSON DateTime


    let start_index = start_date.and_then(|start_date| {
        ts.iter().position(|&t| t >= start_date)
    }).unwrap_or(0);
    
    let end_index = end_date.and_then(|end_date| {
        ts.iter().rposition(|&t| t < end_date).map(|idx| idx + 1)
    }).unwrap_or(ts.len());

    let ts_slice = &ts[start_index..end_index];
    println!("{:?}", ts_slice);

    // // transform results ////////////////////////////////////////////
    // if start_date.is_some() || end_date.is_some() {
    //     let timeseries_meta = collection.find_one(doc! { "_id": results[0].metadata[0] }, None).unwrap();
    // }

    return results;
}