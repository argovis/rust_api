use chrono::{DateTime, Utc};
use mongodb::bson::DateTime as BsonDateTime;
use serde::{Serialize};
use actix_web::{HttpResponse};
use serde_json::{json, from_str};

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

pub fn create_response<T: Serialize>(results: Vec<T>) -> HttpResponse {
    if results.is_empty() {
        HttpResponse::NotFound().json("No results found")
    } else {
        HttpResponse::Ok().json(results)
    }
}

pub fn validate_query_params(params: &serde_json::Value) -> Result<(), HttpResponse> {

    // should have at most one of polygon, box and center.
    let mut count = 0;
    if params.get("polygon").is_some() {
        count += 1;
    }
    if params.get("box").is_some() {
        count += 1;
    }
    if params.get("center").is_some() {
        count += 1;
    }

    if count > 1 {
        return Err(HttpResponse::BadRequest().json(json!({"error": "At most one of 'polygon', 'box', or 'center' should be defined"})));
    }

    // 'center' and 'radius' should both be defined, or neither should be defined
    let center = params.get("center").is_some();
    let radius = params.get("radius").is_some();
    if center != radius {
        return Err(HttpResponse::BadRequest().json(json!({"error": "'center' and 'radius' should both be defined, or neither should be defined"})));
    }

    // If 'polygon' is defined, its value should be the coordinates of a single-ring polygon
    if let Some(polygon) = params.get("polygon") {
        let polygon_str = polygon.as_str().ok_or_else(|| HttpResponse::BadRequest().json(json!({"error": "'polygon' should be an array of coordinate pairs"})))?;
        let coordinates: Vec<Vec<f64>> = from_str(polygon_str).map_err(|_| HttpResponse::BadRequest().json(json!({"error": "'polygon' should be an array of coordinate pairs"})))?;

        // Check that the polygon has at least 4 points (including the repeated start/end point)
        if coordinates.len() < 4 {
            return Err(HttpResponse::BadRequest().json(json!({"error": "'polygon' should have at least 4 points"})));
        }

        // Check that the first and last points are the same
        let first_point = &coordinates[0];
        let last_point = &coordinates[coordinates.len() - 1];
        if first_point != last_point {
            return Err(HttpResponse::BadRequest().json(json!({"error": "'polygon' should be a closed ring"})));
        }

        // Check that each point is a pair of coordinates
        for point in &coordinates {
            if point.len() != 2 {
                return Err(HttpResponse::BadRequest().json(json!({"error": "Each point in 'polygon' should be a pair of coordinates"})));
            }
        }
    }

    // If 'startDate' or 'endDate' are defined, they should have the format YYYY-MM-DDTHH:MM:SSZ
    if let Some(start_date) = params.get("startDate") {
        if let Some(start_date_str) = start_date.as_str() {
            if DateTime::parse_from_rfc3339(start_date_str).is_err() {
                return Err(HttpResponse::BadRequest().json(json!({"error": "'startDate' should have the format YYYY-MM-DDTHH:MM:SSZ"})));
            }
        }
    }
    if let Some(end_date) = params.get("endDate") {
        if let Some(end_date_str) = end_date.as_str() {
            if DateTime::parse_from_rfc3339(end_date_str).is_err() {
                return Err(HttpResponse::BadRequest().json(json!({"error": "'endDate' should have the format YYYY-MM-DDTHH:MM:SSZ"})));
            }
        }
    }

    // If all validations pass, return Ok(())
    Ok(())
}