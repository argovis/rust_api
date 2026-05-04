use super::schema;
use super::helpers;
use mongodb::bson::DateTime as BsonDateTime;

pub fn transform_timeseries<T: schema::IsTimeseries + Clone>(params: serde_json::Value, ts: Vec<BsonDateTime>, results: Vec<T>) -> Vec<T> {
    
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

    // apply appropriate transforms ////////////////////////////////////
    let mut r = results.clone();

    if start_date.is_some() || end_date.is_some() {
        r = slice_timerange(start_date, end_date, ts, r);
    }
    r = slice_data(data, r);

    return r;
}

pub fn slice_timerange<T: schema::IsTimeseries>(start_date: Option<BsonDateTime>, end_date: Option<BsonDateTime>, ts: Vec<BsonDateTime>, mut results: Vec<T>) -> Vec<T> {

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
pub fn slice_data<T: schema::IsTimeseries>(data: Vec<String>, mut results: Vec<T>) -> Vec<T> {

    if data.is_empty() {
        for result in &mut results {
            result.set_data(Vec::new());
        }
    } else if data.contains(&"all".to_string()) {
        return results;
    } else {
        for result in &mut results {
            let data_info = result.data_info();

            let indexes: Vec<usize> = data.iter()
                .filter_map(|item| data_info.0.iter().position(|x| x == item))
                .collect();

            // only keep the requested data
            let filtered_data: Vec<Vec<f64>> = indexes.iter()
                .filter_map(|&i| result.data().get(i).cloned())
                .collect();
            result.set_data(filtered_data);

            // create a custom data_info to go with this reduced data, and add it to the result object
            let filtered_data_info: (Vec<String>, Vec<String>, Vec<Vec<String>>) = (
                indexes.iter().filter_map(|&i| data_info.0.get(i).cloned()).collect(),
                data_info.1.clone(),
                indexes.iter().filter_map(|&i| data_info.2.get(i).cloned()).collect(),
            );
            result.set_data_info(filtered_data_info);
        }

        // if all the data is empty, remove the result
        let mut i = 0;
        while i != results.len() {
            if results[i].data().is_empty() {
                results.remove(i);
            } else {
                i += 1;
            }
        }

        // if we set except_data_values, drop the data from every result
        if data.contains(&"except_data_values".to_string()) {
            for result in &mut results {
                result.set_data(Vec::new());
            }
        }
    }

    results
}

pub fn timeseries_stub<T: schema::IsTimeseries>(results: Vec<T>) -> Vec<schema::TimeseriesStub> {
    let r = results.iter().map(|result| {
        schema::TimeseriesStub {
            _id: result._id(),
            longitude: result.longitude(),
            latitude: result.latitude(),
            level: result.level(),
            metadata: result.metadata(),
        }
    }).collect();

    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::schema::{BsoseSchema, GeoJSONPoint};
    use serde_json::json;

    // Helper: construct a BsoseSchema directly. Field-level visibility is
    // `pub(crate)` so this struct literal works from any test in the crate
    // and gets compile-time field checking — if a schema field is renamed,
    // the test stops compiling.
    fn make_bsose(id: &str, data: Vec<Vec<f64>>, var_names: &[&str]) -> BsoseSchema {
        let names: Vec<String> = var_names.iter().map(|s| s.to_string()).collect();
        let units = vec!["units".to_string(), "long_name".to_string()];
        let per_var_info: Vec<Vec<String>> = names
            .iter()
            .map(|n| vec!["u".to_string(), n.clone()])
            .collect();

        BsoseSchema {
            _id: id.to_string(),
            metadata: vec!["meta1".to_string()],
            basin: 1.0,
            geolocation: GeoJSONPoint {
                location_type: "Point".to_string(),
                coordinates: [10.0, 20.0],
            },
            level: 5.0,
            cell_vertical_fraction: 1.0,
            sea_binary_mask_at_t_location: true,
            ctrl_vector_3d_mask: true,
            cell_z_size: 1.0,
            reference_density_profile: 1.0,
            data,
            timeseries: None,
            data_info: (names, units, per_var_info),
        }
    }

    fn ts(months: &[u32]) -> Vec<BsonDateTime> {
        // Build a BSON date for each (1st of month, year 2020)
        months
            .iter()
            .map(|&m| {
                // milliseconds since epoch for 2020-{m:02}-01T00:00:00Z, computed naively
                let s = format!("2020-{:02}-01T00:00:00Z", m);
                let dt = chrono::DateTime::parse_from_rfc3339(&s).unwrap();
                BsonDateTime::from_millis(dt.timestamp_millis())
            })
            .collect()
    }

    // ---- slice_timerange -----------------------------------------------------

    #[test]
    fn slice_timerange_inclusive_start_exclusive_end() {
        let timeseries = ts(&[1, 2, 3, 4]); // Jan, Feb, Mar, Apr
        // 2 variables, 4 timestamps each
        let r = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0, 3.0, 4.0], vec![10.0, 20.0, 30.0, 40.0]],
            &["temp", "salinity"],
        );

        let start = helpers::string2bsondate("2020-02-01T00:00:00Z");
        let end = helpers::string2bsondate("2020-04-01T00:00:00Z"); // exclusive

        let mut out = slice_timerange(start, end, timeseries, vec![r]);
        // Expect indexes 1..3 -> Feb, Mar
        assert_eq!(out.len(), 1);
        assert_eq!(*out[0].data(), vec![vec![2.0, 3.0], vec![20.0, 30.0]]);
        let ts_field = out[0].timeseries().unwrap();
        assert_eq!(ts_field.len(), 2);
        assert!(ts_field[0].starts_with("2020-02-01"));
        assert!(ts_field[1].starts_with("2020-03-01"));
    }

    #[test]
    fn slice_timerange_no_dates_keeps_full_range() {
        let timeseries = ts(&[1, 2, 3]);
        let r = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0, 3.0]],
            &["temp"],
        );

        let mut out = slice_timerange(None, None, timeseries, vec![r]);
        assert_eq!(*out[0].data(), vec![vec![1.0, 2.0, 3.0]]);
    }

    // ---- slice_data ----------------------------------------------------------

    #[test]
    fn slice_data_empty_request_drops_data() {
        let r = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );
        let mut out = slice_data(vec![], vec![r]);
        // when no data params, slice_data drops the data — but the result row
        // is preserved (the empty-data removal only applies in the "specific
        // fields" branch).
        assert_eq!(out.len(), 1);
        assert!(out[0].data().is_empty());
    }

    #[test]
    fn slice_data_all_keeps_everything() {
        let r = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );
        let mut out = slice_data(vec!["all".to_string()], vec![r]);
        assert_eq!(*out[0].data(), vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[test]
    fn slice_data_specific_field_filters_columns() {
        let r = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );
        let mut out = slice_data(vec!["salinity".to_string()], vec![r]);
        assert_eq!(out.len(), 1);
        assert_eq!(*out[0].data(), vec![vec![3.0, 4.0]]);
    }

    #[test]
    fn slice_data_unknown_field_drops_result() {
        let r = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0]],
            &["temp"],
        );
        let out = slice_data(vec!["nonexistent".to_string()], vec![r]);
        // Filtered data is empty -> the result row is removed entirely.
        assert!(out.is_empty());
    }

    #[test]
    fn slice_data_except_data_values_clears_after_filtering() {
        let r = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0]],
            &["temp"],
        );
        let mut out = slice_data(
            vec!["temp".to_string(), "except_data_values".to_string()],
            vec![r],
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].data().is_empty());
    }

    // ---- transform_timeseries (full pipeline) --------------------------------

    #[test]
    fn transform_timeseries_combines_time_and_data_slices() {
        let timeseries = ts(&[1, 2, 3, 4]);
        let r = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0, 3.0, 4.0], vec![10.0, 20.0, 30.0, 40.0]],
            &["temp", "salinity"],
        );

        let params = json!({
            "startDate": "2020-02-01T00:00:00Z",
            "endDate":   "2020-04-01T00:00:00Z",
            "data":      "salinity",
        });

        let mut out = transform_timeseries(params, timeseries, vec![r]);
        assert_eq!(out.len(), 1);
        assert_eq!(*out[0].data(), vec![vec![20.0, 30.0]]);
    }

    // ---- timeseries_stub -----------------------------------------------------

    #[test]
    fn timeseries_stub_projects_summary_fields() {
        let r = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0]],
            &["temp"],
        );
        let stubs = timeseries_stub(vec![r]);
        assert_eq!(stubs.len(), 1);
        assert_eq!(stubs[0]._id, "doc1");
        assert!((stubs[0].longitude - 10.0).abs() < 1e-9);
        assert!((stubs[0].latitude - 20.0).abs() < 1e-9);
        assert!((stubs[0].level - 5.0).abs() < 1e-9);
    }
}

