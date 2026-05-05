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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- validlonlat ---------------------------------------------------------

    #[test]
    fn validlonlat_passes_through_in_range_coords() {
        let coords = vec![vec![10.0, 20.0], vec![-50.0, -45.0]];
        let out = validlonlat(coords.clone());
        assert_eq!(out, coords);
    }

    #[test]
    fn validlonlat_wraps_longitude_above_180() {
        // 200 % 360 = 200, then > 180, so 200 - 360 = -160
        let out = validlonlat(vec![vec![200.0, 0.0]]);
        assert!((out[0][0] - -160.0).abs() < 1e-9);
    }

    #[test]
    fn validlonlat_wraps_longitude_below_negative_180() {
        // -200 % 360 = -200 (Rust f64 % preserves sign), then < -180, so -200 + 360 = 160
        let out = validlonlat(vec![vec![-200.0, 0.0]]);
        assert!((out[0][0] - 160.0).abs() < 1e-9);
    }

    #[test]
    fn validlonlat_clips_latitude_above_90() {
        // 95 % 180 = 95, > 90 -> clipped to 90
        let out = validlonlat(vec![vec![0.0, 95.0]]);
        assert_eq!(out[0][1], 90.0);
    }

    #[test]
    fn validlonlat_clips_latitude_below_negative_90() {
        let out = validlonlat(vec![vec![0.0, -95.0]]);
        assert_eq!(out[0][1], -90.0);
    }

    #[test]
    fn validlonlat_ignores_malformed_pairs() {
        // anything not length 2 is passed through untouched
        let coords = vec![vec![1.0, 2.0, 3.0]];
        let out = validlonlat(coords.clone());
        assert_eq!(out, coords);
    }

    // ---- date round-trips ----------------------------------------------------

    #[test]
    fn string_to_bson_to_string_round_trips() {
        let s = "2020-06-15T12:34:56Z";
        let d = string2bsondate(s).expect("should parse");
        let back = bsondate2string(&d);
        assert_eq!(back, s);
    }

    #[test]
    fn string2bsondate_rejects_garbage() {
        assert!(string2bsondate("not a date").is_none());
    }

    // ---- create_response -----------------------------------------------------

    #[test]
    fn create_response_returns_404_when_empty() {
        let resp = create_response::<i32>(vec![]);
        assert_eq!(resp.status(), 404);
    }

    #[test]
    fn create_response_returns_200_when_populated() {
        let resp = create_response(vec![1, 2, 3]);
        assert_eq!(resp.status(), 200);
    }

    // ---- validate_query_params -----------------------------------------------

    #[test]
    fn validate_accepts_empty_params() {
        let params = json!({});
        assert!(validate_query_params(&params).is_ok());
    }

    #[test]
    fn validate_rejects_two_geo_params() {
        let params = json!({
            "polygon": "[[0,0],[1,0],[1,1],[0,0]]",
            "box": "[[0,0],[1,1]]"
        });
        let err = validate_query_params(&params).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn validate_rejects_three_geo_params() {
        let params = json!({
            "polygon": "[[0,0],[1,0],[1,1],[0,0]]",
            "box": "[[0,0],[1,1]]",
            "center": "[0,0]"
        });
        let err = validate_query_params(&params).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn validate_rejects_center_without_radius() {
        let params = json!({"center": "[0,0]"});
        assert!(validate_query_params(&params).is_err());
    }

    #[test]
    fn validate_rejects_radius_without_center() {
        let params = json!({"radius": "1000"});
        assert!(validate_query_params(&params).is_err());
    }

    #[test]
    fn validate_accepts_center_and_radius() {
        let params = json!({"center": "[0,0]", "radius": "1000"});
        assert!(validate_query_params(&params).is_ok());
    }

    #[test]
    fn validate_rejects_polygon_too_few_points() {
        let params = json!({"polygon": "[[0,0],[1,0],[0,0]]"}); // only 3 points
        assert!(validate_query_params(&params).is_err());
    }

    #[test]
    fn validate_rejects_polygon_not_closed() {
        let params = json!({"polygon": "[[0,0],[1,0],[1,1],[0,1]]"}); // first != last
        assert!(validate_query_params(&params).is_err());
    }

    #[test]
    fn validate_rejects_polygon_with_bad_point() {
        let params = json!({"polygon": "[[0,0,0],[1,0],[1,1],[0,0,0]]"});
        assert!(validate_query_params(&params).is_err());
    }

    #[test]
    fn validate_accepts_well_formed_polygon() {
        let params = json!({"polygon": "[[0,0],[1,0],[1,1],[0,1],[0,0]]"});
        assert!(validate_query_params(&params).is_ok());
    }

    #[test]
    fn validate_rejects_unparseable_polygon_string() {
        let params = json!({"polygon": "not json"});
        assert!(validate_query_params(&params).is_err());
    }

    #[test]
    fn validate_rejects_bad_start_date() {
        let params = json!({"startDate": "yesterday"});
        assert!(validate_query_params(&params).is_err());
    }

    #[test]
    fn validate_rejects_bad_end_date() {
        let params = json!({"endDate": "2020/01/01"});
        assert!(validate_query_params(&params).is_err());
    }

    #[test]
    fn validate_accepts_rfc3339_dates() {
        let params = json!({
            "startDate": "2020-01-01T00:00:00Z",
            "endDate":   "2020-12-31T23:59:59Z"
        });
        assert!(validate_query_params(&params).is_ok());
    }
}