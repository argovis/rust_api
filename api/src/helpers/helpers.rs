use chrono::{DateTime, Utc};
use mongodb::bson::DateTime as BsonDateTime;

pub fn validlonlat(coords: Vec<Vec<f64>>) -> Vec<Vec<f64>> {
    coords.into_iter().map(|mut pair| {
        if pair.len() == 2 {
            pair[0] = pair[0] % 360.0;
            pair[0] = if pair[0] > 180.0 { pair[0] - 360.0 } else if pair[0] < -180.0 { pair[0] + 360.0 } else { pair[0] };
            pair[1] = pair[1] % 180.0;
            pair[1] = if pair[1] > 90.0 { 90.0 } else if pair[1] < -90.0 { -90.0 } else { pair[1] };
        }
        pair
    }).collect()
}

pub fn string2bsondate(date_str: &str) -> Option<BsonDateTime> {
    date_str.parse::<DateTime<Utc>>().ok()
        .map(|dt| BsonDateTime::from_millis(dt.timestamp_millis()))
}

pub fn bsondate2string(date: &BsonDateTime) -> String {
    let millis = date.timestamp_millis();
    let datetime = DateTime::<Utc>::from_timestamp(millis / 1000, (millis % 1000) as u32 * 1_000_000);
    match datetime {
        Some(dt) => dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        None => String::from("Invalid timestamp"),
    }
}