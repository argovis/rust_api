
use super::helpers;

use serde_json::json;
use mongodb::bson;

pub fn filter_timeseries(params: serde_json::Value) -> mongodb::bson::Document {
    // Extract the query parameters
    let polygon = params.get("polygon").map(|p| p.as_str().unwrap());
    let boxregion = params.get("box").map(|p| p.as_str().unwrap());
    let center = params.get("center").map(|p| p.as_str().unwrap());
    let radius = params.get("radius").map(|d| d.as_str().unwrap().parse::<f64>().unwrap());
    let id = params.get("id").map(|p| p.as_str().unwrap());
    let vertical_range = params.get("verticalRange").map(|p| p.as_str().unwrap());
    // todo: legacy search parameters: startDate,endDate,compression,mostrecent,data,batchmeta

    // Construct the filter
    let mut filter = mongodb::bson::doc! {};
    if let Some(id) = id {
        filter = id_filter(id, filter);
    }
    if let Some(polygon) = polygon {
        filter = polygon_filter(polygon, filter);
    }
    if let Some(boxregion) = boxregion {
        filter = box_filter(boxregion, filter);
    }
    if let (Some(center), Some(radius)) = (center, radius) {
        filter = center_filter(center, radius, filter);
    }
    if let Some(vertical_range) = vertical_range {
        filter = vertical_range_filter(vertical_range, filter);
    }

    return filter;
}

fn polygon_filter(polygon: &str, mut filter: mongodb::bson::Document) -> mongodb::bson::Document {
    let mut polygon_coordinates: Vec<Vec<f64>> = serde_json::from_str(polygon).unwrap();

    // coordinate sanitation
    polygon_coordinates = helpers::validlonlat(polygon_coordinates);

    // filter construction
    let polygon_geojson = bson::to_bson(&json!({ 
        "type": "Polygon",
        "coordinates": [polygon_coordinates]
    })).unwrap();
    filter.insert("geolocation", mongodb::bson::doc! { "$geoWithin": { "$geometry": polygon_geojson } });

    filter
}

fn box_filter(boxregion: &str, mut filter: mongodb::bson::Document) -> mongodb::bson::Document {
    let mut box_coordinates: Vec<Vec<f64>> = serde_json::from_str(boxregion).unwrap();

    // coordinate sanitation
    box_coordinates = helpers::validlonlat(box_coordinates);

    // box might cross dateline, need to split into two boxes
    let box_list = if box_coordinates[0][0] > box_coordinates[1][0] {
        vec![
            vec![box_coordinates[0].clone(), vec![180.0, box_coordinates[1][1]]],
            vec![vec![-180.0, box_coordinates[0][1]], box_coordinates[1].clone()]
        ]
    } else {
        vec![box_coordinates]
    };

    // filter construction
    let mut box_filters = Vec::new();
    for boxx in &box_list {
        let box_filter = mongodb::bson::doc! {
            "geolocation.coordinates": {
                "$geoWithin": {
                    "$box": boxx
                }
            }
        };
        box_filters.push(box_filter);
    }
    filter.insert("$or", box_filters);

    filter
}

fn center_filter(center: &str, radius: f64, mut filter: mongodb::bson::Document) -> mongodb::bson::Document {
    let center_coordinates: Vec<f64> = serde_json::from_str(center).unwrap();

    // coordinate sanitation
    let center_coordinates = helpers::validlonlat(vec![center_coordinates])[0].clone();

    // filter construction
    filter.insert("geolocation", mongodb::bson::doc! {
        "$near": {
            "$geometry": {
                "type": "Point",
                "coordinates": center_coordinates
            },
            "$maxDistance": radius
        }
    });

    filter
}

fn id_filter(id: &str, mut filter: mongodb::bson::Document) -> mongodb::bson::Document {
    filter.insert("_id", id);
    filter
}

fn vertical_range_filter(vertical_range: &str, mut filter: mongodb::bson::Document) -> mongodb::bson::Document {
    let vertical_range: Vec<f64> = serde_json::from_str(vertical_range).unwrap();
    filter.insert("level", mongodb::bson::doc! { "$gte": vertical_range[0], "$lt": vertical_range[1] });
    filter
}