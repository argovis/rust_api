
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_params_produce_empty_filter() {
        let f = filter_timeseries(json!({}));
        assert_eq!(f.len(), 0);
    }

    #[test]
    fn id_filter_sets_id_equality() {
        let f = filter_timeseries(json!({"id": "doc1"}));
        assert_eq!(f.get_str("_id").unwrap(), "doc1");
    }

    #[test]
    fn vertical_range_filter_uses_gte_and_lt() {
        let f = filter_timeseries(json!({"verticalRange": "[5.0, 50.0]"}));
        let level = f.get_document("level").unwrap();
        assert!((level.get_f64("$gte").unwrap() - 5.0).abs() < 1e-9);
        assert!((level.get_f64("$lt").unwrap() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn polygon_filter_builds_geowithin_geometry() {
        let f = filter_timeseries(json!({
            "polygon": "[[0,0],[10,0],[10,10],[0,10],[0,0]]"
        }));
        let geo = f.get_document("geolocation").unwrap();
        let within = geo.get_document("$geoWithin").unwrap();
        let geometry = within.get_document("$geometry").unwrap();
        assert_eq!(geometry.get_str("type").unwrap(), "Polygon");
        // coordinates should be a single ring (array of arrays of arrays)
        let coords = geometry.get_array("coordinates").unwrap();
        assert_eq!(coords.len(), 1);
    }

    #[test]
    fn center_filter_builds_geonear() {
        let f = filter_timeseries(json!({
            "center": "[10.0, 20.0]",
            "radius": "5000"
        }));
        let geo = f.get_document("geolocation").unwrap();
        let near = geo.get_document("$near").unwrap();
        let geometry = near.get_document("$geometry").unwrap();
        assert_eq!(geometry.get_str("type").unwrap(), "Point");
        assert!((near.get_f64("$maxDistance").unwrap() - 5000.0).abs() < 1e-9);
    }

    #[test]
    fn box_filter_single_box_when_not_crossing_dateline() {
        // SW corner at [10, 10], NE corner at [20, 20] — does not cross
        let f = filter_timeseries(json!({"box": "[[10,10],[20,20]]"}));
        let or = f.get_array("$or").unwrap();
        assert_eq!(or.len(), 1, "non-crossing box should produce a single $or branch");
    }

    #[test]
    fn box_filter_splits_when_crossing_dateline() {
        // SW lon (170) > NE lon (-170) -> the box wraps the dateline
        let f = filter_timeseries(json!({"box": "[[170,10],[-170,20]]"}));
        let or = f.get_array("$or").unwrap();
        assert_eq!(or.len(), 2, "dateline-crossing box should split into two branches");
    }

    #[test]
    fn id_and_vertical_range_compose() {
        let f = filter_timeseries(json!({
            "id": "doc1",
            "verticalRange": "[0, 100]"
        }));
        assert_eq!(f.get_str("_id").unwrap(), "doc1");
        assert!(f.get_document("level").is_ok());
    }
}