use super::schema;
use super::helpers;
use mongodb::bson::DateTime as BsonDateTime;

/// Apply the user's `startDate` / `endDate` / `data` parameters to a single
/// timeseries document. Returns `None` if the document was filtered out
/// entirely (e.g. `data=somefield` produced no matching columns).
///
/// Response-shape rules enforced here:
///
///   - `timeseries` appears on the returned doc *iff* the user supplied
///     `startDate` or `endDate`. Otherwise the field stays `None`, and
///     clients fall back to the dataset-wide timeseries on the meta
///     endpoint. (Mechanically: `slice_timerange` populates the field
///     only when invoked, which only happens for date-bounded queries.)
///
///   - `data_info` appears on the returned doc *iff* the user supplied
///     `data=`. With `data=` set we materialise the working `data_info`
///     (see precedence rule below) onto the doc so `slice_data` can
///     filter, and the resulting filtered `data_info` rides along in
///     the response. Without `data=`, we scrub `data_info` to `None`
///     (even if the source doc carried one — the BSOSE case) so the
///     response stays slim and clients defer to the meta endpoint.
///
/// Precedence rule for the working `data_info` when `data=` is set:
///
///   - If the data doc carries its own non-empty `data_info` (the BSOSE
///     case today), it wins; the cached meta-level default is ignored.
///   - If the data doc has no `data_info` (the OI SST case — single
///     variable, info kept on the meta doc), the cached default is
///     stamped onto the doc before `slice_data` runs.
///
/// The cache parameter is `&DataInfo` (not `Option`): an empty tuple
/// is its own "no default" sentinel, so a dataset whose meta doc has
/// no `data_info` field just lands an empty tuple in the cache.
pub fn transform_timeseries<T: schema::IsTimeseries>(
    params: &serde_json::Value,
    ts: &[BsonDateTime],
    cached_data_info: &schema::DataInfo,
    mut doc: T,
) -> Option<T> {
    let start_date = params.get("startDate")
        .and_then(|v| v.as_str())
        .and_then(helpers::string2bsondate);

    let end_date = params.get("endDate")
        .and_then(|v| v.as_str())
        .and_then(helpers::string2bsondate);

    let data: Vec<String> = params.get("data")
        .and_then(|v| v.as_str())
        .map(|s| s.split(',').map(|s| s.to_string()).collect())
        .unwrap_or_default();

    if start_date.is_some() || end_date.is_some() {
        slice_timerange(start_date, end_date, ts, &mut doc);
    }

    if data.is_empty() {
        // No `data=` qsp. Clear `data` (the historical behaviour for
        // this code path) and scrub `data_info` so the response omits
        // it. Clients that need variable info read the meta endpoint.
        doc.set_data(Vec::new());
        doc.set_data_info(None);
        return Some(doc);
    }

    // `data=` qsp set. Materialise the working `data_info`: doc-level
    // wins over cache, and an empty (or absent) doc-level value falls
    // back to the meta-level cached default.
    let working_info: schema::DataInfo = doc
        .data_info()
        .filter(|di| !di.0.is_empty())
        .unwrap_or_else(|| cached_data_info.clone());
    doc.set_data_info(Some(working_info.clone()));

    slice_data(&data, &working_info, doc)
}

/// Slice the document's data columns and timeseries field to the time window
/// `[start_date, end_date)`. The window is computed against `ts` (the
/// timeseries metadata cached at startup); each variable's per-timestep
/// values are sliced in lockstep. Mutates `doc` in place.
pub fn slice_timerange<T: schema::IsTimeseries>(
    start_date: Option<BsonDateTime>,
    end_date: Option<BsonDateTime>,
    ts: &[BsonDateTime],
    doc: &mut T,
) {
    let start_index = start_date
        .and_then(|sd| ts.iter().position(|&t| t >= sd))
        .unwrap_or(0);

    let end_index = end_date
        .and_then(|ed| ts.iter().rposition(|&t| t < ed).map(|i| i + 1))
        .unwrap_or(ts.len());

    let time_window: Vec<String> = ts[start_index..end_index]
        .iter()
        .map(|t| helpers::bsondate2string(t))
        .collect();

    let data = doc.data();
    *data = data
        .iter()
        .map(|inner| inner[start_index..end_index].to_vec())
        .collect();

    match doc.timeseries() {
        Some(timeseries) => *timeseries = time_window,
        None => doc.set_timeseries(time_window),
    }
}

/// Apply the `data=` query parameter to a single document. Called only
/// when `data=` was actually supplied — the empty-`data` case is handled
/// by `transform_timeseries` before getting here.
///
///   - `data` contains "all": leaves data/data_info untouched.
///   - otherwise: filters the data columns down to the named variables
///     and writes the filtered `data_info` back onto the doc; returns
///     `None` if no requested variables match (caller drops the row).
///   - if `data` also contains "except_data_values", clears the `data`
///     field after column-filtering, but keeps the row (the filtered
///     `data_info` is what the caller wanted to see).
///
/// `data_info` is passed in as an explicit `&DataInfo` rather than read
/// off the doc — the caller (`transform_timeseries`) is responsible
/// for deciding which `data_info` is in effect (doc-level vs cached
/// meta-level default) and putting it on the doc before calling here.
pub fn slice_data<T: schema::IsTimeseries>(
    data: &[String],
    data_info: &schema::DataInfo,
    mut doc: T,
) -> Option<T> {
    if data.iter().any(|s| s == "all") {
        return Some(doc);
    }

    // Specific fields requested — filter columns.
    let indexes: Vec<usize> = data
        .iter()
        .filter_map(|item| data_info.0.iter().position(|x| x == item))
        .collect();

    let filtered_data: Vec<Vec<f64>> = indexes
        .iter()
        .filter_map(|&i| doc.data().get(i).cloned())
        .collect();
    doc.set_data(filtered_data);

    let filtered_data_info: schema::DataInfo = (
        indexes.iter().filter_map(|&i| data_info.0.get(i).cloned()).collect(),
        data_info.1.clone(),
        indexes.iter().filter_map(|&i| data_info.2.get(i).cloned()).collect(),
    );
    doc.set_data_info(Some(filtered_data_info));

    // No matching columns -> caller drops the row.
    if doc.data().is_empty() {
        return None;
    }

    // except_data_values clears data after column-filtering.
    if data.iter().any(|s| s == "except_data_values") {
        doc.set_data(Vec::new());
    }

    Some(doc)
}

/// Project a single timeseries document down to its summary stub.
pub fn timeseries_stub<T: schema::IsTimeseries>(result: &T) -> schema::TimeseriesStub {
    schema::TimeseriesStub {
        _id: result._id(),
        longitude: result.longitude(),
        latitude: result.latitude(),
        level: result.level(),
        metadata: result.metadata(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::schema::{BsoseSchema, DataInfo, GeoJSONPoint, IsTimeseries};
    use serde_json::json;

    // Helper: construct a DataInfo from variable names. Used both inside
    // `make_bsose` and at slice_data call sites that need to pass an
    // explicit DataInfo.
    fn make_data_info(var_names: &[&str]) -> DataInfo {
        let names: Vec<String> = var_names.iter().map(|s| s.to_string()).collect();
        let units = vec!["units".to_string(), "long_name".to_string()];
        let per_var_info: Vec<Vec<String>> = names
            .iter()
            .map(|n| vec!["u".to_string(), n.clone()])
            .collect();
        (names, units, per_var_info)
    }

    /// Empty DataInfo sentinel — used as the "no meta-level default" cache
    /// value in tests that exercise the BSOSE-style doc-level precedence.
    fn empty_data_info() -> DataInfo {
        (vec![], vec![], vec![])
    }

    // Helper: construct a BsoseSchema directly. Field-level visibility is
    // `pub(crate)` so this struct literal works from any test in the crate
    // and gets compile-time field checking — if a schema field is renamed,
    // the test stops compiling.
    fn make_bsose(id: &str, data: Vec<Vec<f64>>, var_names: &[&str]) -> BsoseSchema {
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
            sea_binary_mask_at_t_locaiton: true,
            cell_z_size: 1.0,
            reference_density_profile: 1.0,
            data,
            timeseries: None,
            data_info: Some(make_data_info(var_names)),
        }
    }

    fn ts(months: &[u32]) -> Vec<BsonDateTime> {
        // BSON date for the 1st of each named month in 2020.
        months
            .iter()
            .map(|&m| {
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
        let mut doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0, 3.0, 4.0], vec![10.0, 20.0, 30.0, 40.0]],
            &["temp", "salinity"],
        );

        let start = helpers::string2bsondate("2020-02-01T00:00:00Z");
        let end = helpers::string2bsondate("2020-04-01T00:00:00Z"); // exclusive

        slice_timerange(start, end, &timeseries, &mut doc);
        // Expect indexes 1..3 -> Feb, Mar
        assert_eq!(*doc.data(), vec![vec![2.0, 3.0], vec![20.0, 30.0]]);
        let ts_field = doc.timeseries().unwrap();
        assert_eq!(ts_field.len(), 2);
        assert!(ts_field[0].starts_with("2020-02-01"));
        assert!(ts_field[1].starts_with("2020-03-01"));
    }

    #[test]
    fn slice_timerange_no_dates_keeps_full_range() {
        let timeseries = ts(&[1, 2, 3]);
        let mut doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0, 3.0]],
            &["temp"],
        );

        slice_timerange(None, None, &timeseries, &mut doc);
        assert_eq!(*doc.data(), vec![vec![1.0, 2.0, 3.0]]);
    }

    // ---- slice_data ----------------------------------------------------------
    //
    // `slice_data` is only called from `transform_timeseries` in the
    // `data=` qsp case, so these tests always supply a non-empty `data`
    // vector. The empty-data case is exercised through the
    // `transform_timeseries` tests below.

    #[test]
    fn slice_data_all_keeps_everything() {
        let info = make_data_info(&["temp", "salinity"]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );
        let mut out = slice_data(&["all".to_string()], &info, doc).unwrap();
        assert_eq!(*out.data(), vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[test]
    fn slice_data_specific_field_filters_columns() {
        let info = make_data_info(&["temp", "salinity"]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );
        let mut out = slice_data(&["salinity".to_string()], &info, doc).unwrap();
        assert_eq!(*out.data(), vec![vec![3.0, 4.0]]);
    }

    #[test]
    fn slice_data_unknown_field_drops_result() {
        let info = make_data_info(&["temp"]);
        let doc = make_bsose("doc1", vec![vec![1.0, 2.0]], &["temp"]);
        let out = slice_data(&["nonexistent".to_string()], &info, doc);
        assert!(out.is_none(), "no matching columns -> row is dropped");
    }

    #[test]
    fn slice_data_except_data_values_clears_after_filtering() {
        let info = make_data_info(&["temp"]);
        let doc = make_bsose("doc1", vec![vec![1.0, 2.0]], &["temp"]);
        let mut out = slice_data(
            &["temp".to_string(), "except_data_values".to_string()],
            &info,
            doc,
        )
        .unwrap();
        assert!(out.data().is_empty());
    }

    // ---- transform_timeseries (full pipeline) --------------------------------

    #[test]
    fn transform_timeseries_combines_time_and_data_slices() {
        let timeseries = ts(&[1, 2, 3, 4]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0, 3.0, 4.0], vec![10.0, 20.0, 30.0, 40.0]],
            &["temp", "salinity"],
        );

        let params = json!({
            "startDate": "2020-02-01T00:00:00Z",
            "endDate":   "2020-04-01T00:00:00Z",
            "data":      "salinity",
        });

        // Empty cached_data_info — this doc already carries its own
        // data_info, so the precedence rule means the cache is never
        // consulted regardless.
        let mut out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc).unwrap();
        assert_eq!(*out.data(), vec![vec![20.0, 30.0]]);
    }

    // ---- transform_timeseries: data_info precedence -------------------------

    #[test]
    fn transform_uses_cached_data_info_when_doc_has_none() {
        // OI SST-style: the data doc carries no data_info; the meta-level
        // cache supplies the variable names so slice_data can resolve
        // `data=sst`.
        let timeseries = ts(&[1, 2]);
        let mut doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0]],
            &["sst"], // overridden to None below to simulate an OI SST doc
        );
        // Clear the doc's data_info so the cache fallback kicks in.
        doc.data_info = None;

        let cache: schema::DataInfo = (
            vec!["sst".to_string()],
            vec!["units".to_string(), "long_name".to_string()],
            vec![vec!["degC".to_string(), "SST".to_string()]],
        );

        let params = json!({"data": "sst"});
        let mut out =
            transform_timeseries(&params, &timeseries, &cache, doc).expect("should resolve");
        // sst column survives.
        assert_eq!(*out.data(), vec![vec![1.0, 2.0]]);
        // data_info has been stamped from the cache, then column-filtered
        // by slice_data — should still list sst.
        let info = out.data_info().expect("data_info present when data= is set");
        assert_eq!(info.0, vec!["sst".to_string()]);
    }

    #[test]
    fn transform_doc_data_info_wins_over_cache() {
        // BSOSE-style: the doc has data_info and the cache also has
        // something (hypothetically). The doc's value should take
        // precedence — the cache should not overwrite it.
        let timeseries = ts(&[1, 2]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );

        // Cache says "sst" — but doc has temp/salinity. Doc wins; the
        // request for `data=temp` should resolve against the doc, not
        // get clobbered by the cache.
        let cache: schema::DataInfo = (
            vec!["sst".to_string()],
            vec!["units".to_string()],
            vec![vec!["degC".to_string()]],
        );

        let params = json!({"data": "temp"});
        let mut out =
            transform_timeseries(&params, &timeseries, &cache, doc).expect("should resolve");
        assert_eq!(*out.data(), vec![vec![1.0, 2.0]]);
        let info = out.data_info().expect("data_info present when data= is set");
        assert_eq!(info.0, vec!["temp".to_string()]);
    }

    // ---- response-shape rule: data_info & timeseries omitted unless munged --

    #[test]
    fn transform_omits_data_info_when_no_data_qsp() {
        // No `data=` in params: the response doc should carry no
        // data_info — clients fall back to the meta endpoint.
        let timeseries = ts(&[1, 2]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );
        let params = json!({}); // no data=, no dates
        let mut out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc)
            .expect("doc should survive");
        assert!(
            out.data_info().is_none(),
            "data_info should be absent when data= qsp is unset, got {:?}",
            out.data_info()
        );
        // data field is cleared in the no-data= branch (historical behaviour).
        assert!(out.data().is_empty());
    }

    #[test]
    fn transform_omits_timeseries_when_no_date_qsp() {
        // No `startDate` / `endDate`: the response doc should carry no
        // timeseries field — clients fall back to the meta endpoint's
        // dataset-wide timeseries.
        let timeseries = ts(&[1, 2]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );
        let params = json!({"data": "all"});
        let mut out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc)
            .expect("doc should survive");
        assert!(
            out.timeseries().is_none(),
            "timeseries should be absent when neither startDate nor endDate is set"
        );
    }

    #[test]
    fn transform_includes_timeseries_when_start_date_set() {
        // startDate alone is enough to trigger timeseries on the response.
        let timeseries = ts(&[1, 2, 3]);
        let doc = make_bsose("doc1", vec![vec![1.0, 2.0, 3.0]], &["temp"]);
        let params = json!({"startDate": "2020-02-01T00:00:00Z"});
        let mut out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc)
            .expect("doc should survive");
        let ts_field = out.timeseries().expect("timeseries present when startDate set");
        assert_eq!(ts_field.len(), 2);
        assert!(ts_field[0].starts_with("2020-02-01"));
        // Without `data=`, data_info still absent.
        assert!(out.data_info().is_none());
    }

    #[test]
    fn transform_includes_data_info_when_data_qsp_set() {
        // `data=` alone is enough to trigger data_info on the response,
        // even without date params.
        let timeseries = ts(&[1, 2]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );
        let params = json!({"data": "temp"});
        let mut out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc)
            .expect("doc should survive");
        let info = out.data_info().expect("data_info present when data= is set");
        assert_eq!(info.0, vec!["temp".to_string()]);
        // Without dates, timeseries still absent.
        assert!(out.timeseries().is_none());
    }

    // ---- timeseries_stub -----------------------------------------------------

    #[test]
    fn timeseries_stub_projects_summary_fields() {
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0]],
            &["temp"],
        );
        let stub = timeseries_stub(&doc);
        assert_eq!(stub._id, "doc1");
        assert!((stub.longitude - 10.0).abs() < 1e-9);
        assert!((stub.latitude - 20.0).abs() < 1e-9);
        assert!((stub.level - 5.0).abs() < 1e-9);
    }
}
