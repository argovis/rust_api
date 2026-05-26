//! Pure helpers for tile-based pagination URLs and query-param parsing.
//!
//! This module is intentionally tiny and free of MongoDB/Actix dependencies
//! so that it can be unit-tested without standing up a server or a DB. The
//! handler in `main.rs` consumes these helpers when it needs to (a) read
//! the requested tile index off the query, and (b) build the next page's
//! URL after a successful serve.

use serde_json::Value;

/// Read `tile_index` from the query params.
///
/// Returns `Ok(0)` if the param is absent (default behaviour: start at the
/// first tile). Returns `Err(...)` if the param is present but doesn't
/// parse as a non-negative integer — the handler turns this into a 400.
///
/// Why strict-on-present-but-invalid: tile_index is almost always supplied
/// by a previous response's `next_url`, so a malformed value is a client
/// bug worth surfacing rather than silently restarting pagination at zero.
pub fn parse_tile_index(params: &Value) -> Result<usize, String> {
    match params.get("tile_index") {
        None => Ok(0),
        Some(v) => v
            .as_str()
            .ok_or_else(|| "tile_index must be a string-encoded integer".to_string())
            .and_then(|s| {
                s.parse::<usize>()
                    .map_err(|_| format!("tile_index '{}' is not a non-negative integer", s))
            }),
    }
}

/// Build the URL for the next page of a paginated request.
///
/// The result preserves every query param from the current request except
/// `tile_index`, which is overwritten with `next_index`. Values are
/// percent-encoded so that polygon/box payloads (which contain `[`, `]`,
/// `,`) round-trip cleanly. The output is a path + query string with no
/// scheme/host — clients are expected to resolve it against the original
/// request's origin.
///
/// Param order in the output is alphabetical, not insertion-order. This is
/// intentional: it makes the URL deterministic for a given (path, params,
/// next_index) tuple, which makes tests easy to write and caches behave
/// predictably.
pub fn build_next_url(path: &str, params: &Value, next_index: usize) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();

    if let Some(obj) = params.as_object() {
        for (k, v) in obj {
            if k == "tile_index" {
                // The caller's tile_index gets overwritten with next_index
                // below. Skipping it here also handles the case where the
                // caller passed tile_index=N and we now serve tile N+M.
                continue;
            }
            // All Actix query params arrive as strings, but a non-string
            // value would just serialize via Display. Cheap and forgiving.
            let s = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            pairs.push((k.clone(), s));
        }
    }
    pairs.push(("tile_index".to_string(), next_index.to_string()));

    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let query: String = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    if query.is_empty() {
        path.to_string()
    } else {
        format!("{}?{}", path, query)
    }
}

/// Minimal RFC 3986 unreserved-set percent-encoder.
///
/// We could pull in `percent-encoding` or `urlencoding` for this, but it's
/// 15 lines and we don't want a new crate dependency for one helper. The
/// unreserved set (A-Z a-z 0-9 - . _ ~) passes through; everything else is
/// encoded as `%XX` of its UTF-8 byte representation.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- parse_tile_index ---------------------------------------------------

    #[test]
    fn parse_tile_index_defaults_to_zero_when_absent() {
        assert_eq!(parse_tile_index(&json!({})).unwrap(), 0);
    }

    #[test]
    fn parse_tile_index_parses_string_integer() {
        assert_eq!(parse_tile_index(&json!({"tile_index": "42"})).unwrap(), 42);
    }

    #[test]
    fn parse_tile_index_zero_is_allowed() {
        assert_eq!(parse_tile_index(&json!({"tile_index": "0"})).unwrap(), 0);
    }

    #[test]
    fn parse_tile_index_rejects_non_numeric() {
        assert!(parse_tile_index(&json!({"tile_index": "abc"})).is_err());
    }

    #[test]
    fn parse_tile_index_rejects_negative() {
        // usize::parse rejects negatives, so this falls through the same
        // error path as garbage input.
        assert!(parse_tile_index(&json!({"tile_index": "-1"})).is_err());
    }

    // ---- build_next_url -----------------------------------------------------

    #[test]
    fn build_next_url_just_tile_index_when_no_other_params() {
        let out = build_next_url("/timeseries/bsose", &json!({}), 1);
        assert_eq!(out, "/timeseries/bsose?tile_index=1");
    }

    #[test]
    fn build_next_url_preserves_other_params() {
        let out = build_next_url(
            "/timeseries/bsose",
            &json!({"data": "all"}),
            3,
        );
        assert_eq!(out, "/timeseries/bsose?data=all&tile_index=3");
    }

    #[test]
    fn build_next_url_overwrites_existing_tile_index() {
        let out = build_next_url(
            "/timeseries/bsose",
            &json!({"data": "all", "tile_index": "0"}),
            5,
        );
        // The 0 should not appear; the new value is 5.
        assert_eq!(out, "/timeseries/bsose?data=all&tile_index=5");
    }

    #[test]
    fn build_next_url_percent_encodes_brackets_and_commas() {
        // Box payloads contain `[`, `]`, `,` — all reserved characters.
        let out = build_next_url(
            "/timeseries/bsose",
            &json!({"box": "[[0,0],[10,10]]"}),
            1,
        );
        assert!(
            out.contains("box=%5B%5B0%2C0%5D%2C%5B10%2C10%5D%5D"),
            "expected percent-encoded brackets and commas, got: {}",
            out
        );
    }

    #[test]
    fn build_next_url_sorts_params_alphabetically() {
        let out = build_next_url(
            "/timeseries/bsose",
            &json!({"data": "all", "id": "doc1"}),
            7,
        );
        // Alphabetical: data, id, tile_index.
        assert_eq!(out, "/timeseries/bsose?data=all&id=doc1&tile_index=7");
    }

    #[test]
    fn build_next_url_handles_iso_dates() {
        // ISO 8601 dates contain `:` which is reserved. Encoder should
        // turn it into %3A.
        let out = build_next_url(
            "/timeseries/bsose",
            &json!({"startDate": "2020-01-01T00:00:00Z"}),
            0,
        );
        assert!(out.contains("startDate=2020-01-01T00%3A00%3A00Z"), "got: {}", out);
    }

    #[test]
    fn url_encode_passes_through_unreserved_set() {
        assert_eq!(url_encode("abcXYZ-._~0123"), "abcXYZ-._~0123");
    }

    #[test]
    fn url_encode_handles_multibyte_utf8() {
        // The percent-encoding spec operates on UTF-8 bytes. A single
        // emoji is 4 bytes → 4 %XX groups.
        let out = url_encode("🐱");
        assert_eq!(out.len(), 12); // 4 bytes × 3 chars per byte
        assert!(out.starts_with("%"));
    }
}
