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

// ---- profile family ---------------------------------------------------------

/// Apply the user's `data=` parameter to a single profile document —
/// the profile-family sibling of `transform_timeseries`, sitting in the
/// same seam: the handler calls it on each doc coming off the Mongo
/// cursor, and `None` drops the doc from the stream.
///
/// The seam is deliberately narrower than the timeseries one
/// (ARGO-DESIGN.md):
///
///   - `startDate` / `endDate` never reach this function: a profile's
///     time is a scalar Mongo field, so date windowing is a query
///     predicate composed upstream, not a transform. Date params in
///     `params` are simply irrelevant here.
///   - `verticalRange` row slicing and integer-token QC filtering are
///     phase 2. Integer tokens in `data=` are accepted by validation
///     and **ignored** here for now — they select the same columns as
///     the equivalent request without them.
///   - `data_info` is per-doc only; there is no meta-level cache, so no
///     precedence rule and no cache parameter.
///   - `~variable` tokens (2.x negation) are profile-level presence
///     filters: a doc whose `data_info` lists a negated variable is
///     dropped. They select profiles, never columns — and an
///     only-negations request degrades to the slim listing once the
///     selection is done.
///
/// Response-shape rules match the grids exactly:
///
///   - no `data=` (or the lone `except-data-values`, normalized
///     2.x-style to absent): clear `data`, scrub `data_info`, keep the
///     doc — the slim listing.
///   - `data=` set: filter columns against the doc's own `data_info`;
///     the filtered `data_info` rides along in the response; drop the
///     doc when nothing survives, with `except-data-values` as the one
///     exemption (an empty `data` is then what the user asked for).
pub fn transform_profile<T: schema::IsProfile>(
    params: &serde_json::Value,
    mut doc: T,
) -> Option<T> {
    let mut data: Vec<String> = params.get("data")
        .and_then(|v| v.as_str())
        .map(|s| s.split(',').map(|s| s.to_string()).collect())
        .unwrap_or_default();

    // `~variable` tokens are profile-level presence NEGATIONS: keep only
    // docs whose data_info does NOT list the variable (e.g.
    // `data=salinity,~doxy` → docs with salinity and without doxy).
    // They're extracted first because they apply to every response shape
    // — including `all` and the only-negations slim listing below — and
    // contribute nothing to column selection. A doc with no data_info
    // lists nothing, so it trivially lacks every negated variable and
    // passes.
    let negations: Vec<String> = data
        .iter()
        .filter_map(|t| t.strip_prefix('~').map(|n| n.to_string()))
        .collect();
    data.retain(|t| !t.starts_with('~'));

    if !negations.is_empty() {
        if let Some(di) = doc.data_info() {
            if negations
                .iter()
                .any(|n| di.0.iter().any(|name| name == n))
            {
                return None; // doc carries a negated variable — drop.
            }
        }
    }

    // Same 2.x normalization as the grids: a lone `except-data-values`
    // is "clear the values of… nothing" — treat as absent `data=`. With
    // negations extracted, an only-negations request (`data=~doxy`)
    // lands here too: the negations have done their selecting, and with
    // no positive tokens the response is the slim listing.
    if data.len() == 1 && data[0] == "except-data-values" {
        data = Vec::new();
    }

    if data.is_empty() {
        doc.set_data(Vec::new());
        doc.set_data_info(None);
        return Some(doc);
    }

    // Integer tokens are QC filters — phase 2. Strip them here so the
    // column-selection pass below sees only universal tokens and
    // variable names; when QC filtering lands it becomes a row-selection
    // pass between slicing and the drop-on-empty check.
    let named: Vec<String> = data
        .iter()
        .filter(|t| t.parse::<i64>().is_err())
        .cloned()
        .collect();

    // Integer tokens with no variable to filter (`data=1`, or
    // `data=1,except-data-values`) are nonsense and 400 at validation
    // (`validate_data_param`, matching 2.x), so this branch is
    // unreachable through the route. Defensively, degrade to the slim
    // listing rather than panic or serve unfiltered data.
    if named.is_empty() || (named.len() == 1 && named[0] == "except-data-values") {
        doc.set_data(Vec::new());
        doc.set_data_info(None);
        return Some(doc);
    }

    let working_info = doc.data_info().filter(|di| !di.0.is_empty());

    let mut sliced = slice_profile_data(&named, working_info.as_ref(), doc)?;

    // Drop-on-empty, same rule and same exemption as the grids.
    let user_wants_empty_data = named.iter().any(|s| s == "except-data-values");
    if !user_wants_empty_data && sliced.data().iter().all(|inner| inner.is_empty()) {
        return None;
    }

    Some(sliced)
}

/// The profile family's vertical coordinate variable. It **always rides
/// along** with any `data=` selection (2.x behaviour): a variable's
/// values are meaningless without the pressures they were measured at,
/// so `data=temperature` returns pressure, its QC companion, then the
/// requested columns. A future non-Argo profile dataset with a different
/// coordinate name would promote this to per-dataset config.
pub const VERTICAL_COORDINATE: &str = "pressure";

/// Apply the (integer-stripped) `data=` token list to a single profile
/// document. The profile sibling of `slice_data`, with three deltas:
///
///   - the matrix and `data_info` cells are nullable (profile types);
///   - **QC companions ride along**: requesting a variable also keeps
///     its `<var>_argoqc` column when the doc has one (2.x behaviour —
///     QC flags are how users judge the values, so they come along
///     without being asked for by name). The companion lands directly
///     after its variable; asking for a companion explicitly doesn't
///     duplicate it.
///   - **pressure rides along unconditionally** (see
///     `VERTICAL_COORDINATE`): the coordinate columns are pinned first,
///     then the requested columns in token order.
///
/// Survival is decided by the *requested* variables alone: a request
/// naming only variables this doc doesn't carry drops the doc, even
/// though its pressure column would have resolved — pressure is a
/// bonus, never a lifeline.
///
///   - `named` contains "all": doc untouched (its own `data_info` is
///     the response's `data_info`, per the data=-set rule).
///   - otherwise `info` must be `Some` — a doc with no `data_info`
///     can't resolve any named variable, so the row is dropped.
///   - no requested token resolves → `None` (caller drops the row).
///   - `except-data-values` clears `data` after column-filtering but
///     keeps the row (the filtered `data_info` is the point).
pub fn slice_profile_data<T: schema::IsProfile>(
    named: &[String],
    info: Option<&schema::ProfileDataInfo>,
    mut doc: T,
) -> Option<T> {
    if named.iter().any(|s| s == "all") {
        return Some(doc);
    }

    let info = info?;

    let push_unique = |indexes: &mut Vec<usize>, i: usize| {
        if !indexes.contains(&i) {
            indexes.push(i);
        }
    };

    // Resolve a variable plus its `_argoqc` companion (companion lands
    // directly behind its variable).
    let push_with_companion = |indexes: &mut Vec<usize>, name: &str| {
        if let Some(i) = info.0.iter().position(|x| x == name) {
            push_unique(indexes, i);
            let companion = format!("{}_argoqc", name);
            if let Some(j) = info.0.iter().position(|x| *x == companion) {
                push_unique(indexes, j);
            }
        }
    };

    // The *requested* tokens, in token order — these decide survival.
    let mut requested: Vec<usize> = Vec::new();
    for token in named {
        if token == "except-data-values" {
            continue;
        }
        push_with_companion(&mut requested, token);
    }

    // No requested token resolved -> caller drops the row (pressure
    // riding along must not rescue a doc that has nothing the user
    // asked for).
    if requested.is_empty() {
        return None;
    }

    // Final column order: the vertical coordinate (+ companion) pinned
    // first, then the requested columns. Dedup means an explicit
    // `data=pressure,...` doesn't double it.
    let mut indexes: Vec<usize> = Vec::new();
    push_with_companion(&mut indexes, VERTICAL_COORDINATE);
    for i in requested {
        push_unique(&mut indexes, i);
    }

    let filtered_data: Vec<Vec<Option<f64>>> = indexes
        .iter()
        .filter_map(|&i| doc.data().get(i).cloned())
        .collect();
    doc.set_data(filtered_data);

    let filtered_info: schema::ProfileDataInfo = (
        indexes.iter().filter_map(|&i| info.0.get(i).cloned()).collect(),
        info.1.clone(),
        indexes.iter().filter_map(|&i| info.2.get(i).cloned()).collect(),
    );
    doc.set_data_info(Some(filtered_info));

    // except-data-values clears data after column-filtering.
    if named.iter().any(|s| s == "except-data-values") {
        doc.set_data(Vec::new());
    }

    Some(doc)
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

    // ---- transform_profile ----------------------------------------------------

    use crate::helpers::schema::{ArgoSchema, IsProfile, ProfileDataInfo};
    use mongodb::bson::DateTime as TestBsonDateTime;

    /// ProfileDataInfo over the given names: `<var>_argoqc` names get the
    /// real-world `[null, null]` info row, everything else `[Some, Some]`.
    fn make_profile_info(names: &[&str]) -> ProfileDataInfo {
        let n: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let cols = vec!["units".to_string(), "data_keys_mode".to_string()];
        let rows: Vec<Vec<Option<String>>> = names
            .iter()
            .map(|name| {
                if name.ends_with("_argoqc") {
                    vec![None, None]
                } else {
                    vec![Some("u".to_string()), Some("D".to_string())]
                }
            })
            .collect();
        (n, cols, rows)
    }

    /// Construct an ArgoSchema directly (same compile-time field checking
    /// rationale as `make_bsose`).
    fn make_argo(id: &str, data: Vec<Vec<Option<f64>>>, names: &[&str]) -> ArgoSchema {
        ArgoSchema {
            _id: id.to_string(),
            metadata: vec!["meta_m0".to_string()],
            geolocation: GeoJSONPoint {
                location_type: "Point".to_string(),
                coordinates: [-26.257, 3.427],
            },
            timestamp: TestBsonDateTime::from_millis(933879822002),
            cycle_number: 68,
            basin: 1.0,
            data,
            data_info: Some(make_profile_info(names)),
            date_updated_argovis: None,
            source: Vec::new(),
            geolocation_argoqc: Some(1),
            timestamp_argoqc: Some(1),
            profile_direction: Some("A".to_string()),
            vertical_sampling_scheme: None,
        }
    }

    /// Standard 4-column test doc: pressure / pressure_argoqc /
    /// temperature / temperature_argoqc, three levels, one null
    /// temperature cell (real Argo shape).
    fn argo4() -> ArgoSchema {
        make_argo(
            "p1",
            vec![
                vec![Some(11.9), Some(17.0), Some(22.1)],
                vec![Some(2.0), Some(2.0), Some(2.0)],
                vec![Some(28.021), None, Some(27.969)],
                vec![Some(2.0), Some(2.0), Some(2.0)],
            ],
            &["pressure", "pressure_argoqc", "temperature", "temperature_argoqc"],
        )
    }

    #[test]
    fn profile_no_data_qsp_slims_doc() {
        let mut out = transform_profile(&json!({}), argo4()).expect("doc survives");
        assert!(out.data().is_empty(), "slim listing clears data");
        assert!(out.data_info().is_none(), "slim listing scrubs data_info");
    }

    #[test]
    fn profile_lone_except_data_values_normalizes_to_slim() {
        let mut out = transform_profile(&json!({"data": "except-data-values"}), argo4())
            .expect("doc survives normalization");
        assert!(out.data().is_empty());
        assert!(out.data_info().is_none());
    }

    #[test]
    fn profile_ignores_date_params() {
        // Dates are a Mongo filter for profiles — their presence must not
        // perturb the transform. Same slim listing as no params at all.
        let mut out = transform_profile(
            &json!({"startDate": "1990-01-01T00:00:00Z", "endDate": "1991-01-01T00:00:00Z"}),
            argo4(),
        )
        .expect("dates never drop a profile in the transform");
        assert!(out.data().is_empty());
        assert!(out.data_info().is_none());
    }

    #[test]
    fn profile_all_keeps_everything_with_data_info() {
        let mut out = transform_profile(&json!({"data": "all"}), argo4()).expect("doc survives");
        assert_eq!(out.data().len(), 4, "all four columns kept");
        let info = out.data_info().expect("data_info present when data= is set");
        assert_eq!(info.0.len(), 4);
    }

    #[test]
    fn profile_named_var_brings_pressure_and_qc_companions() {
        let mut out =
            transform_profile(&json!({"data": "temperature"}), argo4()).expect("doc survives");
        let info = out.data_info().expect("filtered data_info rides along");
        assert_eq!(
            info.0,
            vec![
                "pressure".to_string(),
                "pressure_argoqc".to_string(),
                "temperature".to_string(),
                "temperature_argoqc".to_string(),
            ],
            "pressure (+ its QC) pinned first, then the variable with its companion"
        );
        assert_eq!(out.data().len(), 4);
        // Column values follow the same order: pressure first, then the
        // requested variable.
        assert_eq!(out.data()[0][0], Some(11.9));
        assert_eq!(out.data()[2][0], Some(28.021));
        assert_eq!(out.data()[3][0], Some(2.0));
        // Companion data_info rows keep their real-world nulls.
        assert_eq!(info.2[3], vec![None, None]);
    }

    #[test]
    fn profile_pressure_request_is_not_duplicated() {
        // Explicitly asking for pressure must not double the implicit one.
        let mut out =
            transform_profile(&json!({"data": "pressure"}), argo4()).expect("doc survives");
        let info = out.data_info().unwrap();
        assert_eq!(
            info.0,
            vec!["pressure".to_string(), "pressure_argoqc".to_string()]
        );
        assert_eq!(out.data().len(), 2);
    }

    #[test]
    fn profile_explicit_companion_not_duplicated() {
        let mut out = transform_profile(
            &json!({"data": "temperature,temperature_argoqc"}),
            argo4(),
        )
        .expect("doc survives");
        let info = out.data_info().unwrap();
        assert_eq!(
            info.0,
            vec![
                "pressure".to_string(),
                "pressure_argoqc".to_string(),
                "temperature".to_string(),
                "temperature_argoqc".to_string(),
            ],
            "explicitly requesting the companion must not duplicate it"
        );
        assert_eq!(out.data().len(), 4);
    }

    #[test]
    fn profile_pressure_pinned_first_then_token_order() {
        // Even when the request names pressure last, the coordinate
        // columns are pinned to the front; the rest follow token order.
        let mut out = transform_profile(
            &json!({"data": "temperature,pressure"}),
            argo4(),
        )
        .expect("doc survives");
        let info = out.data_info().unwrap();
        assert_eq!(
            info.0,
            vec![
                "pressure".to_string(),
                "pressure_argoqc".to_string(),
                "temperature".to_string(),
                "temperature_argoqc".to_string(),
            ],
            "pressure first, then requested columns in token order"
        );
    }

    #[test]
    fn profile_unknown_var_drops_doc_despite_implicit_pressure() {
        // Survival is decided by the requested variables alone: this doc
        // has a perfectly good pressure column, but the user asked for a
        // variable it doesn't carry, so the row drops.
        let out = transform_profile(&json!({"data": "chlorophyll"}), argo4());
        assert!(out.is_none(), "no requested columns resolved -> row dropped");
    }

    #[test]
    fn profile_integer_tokens_are_ignored_phase1() {
        // QC filtering is phase 2: `data=temperature,1,3` must select the
        // same columns as `data=temperature` and suppress nothing yet.
        let mut with_qc = transform_profile(&json!({"data": "temperature,1,3"}), argo4())
            .expect("doc survives");
        let mut without = transform_profile(&json!({"data": "temperature"}), argo4())
            .expect("doc survives");
        assert_eq!(with_qc.data(), without.data());
        assert_eq!(with_qc.data_info(), without.data_info());
    }

    #[test]
    fn profile_all_integer_tokens_degrade_to_slim_listing() {
        // A bare QC filter with no variables is a 400 at validation
        // (validate_data_param, 2.x parity), so this is the transform's
        // defensive behaviour only: degrade to the slim listing rather
        // than panic or serve unfiltered data.
        let mut out = transform_profile(&json!({"data": "1"}), argo4()).expect("doc survives");
        assert!(out.data().is_empty());
        assert!(out.data_info().is_none());
    }

    #[test]
    fn profile_except_data_values_keeps_filtered_schema_only() {
        let mut out = transform_profile(
            &json!({"data": "temperature,except-data-values"}),
            argo4(),
        )
        .expect("except-data-values exempts drop-on-empty");
        assert!(out.data().is_empty(), "values cleared as requested");
        let info = out.data_info().expect("filtered data_info is the point");
        assert_eq!(
            info.0,
            vec![
                "pressure".to_string(),
                "pressure_argoqc".to_string(),
                "temperature".to_string(),
                "temperature_argoqc".to_string(),
            ]
        );
    }

    #[test]
    fn profile_nulls_survive_column_filtering() {
        let mut out =
            transform_profile(&json!({"data": "temperature"}), argo4()).expect("doc survives");
        assert_eq!(out.data()[2][1], None, "null cell passes through untouched");
    }

    #[test]
    fn profile_variable_without_companion_or_pressure_is_fine() {
        // A doc whose data_info has neither a `<var>_argoqc` companion nor
        // a pressure column: the variable comes back alone, no error —
        // both ride-alongs are opportunistic, never required.
        let doc = make_argo(
            "p2",
            vec![vec![Some(1.0), Some(2.0)]],
            &["temperature"],
        );
        let mut out = transform_profile(&json!({"data": "temperature"}), doc)
            .expect("doc survives");
        let info = out.data_info().unwrap();
        assert_eq!(info.0, vec!["temperature".to_string()]);
        assert_eq!(out.data().len(), 1);
    }

    // ---- ~ negation (profile-level presence filters) ---------------------------

    #[test]
    fn profile_negation_drops_docs_carrying_the_variable() {
        // argo4 carries temperature → a ~temperature request drops it,
        // regardless of the positive tokens also matching.
        let out = transform_profile(&json!({"data": "pressure,~temperature"}), argo4());
        assert!(out.is_none(), "doc listing a negated variable must drop");
    }

    #[test]
    fn profile_negation_passes_docs_lacking_the_variable() {
        // argo4 has no doxy → ~doxy passes, positive selection proceeds.
        let mut out = transform_profile(&json!({"data": "temperature,~doxy"}), argo4())
            .expect("doxy-less doc should survive ~doxy");
        let info = out.data_info().unwrap();
        assert!(info.0.contains(&"temperature".to_string()));
    }

    #[test]
    fn profile_only_negations_give_slim_listing() {
        // data=~doxy alone: the negation does the selecting; with no
        // positive tokens the response is the slim listing.
        let mut out = transform_profile(&json!({"data": "~doxy"}), argo4())
            .expect("doxy-less doc survives an only-negation request");
        assert!(out.data().is_empty());
        assert!(out.data_info().is_none());
        // And a doc that HAS doxy is dropped by the same request.
        let with_doxy = make_argo(
            "p3",
            vec![vec![Some(1.0)], vec![Some(2.0)]],
            &["pressure", "doxy"],
        );
        assert!(transform_profile(&json!({"data": "~doxy"}), with_doxy).is_none());
    }

    #[test]
    fn profile_negation_composes_with_all() {
        // Full columns of profiles lacking the negated variable.
        let mut out = transform_profile(&json!({"data": "all,~doxy"}), argo4())
            .expect("doxy-less doc survives all,~doxy");
        assert_eq!(out.data().len(), 4, "all four columns kept");
    }

    #[test]
    fn profile_negation_matches_names_exactly() {
        // ~doxy must not exclude a doc that carries only doxy2.
        let doc = make_argo(
            "p4",
            vec![vec![Some(1.0)], vec![Some(2.0)]],
            &["pressure", "doxy2"],
        );
        let out = transform_profile(&json!({"data": "pressure,~doxy"}), doc);
        assert!(out.is_some(), "doxy2 is not doxy — exact-name negation only");
    }

    #[test]
    fn profile_negation_with_no_data_info_passes() {
        // A doc with no data_info lists nothing, so it trivially lacks
        // every negated variable.
        let mut doc = argo4();
        doc.data_info = None;
        let out = transform_profile(&json!({"data": "all,~doxy"}), doc);
        assert!(out.is_some(), "no data_info -> nothing listed -> negation passes");
    }

    #[test]
    fn profile_doc_without_data_info_drops_on_named_request() {
        let mut doc = argo4();
        doc.data_info = None;
        assert!(
            transform_profile(&json!({"data": "temperature"}), doc).is_none(),
            "no data_info -> named variables can't resolve -> drop"
        );
        // But `all` needs no resolution — doc survives.
        let mut doc = argo4();
        doc.data_info = None;
        let out = transform_profile(&json!({"data": "all"}), doc);
        assert!(out.is_some(), "data=all needs no data_info");
    }
}
