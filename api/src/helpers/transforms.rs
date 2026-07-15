use super::schema;
use super::helpers;
use mongodb::bson::DateTime as BsonDateTime;

/// Apply the user's `startDate` / `endDate` / `data` parameters to a single
/// timeseries document. Returns `None` if the document was filtered out
/// entirely (e.g. `data=somefield` produced no matching columns, or the
/// date window contains none of the doc's timestamps).
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

    let mut data: Vec<String> = params.get("data")
        .and_then(|v| v.as_str())
        .map(|s| s.split(',').map(|s| s.to_string()).collect())
        .unwrap_or_default();

    // `data=except-data-values` alone is a nonsense query ("clear the
    // values of… nothing"): 2.x silently normalized it to the most
    // literal interpretation — as if `data=` weren't there at all — and
    // we match that. Only the lone-and-exact token is normalized;
    // `except-data-values` alongside variable names keeps its meaning
    // (filtered data_info, values cleared).
    if data.len() == 1 && data[0] == "except-data-values" {
        data = Vec::new();
    }

    if start_date.is_some() || end_date.is_some() {
        slice_timerange(start_date, end_date, ts, &mut doc);
        // An empty timeseries after windowing means no timestamps at
        // this grid point fall inside the requested range — there's
        // effectively no data here for this window, so drop the doc.
        // This fires regardless of `data=` mode: even a schema-only
        // (`except-data-values`) response is meaningless for a window
        // the doc doesn't cover.
        if doc.timeseries().map_or(true, |t| t.is_empty()) {
            return None;
        }
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

    let mut sliced = slice_data(&data, &working_info, doc)?;

    // Drop-on-empty: when the user asked for data and the surviving
    // data is empty — whether the outer Vec is empty (no matching
    // columns survived) OR the outer is non-empty but every inner is
    // empty (e.g. a time window that collapsed every column to zero
    // points) — the doc has nothing useful to convey, so drop it.
    // `iter().all(|inner| inner.is_empty())` returns true for both
    // shapes (vacuously true on an empty outer), so a single check
    // covers both.
    //
    // Exception: `except-data-values` in the data list is an explicit
    // "I want the (filtered) data_info but not the values" signal, so
    // an empty data array is what the user asked for, not a sign that
    // we should drop. Skip the drop check in that case.
    let user_wants_empty_data = data.iter().any(|s| s == "except-data-values");
    if !user_wants_empty_data && sliced.data().iter().all(|inner| inner.is_empty()) {
        return None;
    }

    Some(sliced)
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
    // Match on the Option directly so "no filter" and "filter present but
    // matches nothing" land at different ends of the axis:
    //   - `start_date = None`           → start at 0 (no lower bound).
    //   - `start_date = Some(sd)` and no timestamp is >= sd
    //     (the filter is past the end of the data) → start at ts.len()
    //     so the slice collapses to empty rather than degrading to
    //     "whole range," which a plain `.unwrap_or(0)` would do.
    // Symmetric reasoning for `end_date`.
    let start_index = match start_date {
        None => 0,
        Some(sd) => ts.iter().position(|&t| t >= sd).unwrap_or(ts.len()),
    };
    let end_index = match end_date {
        None => ts.len(),
        Some(ed) => ts
            .iter()
            .rposition(|&t| t < ed)
            .map(|i| i + 1)
            .unwrap_or(0),
    };

    // If the user passed mutually unsatisfiable dates (or startDate is
    // past everything *and* endDate is before everything), `start_index`
    // could exceed `end_index` — slicing `[a..b]` with `a > b` panics.
    // Clamp `end` up to `start` so the slice is always empty rather than
    // a panic.
    let end_index = end_index.max(start_index);

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
///   - if `data` also contains "except-data-values", clears the `data`
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

    // except-data-values clears data after column-filtering.
    if data.iter().any(|s| s == "except-data-values") {
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

    #[test]
    fn slice_timerange_startdate_past_everything_yields_empty_range() {
        // startDate is past the last timestamp — the resulting window
        // should be empty, not the whole range (which the old
        // `.unwrap_or(0)` shape gave by accident).
        let timeseries = ts(&[1, 2, 3]); // Jan, Feb, Mar 2020
        let mut doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0, 3.0]],
            &["temp"],
        );
        let start = helpers::string2bsondate("2021-01-01T00:00:00Z");
        slice_timerange(start, None, &timeseries, &mut doc);
        assert!(doc.data()[0].is_empty());
        assert!(doc.timeseries().unwrap().is_empty());
    }

    #[test]
    fn slice_timerange_enddate_before_everything_yields_empty_range() {
        // endDate is before the first timestamp — the resulting window
        // should be empty, not the whole range.
        let timeseries = ts(&[6, 7, 8]); // Jun, Jul, Aug 2020
        let mut doc = make_bsose(
            "doc1",
            vec![vec![6.0, 7.0, 8.0]],
            &["temp"],
        );
        let end = helpers::string2bsondate("2020-01-01T00:00:00Z");
        slice_timerange(None, end, &timeseries, &mut doc);
        assert!(doc.data()[0].is_empty());
        assert!(doc.timeseries().unwrap().is_empty());
    }

    #[test]
    fn slice_timerange_mutually_unsatisfiable_dates_collapse_to_empty() {
        // startDate past the data AND endDate before the data: the
        // raw indices would be start_index = ts.len(), end_index = 0,
        // which would panic on the slice. The `end_index.max(start_index)`
        // clamp keeps this safe (and empty).
        let timeseries = ts(&[6, 7, 8]);
        let mut doc = make_bsose(
            "doc1",
            vec![vec![6.0, 7.0, 8.0]],
            &["temp"],
        );
        let start = helpers::string2bsondate("2021-01-01T00:00:00Z");
        let end = helpers::string2bsondate("2019-01-01T00:00:00Z");
        slice_timerange(start, end, &timeseries, &mut doc);
        assert!(doc.data()[0].is_empty());
        assert!(doc.timeseries().unwrap().is_empty());
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
            &["temp".to_string(), "except-data-values".to_string()],
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
    fn transform_treats_lone_except_data_values_as_no_data_qsp() {
        // `data=except-data-values` with nothing else in the list is
        // nonsense ("clear the values of… nothing"); 2.x normalized it
        // to the absent-`data=` behaviour and so do we: doc survives,
        // data cleared, data_info scrubbed — byte-identical to the
        // no-data= response above.
        let timeseries = ts(&[1, 2]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );
        let params = json!({"data": "except-data-values"});
        let mut out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc)
            .expect("doc should survive normalization to the no-data= path");
        assert!(
            out.data_info().is_none(),
            "lone except-data-values should scrub data_info like absent data=, got {:?}",
            out.data_info()
        );
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

    // ---- drop-on-empty rule (data= set but data ends up empty) --------------

    #[test]
    fn transform_drops_doc_when_no_matching_columns() {
        // User asked for a variable that doesn't exist on this doc.
        // slice_data filters to an empty outer Vec → doc is dropped.
        let timeseries = ts(&[1, 2]);
        let doc = make_bsose("doc1", vec![vec![1.0, 2.0]], &["temp"]);
        let params = json!({"data": "nonexistent"});
        let out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc);
        assert!(out.is_none(), "doc with no matching columns should be dropped");
    }

    #[test]
    fn transform_drops_doc_when_time_window_collapses_to_empty() {
        // User asked for a time range past the dataset's last timestamp:
        // each column survives column-filtering but ends up with zero
        // time points. The doc has nothing useful to report — drop.
        let timeseries = ts(&[1, 2, 3]); // Jan, Feb, Mar 2020
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0, 3.0], vec![10.0, 20.0, 30.0]],
            &["temp", "salinity"],
        );
        let params = json!({
            "data":      "all",
            "startDate": "2021-01-01T00:00:00Z", // well past the timeseries
        });
        let out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc);
        assert!(
            out.is_none(),
            "doc whose time window collapsed to zero points should be dropped"
        );
    }

    #[test]
    fn transform_drops_doc_when_window_empty_even_without_data_qsp() {
        // Same collapsed window, but with no `data=` at all. The doc
        // used to survive into the slim-listing branch with an empty
        // timeseries; an empty window means there's effectively no data
        // at this grid point in the requested range, so drop it.
        let timeseries = ts(&[1, 2, 3]);
        let doc = make_bsose("doc1", vec![vec![1.0, 2.0, 3.0]], &["temp"]);
        let params = json!({"startDate": "2021-01-01T00:00:00Z"});
        let out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc);
        assert!(
            out.is_none(),
            "empty time window should drop the doc even without data="
        );
    }

    #[test]
    fn transform_drops_doc_when_window_empty_despite_except_data_values() {
        // `except-data-values` exempts a doc from drop-on-empty *data*
        // (the user asked for schema-only), but it does not exempt an
        // empty time *window* — a schema-only response for a range the
        // doc doesn't cover is meaningless.
        let timeseries = ts(&[1, 2, 3]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0, 3.0], vec![10.0, 20.0, 30.0]],
            &["temp", "salinity"],
        );
        let params = json!({
            "data":      "temp,except-data-values",
            "startDate": "2021-01-01T00:00:00Z", // past the timeseries
        });
        let out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc);
        assert!(
            out.is_none(),
            "empty time window should drop the doc despite except-data-values"
        );
    }

    #[test]
    fn transform_keeps_doc_with_except_data_values_despite_empty_data() {
        // `except-data-values` is the user explicitly asking for
        // "schema only, no values". slice_data clears the data after
        // column-filtering; the drop-on-empty rule should skip this
        // case rather than dropping the doc — the empty data was
        // requested.
        let timeseries = ts(&[1, 2]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            &["temp", "salinity"],
        );
        let params = json!({"data": "temp,except-data-values"});
        let mut out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc)
            .expect("except-data-values should not trigger drop-on-empty");
        // Data was deliberately cleared.
        assert!(out.data().is_empty());
        // But the filtered data_info still rides along — that's the
        // whole point of except-data-values.
        let info = out.data_info().expect("data_info still present");
        assert_eq!(info.0, vec!["temp".to_string()]);
    }

    #[test]
    fn transform_keeps_doc_when_at_least_some_data_remains() {
        // User asked for a real column with a non-degenerate time
        // window. The doc should survive.
        let timeseries = ts(&[1, 2, 3, 4]);
        let doc = make_bsose(
            "doc1",
            vec![vec![1.0, 2.0, 3.0, 4.0], vec![10.0, 20.0, 30.0, 40.0]],
            &["temp", "salinity"],
        );
        let params = json!({
            "data":      "temp",
            "startDate": "2020-02-01T00:00:00Z",
            "endDate":   "2020-04-01T00:00:00Z",
        });
        let mut out = transform_timeseries(&params, &timeseries, &empty_data_info(), doc)
            .expect("non-empty doc should survive");
        assert_eq!(*out.data(), vec![vec![2.0, 3.0]]);
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
