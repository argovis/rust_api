use super::schema;
use super::helpers;
use mongodb::bson::DateTime as BsonDateTime;

pub fn transform_timeseries<T: schema::IsTimeseries + Clone>(params: serde_json::Value, ts: Vec<BsonDateTime>, data_info: (Vec<String>, Vec<String>, Vec<Vec<String>>), results: Vec<T>) -> Vec<T> {
    
    // extract query parameters //////////////////////////////////////
    let start_date = params.get("startDate")
        .and_then(|v| v.as_str())
        .and_then(helpers::string2bsondate);

    let end_date = params.get("endDate")
        .and_then(|v| v.as_str())
        .and_then(helpers::string2bsondate);

    let data: Vec<String> = params.get("data")
        .and_then(|v| v.as_str())
        .map(|s| s.split(',').map(|s| s.to_string()).collect())
        .unwrap_or_else(Vec::new);

    // apply appropriate filters ////////////////////////////////////
    let mut r = results.clone();

    if start_date.is_some() || end_date.is_some() {
        r = filter_timerange(start_date, end_date, ts, r);
    }

    //println!("{:?}", data_info );
    r = filter_data(data, data_info, r);

    return r;
}

pub fn filter_timerange<T: schema::IsTimeseries>(start_date: Option<BsonDateTime>, end_date: Option<BsonDateTime>, ts: Vec<BsonDateTime>, mut results: Vec<T>) -> Vec<T> {

    let start_index = start_date.and_then(|start_date| {
        ts.iter().position(|&t| t >= start_date)
    }).unwrap_or(0);
    
    let end_index = end_date.and_then(|end_date| {
        ts.iter().rposition(|&t| t < end_date).map(|idx| idx + 1)
    }).unwrap_or(ts.len());

    let time_window: Vec<String> = ts[start_index..end_index]
        .iter()
        .map(|t| helpers::bsondate2string(t))
        .collect();

    for result in &mut results {
        let data = result.data();
        *data = data.iter().map(|inner_vec| {
            let slice = &inner_vec[start_index..end_index];
            slice.to_vec()
        }).collect();

        match result.timeseries() {
            Some(timeseries) => *timeseries = time_window.clone(),
            None => result.set_timeseries(time_window.clone()),
        }
    }

    results

}

// todo: this will probably be generic over more than just Timeseries
pub fn filter_data<T: schema::IsTimeseries>(data: Vec<String>, data_info: (Vec<String>, Vec<String>, Vec<Vec<String>>), mut results: Vec<T>) -> Vec<T> {

    if data.is_empty() {
        for result in &mut results {
            result.set_data(Vec::new());
        }
    } else {
        let indexes: Vec<usize> = data.iter()
            .filter_map(|item| data_info.0.iter().position(|x| x == item))
            .collect();

        for result in &mut results {
            let filtered_data: Vec<Vec<f64>> = indexes.iter()
                .filter_map(|&i| result.data().get(i).cloned())
                .collect();
            result.set_data(filtered_data);
        }
    }

    results
}