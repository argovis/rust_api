use super::schema;
use super::helpers;
use chrono::{DateTime, Utc};
use mongodb::bson::DateTime as BsonDateTime;

pub fn print_query_params(params: serde_json::Value) {
    println!("{:?}", params);
}

pub fn transform_timeseries<T: schema::IsTimeseries + Clone>(params: serde_json::Value, ts: Vec<BsonDateTime>, results: Vec<T>) -> Vec<T> {
    
    // extract query parameters //////////////////////////////////////
    let start_date = params.get("startDate")
        .and_then(|v| v.as_str())
        .and_then(helpers::string2bsondate);

    let end_date = params.get("endDate")
        .and_then(|v| v.as_str())
        .and_then(helpers::string2bsondate);


    // apply appropriate filters ////////////////////////////////////
    let mut r = results.clone();

    if start_date.is_some() || end_date.is_some() {
        r = filter_timerange(start_date, end_date, ts, r);
    }

    return r;
}

pub fn filter_timerange<T: schema::IsTimeseries>(start_date: Option<BsonDateTime>, end_date: Option<BsonDateTime>, ts: Vec<BsonDateTime>, mut results: Vec<T>) -> Vec<T> {
    // todo: sliced ts should be appended to the results
    let start_index = start_date.and_then(|start_date| {
        ts.iter().position(|&t| t >= start_date)
    }).unwrap_or(0);
    
    let end_index = end_date.and_then(|end_date| {
        ts.iter().rposition(|&t| t < end_date).map(|idx| idx + 1)
    }).unwrap_or(ts.len());

    for result in &mut results {
        let data = result.data();
        *data = data.iter().map(|inner_vec| {
            let slice = &inner_vec[start_index..end_index];
            slice.to_vec()
        }).collect();
    }

    results

}