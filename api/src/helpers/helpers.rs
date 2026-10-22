use chrono::{DateTime, Utc};
use mongodb::bson::DateTime as BsonDateTime;
use serde::{Serialize};
use actix_web::{HttpResponse};
use serde_json::{json, from_str};

use super::dataset_config::DatasetConfig;

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

/// Silently tidy a box's corner pair the way the 2.x API did: if the
/// latitudes arrive as (north, south) — e.g. box=[[0,10],[10,0]] — swap
/// them so the first corner is genuinely the southwest one, yielding
/// [[0,0],[10,10]].
///
/// Longitude is deliberately NOT reordered: `sw_lon > ne_lon` is
/// meaningful (a dateline-crossing box, split downstream into east/west
/// sub-boxes), so "fixing" it would silently turn an intentional
/// dateline box into a near-global one. Latitude has no such wrap —
/// boxes can't cross a pole — so a descending lat pair can only be a
/// mistake, and swapping it is unambiguous.
///
/// Call after `validlonlat` so the comparison sees clipped/wrapped
/// values. Malformed shapes are passed through untouched; shape
/// validation happens elsewhere.
pub fn tidy_box(mut coords: Vec<Vec<f64>>) -> Vec<Vec<f64>> {
    if coords.len() == 2
        && coords[0].len() == 2
        && coords[1].len() == 2
        && coords[0][1] > coords[1][1]
    {
        let sw_lat = coords[1][1];
        coords[1][1] = coords[0][1];
        coords[0][1] = sw_lat;
    }
    coords
}

pub fn string2bsondate(date_str: &str) -> Option<BsonDateTime> {
    date_str.parse::<DateTime<Utc>>().ok()
        .map(|dt| BsonDateTime::from_millis(dt.timestamp_millis()))
}

/// THE datetime rendering for everything this API emits: RFC-3339 with a
/// *fixed* three fractional digits (`1999-08-05T19:03:42.002Z`,
/// `2020-06-15T12:34:56.000Z`). Grid timeseries axes, profile
/// timestamps, source dates — one format, everywhere, forever.
///
/// This is a deliberate, sanctioned normalization over 2.x, which
/// emitted whole seconds for grid axes: `...T00:00:00Z` (2.x) and
/// `...T00:00:00.000Z` (3.x) are the same instant, and parity checks
/// against 2.x should compare dates *parsed*, not byte-wise. The
/// fixed-width fraction (rather than omit-when-zero) matches JS
/// `Date.toISOString()` and keeps output byte-stable — clients never
/// see mixed formats. `string2bsondate` parses every rendering (with or
/// without millis), so values round-trip exactly.
pub fn bsondate2string(date: &BsonDateTime) -> String {
    let millis = date.timestamp_millis();
    let datetime = DateTime::<Utc>::from_timestamp(millis / 1000, (millis % 1000) as u32 * 1_000_000);
    match datetime {
        Some(dt) => dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
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

/// Hard cap on the `mostrecent` qsp: the mode bypasses tile pagination
/// (a global "N most recent matching" can't be tiled), so N is the only
/// thing bounding the response. 1000 slim docs is comfortably under one
/// grid tile's worst case; revisit with usage.
pub const MOSTRECENT_CAP: i64 = 1000;

/// Levenshtein edit distance — used to suggest a "did you mean" target
/// when a qsp doesn't match the whitelist. Iterative table with O(m*n)
/// space; m and n are at most ~15 chars in practice (the longest
/// whitelist entry is `verticalRange`), so this is trivially fast and
/// not worth optimising into the row-rolling form.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..=m {
        dp[i][0] = i;
    }
    for j in 0..=n {
        dp[0][j] = j;
    }
    for i in 1..=m {
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }
    dp[m][n]
}

/// Return the whitelist entry closest to `unknown` if one is within
/// edit distance 2 — covers common typo flavours (single-char
/// substitution / deletion / insertion) without firing on totally
/// unrelated names. `None` when nothing is within range.
///
/// Distance is computed on the *lowercased* forms of both sides, so a
/// fully-uppercased typo (`STARTDATE`) or any mixed-case shout still
/// gets a useful suggestion — case differences between every letter
/// would otherwise push the distance well past the threshold even
/// though the user clearly meant the same name.
fn suggest_qsp(unknown: &str, allowed: &'static [&'static str]) -> Option<&'static str> {
    let unknown_lower = unknown.to_lowercase();
    allowed
        .iter()
        .map(|allowed| (*allowed, edit_distance(&unknown_lower, &allowed.to_lowercase())))
        .filter(|(_, dist)| *dist <= 2)
        .min_by_key(|&(_, dist)| dist)
        .map(|(allowed, _)| allowed)
}

pub fn validate_query_params(
    params: &serde_json::Value,
    config: &DatasetConfig,
) -> Result<(), HttpResponse> {

    // Reject unknown qsp names before any further validation so a typo
    // (e.g. `start_Date` instead of `startDate`) fails loudly with a
    // useful suggestion rather than getting silently ignored downstream.
    // The whitelist is per-dataset (`config.allowed_qsp`): /argo accepts
    // profile-family params (`platform`, `source`, `mostrecent`) the
    // grids reject, and the suggestion machinery follows the same list.
    let allowed = config.allowed_qsp;
    if let Some(obj) = params.as_object() {
        for key in obj.keys() {
            if !allowed.contains(&key.as_str()) {
                let msg = match suggest_qsp(key, allowed) {
                    Some(closest) => format!(
                        "Unknown query parameter '{}'. Did you mean '{}'? Allowed parameters: {}",
                        key, closest, allowed.join(", ")
                    ),
                    None => format!(
                        "Unknown query parameter '{}'. Allowed parameters: {}",
                        key, allowed.join(", ")
                    ),
                };
                return Err(HttpResponse::BadRequest().json(json!({"error": msg})));
            }
        }
    }

    // `platform` selects every profile of one float via an anchored _id
    // prefix match. Restricting the value to alphanumerics keeps the
    // regex we build from it literal (no metacharacters to escape) —
    // WMO platform identifiers are alphanumeric in practice.
    if let Some(platform) = params.get("platform") {
        let ok = platform
            .as_str()
            .map_or(false, |p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()));
        if !ok {
            return Err(HttpResponse::BadRequest().json(json!({
                "error": "'platform' should be a non-empty alphanumeric float identifier, for example platform=13857"
            })));
        }
    }

    // `source` is a comma list of pipeline tags, each optionally negated
    // with `~` (require absence). Tokens validate against the dataset's
    // `allowed_sources`; a positive and negated copy of the same tag is
    // unsatisfiable and rejected.
    if let Some(source) = params.get("source") {
        let raw = source.as_str().unwrap_or("");
        let mut pos: Vec<&str> = Vec::new();
        let mut neg: Vec<&str> = Vec::new();
        for token in raw.split(',') {
            if token.is_empty() {
                return Err(HttpResponse::BadRequest().json(json!({
                    "error": "'source' contains an empty token (likely a leading, trailing, or doubled comma)"
                })));
            }
            let (bucket, tag) = match token.strip_prefix('~') {
                Some(t) => (&mut neg, t),
                None => (&mut pos, token),
            };
            if !config.allowed_sources.contains(&tag) {
                return Err(HttpResponse::BadRequest().json(json!({
                    "error": format!(
                        "Unknown 'source' value '{}'. Allowed values: {} (prefix with ~ to require absence).",
                        token,
                        config.allowed_sources.join(", ")
                    )
                })));
            }
            bucket.push(tag);
        }
        if pos.iter().any(|p| neg.contains(p)) {
            return Err(HttpResponse::BadRequest().json(json!({
                "error": "'source' requires and negates the same tag — unsatisfiable"
            })));
        }
    }

    // `mostrecent` bypasses tile pagination (sort by timestamp desc +
    // limit N), so N is the only response-size bound: it must be a
    // positive integer at or under the cap.
    if let Some(mostrecent) = params.get("mostrecent") {
        let ok = mostrecent
            .as_str()
            .and_then(|m| m.parse::<i64>().ok())
            .map_or(false, |n| n >= 1 && n <= MOSTRECENT_CAP);
        if !ok {
            return Err(HttpResponse::BadRequest().json(json!({
                "error": format!(
                    "'mostrecent' should be a positive integer no greater than {}",
                    MOSTRECENT_CAP
                )
            })));
        }
    }

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

    // If 'center' is defined, it should be comma-separated 'lon,lat'
    // (2.x API format — no brackets, e.g. center=0,0).
    if let Some(center) = params.get("center") {
        if !is_comma_separated_pair(center.as_str().unwrap_or("")) {
            return Err(HttpResponse::BadRequest().json(json!({"error": "'center' should be comma-separated as lon,lat, for example center=-20.5,42"})));
        }
    }

    // Same 2.x format for 'verticalRange': comma-separated 'lo,hi',
    // e.g. verticalRange=0,10.
    if let Some(vertical_range) = params.get("verticalRange") {
        if !is_comma_separated_pair(vertical_range.as_str().unwrap_or("")) {
            return Err(HttpResponse::BadRequest().json(json!({"error": "'verticalRange' should be comma-separated as lo,hi, for example verticalRange=0,10"})));
        }
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

/// True when `s` is exactly two comma-separated finite floats, the 2.x
/// API's qsp pair format (e.g. `center=-20.5,42`, `verticalRange=0,10`).
/// Bracketed JSON arrays (the pre-2.x format) fail this check by design.
fn is_comma_separated_pair(s: &str) -> bool {
    let parts: Vec<&str> = s.split(',').collect();
    parts.len() == 2
        && parts
            .iter()
            .all(|p| p.trim().parse::<f64>().map_or(false, f64::is_finite))
}

/// Universal tokens accepted in the `data=` qsp regardless of which
/// dataset is being queried. `all` and `except-data-values` control
/// response shape rather than naming a variable; both are documented
/// in `api/PAGINATION.md`.
const UNIVERSAL_DATA_TOKENS: &[&str] = &["all", "except-data-values"];

/// Strip the dataset's declared token suffixes off a `data=` token:
/// one optional companion (outermost, e.g. `_argoqc`), then one
/// optional modifier (e.g. `_std` / `_med`), returning the candidate
/// base name. Mirrors the 2.x validator pattern's
/// `base(_std|_med)?(_argoqc)?` ordering — a suffix in the wrong order
/// (e.g. `doxy_argoqc_std`) deliberately fails to strip cleanly and is
/// rejected. Datasets that declare no suffixes get the token back
/// unchanged (exact matching, the grids' behaviour).
fn strip_data_suffixes<'a>(token: &'a str, config: &DatasetConfig) -> &'a str {
    let t = match config.data_var_companion {
        Some(c) => token.strip_suffix(c).unwrap_or(token),
        None => token,
    };
    config
        .data_var_modifiers
        .iter()
        .find_map(|m| t.strip_suffix(m))
        .unwrap_or(t)
}

/// True when `token` matches the dataset's variable grammar:
/// `base[modifier][companion]` with the base in `allowed_data_vars`.
fn matches_data_vocabulary(token: &str, config: &DatasetConfig) -> bool {
    config
        .allowed_data_vars
        .contains(&strip_data_suffixes(token, config))
}

/// Validate the contents of the `data=` qsp against the dataset's
/// `allowed_data_vars`. Each comma-separated token must be one of:
///
///   - a universal token (`all` or `except-data-values`),
///   - a variable name in `config.allowed_data_vars`, or
///   - an integer (accepted as a QC filter; the QC system is dataset-
///     agnostic so any non-negative integer is allowed at this layer).
///
/// Anything else returns 400 with a Levenshtein-suggested correction
/// when one of the named candidates is within edit distance 2.
///
/// No-op when the `data=` qsp is absent.
pub fn validate_data_param(
    params: &serde_json::Value,
    config: &DatasetConfig,
) -> Result<(), HttpResponse> {
    let raw = match params.get("data").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return Ok(()),
    };

    // Track what kinds of tokens appear: integer QC filters only mean
    // something when there's a variable (or `all`) to filter, so a list
    // with integers and no variable is rejected after the loop.
    let mut has_integer = false;
    let mut has_variable = false;

    for token in raw.split(',') {
        if token.is_empty() {
            return Err(HttpResponse::BadRequest().json(json!({
                "error": "'data' qsp contains an empty token (likely a leading, trailing, or doubled comma)"
            })));
        }
        // `~variable` is a profile-level presence NEGATION (keep only
        // docs that don't carry the variable), accepted only where the
        // dataset opts in. A negation selects profiles rather than
        // requesting columns, so it deliberately does NOT set
        // `has_variable` — integer QC filters still need a positive
        // variable to act on.
        if let Some(negated) = token.strip_prefix('~') {
            if !config.data_var_negation {
                return Err(HttpResponse::BadRequest().json(json!({
                    "error": format!(
                        "'data' token '{}': negation (~) is not supported for this dataset",
                        token
                    )
                })));
            }
            if UNIVERSAL_DATA_TOKENS.contains(&negated) || negated.parse::<i64>().is_ok() {
                return Err(HttpResponse::BadRequest().json(json!({
                    "error": format!(
                        "'data' token '{}': only variable names can be negated",
                        token
                    )
                })));
            }
            if matches_data_vocabulary(negated, config) {
                continue;
            }
            let msg = unknown_data_token_message(negated, config);
            return Err(HttpResponse::BadRequest().json(json!({"error": msg})));
        }
        if UNIVERSAL_DATA_TOKENS.contains(&token) {
            if token == "all" {
                has_variable = true;
            }
            continue;
        }
        if matches_data_vocabulary(token, config) {
            has_variable = true;
            continue;
        }
        // Integer QC filter (any signed integer; the QC layer
        // interprets the value, this layer just gate-keeps the shape).
        if token.parse::<i64>().is_ok() {
            has_integer = true;
            continue;
        }
        let msg = unknown_data_token_message(token, config);
        return Err(HttpResponse::BadRequest().json(json!({"error": msg})));
    }

    // `data=1` (or `data=1,except-data-values`) is nonsense — a QC
    // filter with nothing to filter. 2.x rejected it and so do we.
    if has_integer && !has_variable {
        return Err(HttpResponse::BadRequest().json(json!({
            "error": "'data' contains integer QC filters but no variable to filter: pair integers with at least one variable name (or 'all'), e.g. data=temperature,1"
        })));
    }
    Ok(())
}

fn unknown_data_token_message(token: &str, config: &DatasetConfig) -> String {
    // Suggest only against named candidates (universal tokens + per-
    // dataset variable names). Integer QC filters aren't suggestible
    // — the user either types one correctly or they don't.
    //
    // Distance is computed against the *base* form of the typo (suffixes
    // stripped per the dataset's grammar), case-insensitively, so a user
    // typing `doxi_argoqc` still gets pointed at `doxy`. Validation
    // itself is case-sensitive (the user is asked to fix their request),
    // but the suggestion should be forgiving.
    let candidates: Vec<&str> = UNIVERSAL_DATA_TOKENS
        .iter()
        .copied()
        .chain(config.allowed_data_vars.iter().copied())
        .collect();

    let token_lower = strip_data_suffixes(token, config).to_lowercase();
    let suggestion = candidates
        .iter()
        .map(|c| (*c, edit_distance(&token_lower, &c.to_lowercase())))
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|&(_, d)| d)
        .map(|(c, _)| c);

    let allowed_list = candidates.join(", ");
    match suggestion {
        Some(s) => format!(
            "Unknown 'data' value '{}'. Did you mean '{}'? Allowed values for this dataset: {} (plus any integer for QC filtering).",
            token, s, allowed_list
        ),
        None => format!(
            "Unknown 'data' value '{}'. Allowed values for this dataset: {} (plus any integer for QC filtering).",
            token, allowed_list
        ),
    }
}

/// Enforce the dataset's `max_radius_meters` for `center + radius` queries.
///
/// The `radius` qsp is expressed in km (2.x API format); the config cap
/// stays in meters, so we convert before comparing.
///
/// `center + radius` is the one geo mode that *isn't* spatially tiled
/// (Mongo's `$near` handles the bounding internally), so a request with an
/// unbounded radius could ask for a half-globe disk and return millions of
/// docs. We cap the radius in config and reject anything past it here.
///
/// Only runs when `center` is present in the params: if center is absent,
/// this mode doesn't apply and the cap is irrelevant. When center IS
/// present, `validate_query_params` has already required `radius` to be
/// present as well; we additionally require it to parse as a non-negative
/// finite f64 (a pre-existing bug elsewhere unwraps that parse — catching
/// it here makes the error path graceful for callers).
pub fn validate_radius_cap(
    params: &serde_json::Value,
    config: &DatasetConfig,
) -> Result<(), HttpResponse> {
    if params.get("center").is_none() {
        return Ok(());
    }
    let radius_str = match params.get("radius").and_then(|v| v.as_str()) {
        Some(s) => s,
        // validate_query_params should have caught a missing radius
        // already; if it didn't, treating this as "no cap to enforce"
        // hands the request through to the existing error path rather
        // than masking it with a different 400.
        None => return Ok(()),
    };
    // Explicit turbofish: the match guard runs before the let-binding's
    // type annotation flows back to `parse()`, so type inference can't
    // resolve `F` otherwise.
    let radius: f64 = match radius_str.parse::<f64>() {
        Ok(r) if r.is_finite() && r >= 0.0 => r,
        _ => {
            return Err(HttpResponse::BadRequest().json(json!({
                "error": format!(
                    "'radius' must be a non-negative finite number, got '{}'",
                    radius_str
                )
            })));
        }
    };
    // radius qsp is km; cap is stored in meters
    if radius * 1000.0 > config.max_radius_meters {
        return Err(HttpResponse::BadRequest().json(json!({
            "error": format!(
                "radius {} km exceeds the dataset's maximum allowed radius of {} km",
                radius, config.max_radius_meters / 1000.0
            )
        })));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::dataset_config::PageAxis;
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

    // ---- tidy_box ------------------------------------------------------------

    #[test]
    fn tidy_box_swaps_reversed_latitudes() {
        // box=[[0,10],[10,0]] — corners given as NW/SE — becomes SW/NE
        let out = tidy_box(vec![vec![0.0, 10.0], vec![10.0, 0.0]]);
        assert_eq!(out, vec![vec![0.0, 0.0], vec![10.0, 10.0]]);
    }

    #[test]
    fn tidy_box_leaves_well_ordered_box_unchanged() {
        let coords = vec![vec![0.0, 0.0], vec![10.0, 10.0]];
        assert_eq!(tidy_box(coords.clone()), coords);
    }

    #[test]
    fn tidy_box_never_touches_longitudes() {
        // sw_lon > ne_lon is a dateline-crossing box, not a mistake —
        // the lon pair must survive tidying even when lats also swap.
        let out = tidy_box(vec![vec![170.0, 20.0], vec![-170.0, 10.0]]);
        assert_eq!(out, vec![vec![170.0, 10.0], vec![-170.0, 20.0]]);
    }

    #[test]
    fn tidy_box_passes_malformed_shapes_through() {
        let coords = vec![vec![0.0, 10.0]];
        assert_eq!(tidy_box(coords.clone()), coords);
        let coords = vec![vec![0.0, 10.0, 5.0], vec![10.0, 0.0]];
        assert_eq!(tidy_box(coords.clone()), coords);
    }

    // ---- date round-trips ----------------------------------------------------

    #[test]
    fn string_to_bson_to_string_round_trips() {
        // Rendering normalizes to fixed-millis, so a whole-second input
        // comes back with `.000` appended — same instant, wider string.
        let s = "2020-06-15T12:34:56Z";
        let d = string2bsondate(s).expect("should parse");
        assert_eq!(bsondate2string(&d), "2020-06-15T12:34:56.000Z");
        // Semantic round trip is exact.
        assert_eq!(string2bsondate(&bsondate2string(&d)), Some(d));
    }

    #[test]
    fn string2bsondate_rejects_garbage() {
        assert!(string2bsondate("not a date").is_none());
    }

    #[test]
    fn bsondate2string_pads_whole_seconds_to_three_digits() {
        // Fixed-width millis: a whole-second value renders `.000`, so
        // responses never mix formats (JS toISOString convention).
        let d = string2bsondate("2020-06-15T12:34:56Z").unwrap();
        assert_eq!(bsondate2string(&d), "2020-06-15T12:34:56.000Z");
    }

    #[test]
    fn bsondate2string_preserves_real_millis() {
        let d = string2bsondate("1999-08-05T19:03:42.002Z").unwrap();
        assert_eq!(bsondate2string(&d), "1999-08-05T19:03:42.002Z");
    }

    #[test]
    fn rendering_round_trips_exactly() {
        for s in ["1999-08-05T19:03:42.002Z", "2020-06-15T12:34:56.000Z"] {
            let d = string2bsondate(s).unwrap();
            let back = string2bsondate(&bsondate2string(&d)).unwrap();
            assert_eq!(d, back, "round trip must be exact for {}", s);
        }
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

    /// Grid-shaped config for whitelist tests (GRID_QSP, no profile
    /// params). Other knobs are irrelevant to validate_query_params.
    const QSP_TEST_CONFIG: DatasetConfig = DatasetConfig {
        tile_degrees: 10.0,
        max_radius_meters: 1_000_000.0,
        page_axis: PageAxis::Levels(&[0.0]),
        coverage_bbox: None,
        allowed_data_vars: &[],
        data_var_modifiers: &[],
        data_var_companion: None,
        data_var_negation: false,
        allowed_qsp: crate::helpers::dataset_config::GRID_QSP,
        allowed_sources: &[],
    };

    /// Argo-shaped config for whitelist tests (ARGO_QSP + sources).
    const ARGO_QSP_TEST_CONFIG: DatasetConfig = DatasetConfig {
        tile_degrees: 5.0,
        max_radius_meters: 1_000_000.0,
        page_axis: PageAxis::TimeSlabs { width_days: 365 },
        coverage_bbox: None,
        allowed_data_vars: &[],
        data_var_modifiers: &[],
        data_var_companion: None,
        data_var_negation: true,
        allowed_qsp: crate::helpers::dataset_config::ARGO_QSP,
        allowed_sources: crate::helpers::dataset_config::ARGO_SOURCES,
    };

    #[test]
    fn validate_accepts_empty_params() {
        let params = json!({});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_ok());
    }

    #[test]
    fn validate_rejects_two_geo_params() {
        let params = json!({
            "polygon": "[[0,0],[1,0],[1,1],[0,0]]",
            "box": "[[0,0],[1,1]]"
        });
        let err = validate_query_params(&params, &QSP_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn validate_rejects_three_geo_params() {
        let params = json!({
            "polygon": "[[0,0],[1,0],[1,1],[0,0]]",
            "box": "[[0,0],[1,1]]",
            "center": "0,0"
        });
        let err = validate_query_params(&params, &QSP_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn validate_rejects_center_without_radius() {
        let params = json!({"center": "0,0"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_err());
    }

    #[test]
    fn validate_rejects_radius_without_center() {
        let params = json!({"radius": "1000"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_err());
    }

    #[test]
    fn validate_accepts_center_and_radius() {
        let params = json!({"center": "0,0", "radius": "1000"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_ok());
    }

    #[test]
    fn validate_rejects_bracketed_center() {
        // 2.x format is comma-separated lon,lat; the old bracketed JSON
        // array form should 400 with a format hint rather than 500.
        let params = json!({"center": "[0,0]", "radius": "1000"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_err());
    }

    #[test]
    fn validate_rejects_malformed_center() {
        for bad in ["0", "0,0,0", "a,b", "0,", ",0", "0,inf"] {
            let params = json!({"center": bad, "radius": "1000"});
            assert!(
                validate_query_params(&params, &QSP_TEST_CONFIG).is_err(),
                "center '{}' should be rejected",
                bad
            );
        }
    }

    #[test]
    fn validate_accepts_center_with_spaces_and_signs() {
        let params = json!({"center": "-20.5, 42", "radius": "1000"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_ok());
    }

    #[test]
    fn validate_accepts_comma_separated_vertical_range() {
        let params = json!({"verticalRange": "0,10"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_ok());
        // negative bounds are legitimate (e.g. heights above a datum)
        let params = json!({"verticalRange": "-5,5"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_ok());
    }

    #[test]
    fn validate_rejects_bracketed_vertical_range() {
        // 2.x format is comma-separated lo,hi; the old bracketed JSON
        // array form should 400 with a format hint rather than 500.
        let params = json!({"verticalRange": "[0,10]"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_err());
    }

    #[test]
    fn validate_rejects_malformed_vertical_range() {
        for bad in ["0", "0,10,20", "a,b", "0,", ",10", "0,inf"] {
            let params = json!({"verticalRange": bad});
            assert!(
                validate_query_params(&params, &QSP_TEST_CONFIG).is_err(),
                "verticalRange '{}' should be rejected",
                bad
            );
        }
    }

    #[test]
    fn validate_rejects_polygon_too_few_points() {
        let params = json!({"polygon": "[[0,0],[1,0],[0,0]]"}); // only 3 points
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_err());
    }

    #[test]
    fn validate_rejects_polygon_not_closed() {
        let params = json!({"polygon": "[[0,0],[1,0],[1,1],[0,1]]"}); // first != last
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_err());
    }

    #[test]
    fn validate_rejects_polygon_with_bad_point() {
        let params = json!({"polygon": "[[0,0,0],[1,0],[1,1],[0,0,0]]"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_err());
    }

    #[test]
    fn validate_accepts_well_formed_polygon() {
        let params = json!({"polygon": "[[0,0],[1,0],[1,1],[0,1],[0,0]]"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_ok());
    }

    #[test]
    fn validate_rejects_unparseable_polygon_string() {
        let params = json!({"polygon": "not json"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_err());
    }

    #[test]
    fn validate_rejects_bad_start_date() {
        let params = json!({"startDate": "yesterday"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_err());
    }

    #[test]
    fn validate_rejects_bad_end_date() {
        let params = json!({"endDate": "2020/01/01"});
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_err());
    }

    #[test]
    fn validate_accepts_rfc3339_dates() {
        let params = json!({
            "startDate": "2020-01-01T00:00:00Z",
            "endDate":   "2020-12-31T23:59:59Z"
        });
        assert!(validate_query_params(&params, &QSP_TEST_CONFIG).is_ok());
    }

    // ---- qsp whitelist --------------------------------------------------------

    #[test]
    fn validate_rejects_unknown_qsp() {
        let params = json!({"start_Date": "2020-01-01T00:00:00Z"});
        let err = validate_query_params(&params, &QSP_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn validate_rejects_unknown_qsp_even_when_other_params_valid() {
        // A request that's well-formed apart from one stray unknown
        // param should still be rejected — the whitelist check fires
        // before any other validation.
        let params = json!({
            "box": "[[0,0],[10,10]]",
            "frobnicate": "yes",
        });
        let err = validate_query_params(&params, &QSP_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn validate_known_qsps_pass_through_whitelist() {
        // Sanity check: every entry in the whitelist that doesn't have
        // its own shape constraints should be accepted on its own.
        // (Constrained params like `polygon` have their own dedicated
        // tests above; this is purely the whitelist check.)
        for (key, value) in [
            ("id", "doc1"),
            ("data", "all"),
            ("compression", "minimal"),
            ("batchmeta", "true"),
            ("tile_index", "5"),
            ("startDate", "2020-01-01T00:00:00Z"),
            ("endDate", "2020-12-31T23:59:59Z"),
        ] {
            let params = json!({ key: value });
            assert!(
                validate_query_params(&params, &QSP_TEST_CONFIG).is_ok(),
                "whitelist should accept '{}' but rejected it",
                key
            );
        }
    }

    #[test]
    fn whitelist_is_per_dataset() {
        // Profile-family params 400 on a grid config but pass on argo's.
        for key in ["platform", "source", "mostrecent"] {
            let value = match key {
                "platform" => "13857",
                "source" => "argo_core",
                _ => "10",
            };
            let params = json!({ key: value });
            assert!(
                validate_query_params(&params, &QSP_TEST_CONFIG).is_err(),
                "grids should reject '{}'",
                key
            );
            assert!(
                validate_query_params(&params, &ARGO_QSP_TEST_CONFIG).is_ok(),
                "argo should accept '{}'",
                key
            );
        }
    }

    #[test]
    fn validate_platform_shape() {
        for bad in ["", "13857_068", "138 57", "^13857", "13857$"] {
            let params = json!({"platform": bad});
            assert!(
                validate_query_params(&params, &ARGO_QSP_TEST_CONFIG).is_err(),
                "platform '{}' should be rejected",
                bad
            );
        }
        let params = json!({"platform": "4902911"});
        assert!(validate_query_params(&params, &ARGO_QSP_TEST_CONFIG).is_ok());
    }

    #[test]
    fn validate_source_tokens() {
        for ok in ["argo_core", "~argo_bgc", "argo_core,~argo_deep", "argo_core,argo_bgc"] {
            let params = json!({"source": ok});
            assert!(
                validate_query_params(&params, &ARGO_QSP_TEST_CONFIG).is_ok(),
                "source '{}' should be accepted",
                ok
            );
        }
        for bad in ["argo_frobnicate", "~bogus", "argo_core,", "argo_bgc,~argo_bgc"] {
            let params = json!({"source": bad});
            assert!(
                validate_query_params(&params, &ARGO_QSP_TEST_CONFIG).is_err(),
                "source '{}' should be rejected",
                bad
            );
        }
    }

    #[test]
    fn validate_mostrecent_bounds() {
        for ok in ["1", "50", "1000"] {
            let params = json!({"mostrecent": ok});
            assert!(
                validate_query_params(&params, &ARGO_QSP_TEST_CONFIG).is_ok(),
                "mostrecent '{}' should be accepted",
                ok
            );
        }
        for bad in ["0", "-5", "1001", "many", "1.5"] {
            let params = json!({"mostrecent": bad});
            assert!(
                validate_query_params(&params, &ARGO_QSP_TEST_CONFIG).is_err(),
                "mostrecent '{}' should be rejected",
                bad
            );
        }
    }

    #[test]
    fn edit_distance_handles_common_typos() {
        assert_eq!(edit_distance("startdate", "startDate"), 1); // case-only sub
        assert_eq!(edit_distance("start_Date", "startDate"), 1); // single extra char
        assert_eq!(edit_distance("verticalrange", "verticalRange"), 1); // case
        assert_eq!(edit_distance("Box", "box"), 1);
        assert_eq!(edit_distance("", "box"), 3); // pure insertion cost
        assert_eq!(edit_distance("box", "box"), 0);
        assert!(edit_distance("zzzzzzzzz", "startDate") > 2);
    }

    #[test]
    fn suggest_qsp_finds_close_match() {
        assert_eq!(suggest_qsp("start_Date", crate::helpers::dataset_config::GRID_QSP), Some("startDate"));
        assert_eq!(suggest_qsp("startdate", crate::helpers::dataset_config::GRID_QSP), Some("startDate"));
        assert_eq!(suggest_qsp("verticalrange", crate::helpers::dataset_config::GRID_QSP), Some("verticalRange"));
        assert_eq!(suggest_qsp("Box", crate::helpers::dataset_config::GRID_QSP), Some("box"));
    }

    #[test]
    fn suggest_qsp_is_case_insensitive() {
        // Fully-uppercased or arbitrarily-cased typos should still get
        // a useful suggestion — without case-folding, every letter
        // would count as a substitution and the distance would blow
        // past the threshold.
        assert_eq!(suggest_qsp("STARTDATE", crate::helpers::dataset_config::GRID_QSP), Some("startDate"));
        assert_eq!(suggest_qsp("StArTdAtE", crate::helpers::dataset_config::GRID_QSP), Some("startDate"));
        assert_eq!(suggest_qsp("BOX", crate::helpers::dataset_config::GRID_QSP), Some("box"));
    }

    #[test]
    fn suggest_qsp_returns_none_for_unrelated_input() {
        // A name with no plausibly close match in the whitelist
        // should return None rather than reaching for the nearest
        // entry no matter how far.
        assert_eq!(suggest_qsp("frobnicate", crate::helpers::dataset_config::GRID_QSP), None);
        assert_eq!(suggest_qsp("xyzzy", crate::helpers::dataset_config::GRID_QSP), None);
    }

    // ---- validate_radius_cap -------------------------------------------------

    /// Minimal config for radius-cap testing. tile_degrees / levels are
    /// irrelevant for this validator.
    const RADIUS_TEST_CONFIG: DatasetConfig = DatasetConfig {
        tile_degrees: 10.0,
        max_radius_meters: 1_000_000.0, // 1000 km
        page_axis: PageAxis::Levels(&[0.0]),
        coverage_bbox: None,
        allowed_data_vars: &[],
        data_var_modifiers: &[],
        data_var_companion: None,
        data_var_negation: false,
        allowed_qsp: crate::helpers::dataset_config::GRID_QSP,
        allowed_sources: &[],
    };

    /// Test config with a couple of allowed variable names, used by
    /// the `validate_data_param` tests below.
    const DATA_TEST_CONFIG: DatasetConfig = DatasetConfig {
        tile_degrees: 10.0,
        max_radius_meters: 1_000_000.0,
        page_axis: PageAxis::Levels(&[0.0]),
        coverage_bbox: None,
        allowed_data_vars: &["THETA", "SALT"],
        data_var_modifiers: &[],
        data_var_companion: None,
        data_var_negation: false,
        allowed_qsp: crate::helpers::dataset_config::GRID_QSP,
        allowed_sources: &[],
    };

    // ---- validate_data_param --------------------------------------------------

    #[test]
    fn validate_data_param_noop_when_data_absent() {
        let params = json!({});
        assert!(validate_data_param(&params, &DATA_TEST_CONFIG).is_ok());
    }

    #[test]
    fn validate_data_param_accepts_universal_tokens() {
        for token in ["all", "except-data-values"] {
            let params = json!({ "data": token });
            assert!(
                validate_data_param(&params, &DATA_TEST_CONFIG).is_ok(),
                "universal token '{}' should be accepted",
                token
            );
        }
    }

    #[test]
    fn validate_data_param_accepts_per_dataset_var_names() {
        for token in ["THETA", "SALT"] {
            let params = json!({ "data": token });
            assert!(
                validate_data_param(&params, &DATA_TEST_CONFIG).is_ok(),
                "allowed var '{}' should be accepted",
                token
            );
        }
    }

    #[test]
    fn validate_data_param_accepts_integer_qc_filters_alongside_a_variable() {
        // Integers are QC filters — meaningful only with a variable (or
        // `all`) to filter. Paired, any signed integer passes; the QC
        // layer (downstream) interprets the actual value.
        for token in ["0", "1", "42", "-1", "9999"] {
            let params = json!({ "data": format!("THETA,{}", token) });
            assert!(
                validate_data_param(&params, &DATA_TEST_CONFIG).is_ok(),
                "integer token '{}' alongside a variable should be accepted",
                token
            );
        }
        let params = json!({ "data": "all,3" });
        assert!(validate_data_param(&params, &DATA_TEST_CONFIG).is_ok());
    }

    #[test]
    fn validate_data_param_rejects_bare_integer_qc_filters() {
        // `data=1` is a QC filter with nothing to filter — nonsense that
        // 2.x rejected, and so do we. `except-data-values` names no
        // variable either, so it doesn't rescue the list.
        for raw in ["1", "1,3", "-1", "1,except-data-values"] {
            let params = json!({ "data": raw });
            let err = validate_data_param(&params, &DATA_TEST_CONFIG).unwrap_err();
            assert_eq!(err.status(), 400, "'{}' should be rejected", raw);
        }
    }

    // ---- suffix grammar (profile-family token shapes) --------------------------

    /// Argo-shaped grammar config: bases plus `_std`/`_med` modifiers and
    /// the `_argoqc` companion, per the 2.x validator pattern.
    const GRAMMAR_TEST_CONFIG: DatasetConfig = DatasetConfig {
        tile_degrees: 10.0,
        max_radius_meters: 1_000_000.0,
        page_axis: PageAxis::Levels(&[0.0]),
        coverage_bbox: None,
        allowed_data_vars: &["doxy", "pressure"],
        data_var_modifiers: &["_std", "_med"],
        data_var_companion: Some("_argoqc"),
        data_var_negation: true,
        allowed_qsp: crate::helpers::dataset_config::ARGO_QSP,
        allowed_sources: crate::helpers::dataset_config::ARGO_SOURCES,
    };

    #[test]
    fn grammar_accepts_all_suffix_combinations() {
        // base(_std|_med)?(_argoqc)? — every combination of the optional
        // suffixes validates.
        for token in [
            "doxy",
            "doxy_std",
            "doxy_med",
            "doxy_argoqc",
            "doxy_std_argoqc",
            "doxy_med_argoqc",
        ] {
            let params = json!({ "data": token });
            assert!(
                validate_data_param(&params, &GRAMMAR_TEST_CONFIG).is_ok(),
                "token '{}' should be accepted",
                token
            );
        }
    }

    #[test]
    fn grammar_rejects_wrong_suffix_order_and_unknown_bases() {
        for token in [
            "doxy_argoqc_std", // companion must be outermost
            "doxy_std_med",    // at most one modifier
            "chla",            // base not in this config's vocabulary
            "chla_argoqc",     // suffixing an unknown base doesn't help
            "_argoqc",         // suffix with no base
        ] {
            let params = json!({ "data": token });
            assert!(
                validate_data_param(&params, &GRAMMAR_TEST_CONFIG).is_err(),
                "token '{}' should be rejected",
                token
            );
        }
    }

    #[test]
    fn grammar_suffixed_token_counts_as_a_variable_for_integer_pairing() {
        // A suffixed variable satisfies the "integers need a variable"
        // rule just like a bare one.
        let params = json!({ "data": "doxy_argoqc,1" });
        assert!(validate_data_param(&params, &GRAMMAR_TEST_CONFIG).is_ok());
    }

    #[test]
    fn grammar_grids_stay_exact_match() {
        // A grid config declares no suffixes, so suffixed forms of its
        // variables are rejected — the grammar is opt-in per dataset.
        let params = json!({ "data": "THETA_argoqc" });
        assert!(validate_data_param(&params, &DATA_TEST_CONFIG).is_err());
    }

    #[test]
    fn grammar_suggestion_strips_suffixes_before_matching() {
        // A typo'd base inside a suffixed token still earns a suggestion
        // pointing at the base name.
        let msg = unknown_data_token_message("doxi_argoqc", &GRAMMAR_TEST_CONFIG);
        assert!(
            msg.contains("Did you mean 'doxy'"),
            "suggestion missing in: {}",
            msg
        );
    }

    // ---- ~ negation validation --------------------------------------------------

    #[test]
    fn negation_accepts_valid_variable_tokens() {
        for raw in ["~doxy", "~doxy_std_argoqc", "pressure,~doxy", "all,~doxy"] {
            let params = json!({ "data": raw });
            assert!(
                validate_data_param(&params, &GRAMMAR_TEST_CONFIG).is_ok(),
                "'{}' should be accepted",
                raw
            );
        }
    }

    #[test]
    fn negation_rejects_non_variable_and_unknown_targets() {
        // NB: GRAMMAR_TEST_CONFIG's vocabulary is just doxy + pressure —
        // 'frobnicate' stands in for any base outside the dataset's
        // vocabulary (on the real ARGO_CONFIG, every ARGO_VARS base is
        // negatable). Matches the 2.x pattern, where ~ wraps the same
        // alternation of known names.
        for raw in ["~all", "~except-data-values", "~1", "~frobnicate", "~~doxy"] {
            let params = json!({ "data": raw });
            assert!(
                validate_data_param(&params, &GRAMMAR_TEST_CONFIG).is_err(),
                "'{}' should be rejected",
                raw
            );
        }
    }

    #[test]
    fn negation_does_not_satisfy_integer_pairing() {
        // A negation selects profiles; it is not a variable for integer
        // QC filters to act on. `data=~doxy,1` is still a QC filter with
        // nothing to filter.
        let params = json!({ "data": "~doxy,1" });
        let err = validate_data_param(&params, &GRAMMAR_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn negation_is_rejected_where_not_opted_in() {
        // Grid configs don't declare negation support; ~ tokens 400 even
        // on an otherwise-valid variable.
        let params = json!({ "data": "~THETA" });
        let err = validate_data_param(&params, &DATA_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn validate_data_param_accepts_mixed_list() {
        let params = json!({ "data": "all,THETA,1,SALT" });
        assert!(validate_data_param(&params, &DATA_TEST_CONFIG).is_ok());
    }

    #[test]
    fn validate_data_param_rejects_unknown_var() {
        let params = json!({ "data": "salinity" });
        let err = validate_data_param(&params, &DATA_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn validate_data_param_rejects_unknown_var_in_mixed_list() {
        // First token is fine, second is the bad one — should still
        // reject (loops over every token).
        let params = json!({ "data": "THETA,bogus" });
        let err = validate_data_param(&params, &DATA_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn validate_data_param_rejects_empty_token() {
        // Trailing or doubled commas yield an empty token in the
        // split — surface that as its own clear error rather than
        // bouncing through the suggestion path.
        for raw in ["THETA,", ",THETA", "THETA,,SALT"] {
            let params = json!({ "data": raw });
            let err = validate_data_param(&params, &DATA_TEST_CONFIG).unwrap_err();
            assert_eq!(err.status(), 400);
        }
    }

    #[test]
    fn unknown_data_token_message_includes_suggestion_when_close() {
        // The single typo here is in the case of the last char; edit
        // distance 1 → suggest THETA.
        let msg = unknown_data_token_message("Theta", &DATA_TEST_CONFIG);
        assert!(
            msg.contains("Did you mean 'THETA'"),
            "suggestion missing in: {}",
            msg
        );
    }

    #[test]
    fn unknown_data_token_message_omits_suggestion_when_far() {
        let msg = unknown_data_token_message("zzzzzzzzzzz", &DATA_TEST_CONFIG);
        assert!(
            !msg.contains("Did you mean"),
            "should not suggest a far-away name: {}",
            msg
        );
    }

    #[test]
    fn unknown_data_token_message_suggestion_is_case_insensitive() {
        // Fully-lowercased or fully-uppercased typos of a config'd var
        // should still produce a suggestion pointing at the correctly-
        // cased original. Without case-folding, every letter mismatch
        // would be a substitution and the distance would exceed the
        // threshold for variables like `THETA`.
        for typo in ["theta", "Theta", "ThEtA"] {
            let msg = unknown_data_token_message(typo, &DATA_TEST_CONFIG);
            assert!(
                msg.contains("Did you mean 'THETA'"),
                "expected THETA suggestion for '{}', got: {}",
                typo,
                msg
            );
        }
    }

    #[test]
    fn radius_cap_skipped_when_no_center() {
        // No center → mode doesn't apply, cap is moot.
        let params = json!({});
        assert!(validate_radius_cap(&params, &RADIUS_TEST_CONFIG).is_ok());
    }

    #[test]
    fn radius_cap_accepts_radius_at_exactly_the_cap() {
        // cap is 1_000_000 m = 1000 km; radius qsp is km
        let params = json!({"center": "0,0", "radius": "1000"});
        assert!(validate_radius_cap(&params, &RADIUS_TEST_CONFIG).is_ok());
    }

    #[test]
    fn radius_cap_accepts_radius_below_cap() {
        let params = json!({"center": "0,0", "radius": "500"});
        assert!(validate_radius_cap(&params, &RADIUS_TEST_CONFIG).is_ok());
    }

    #[test]
    fn radius_cap_rejects_radius_above_cap() {
        let params = json!({"center": "0,0", "radius": "1000.001"});
        let err = validate_radius_cap(&params, &RADIUS_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn radius_cap_rejects_non_numeric_radius() {
        let params = json!({"center": "0,0", "radius": "huge"});
        let err = validate_radius_cap(&params, &RADIUS_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn radius_cap_rejects_negative_radius() {
        let params = json!({"center": "0,0", "radius": "-1"});
        let err = validate_radius_cap(&params, &RADIUS_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn radius_cap_rejects_non_finite_radius() {
        let params = json!({"center": "0,0", "radius": "inf"});
        let err = validate_radius_cap(&params, &RADIUS_TEST_CONFIG).unwrap_err();
        assert_eq!(err.status(), 400);
    }
}