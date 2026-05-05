// Shared helpers for integration tests.
//
// Reads API_URL and MONGODB_URI from the environment with localhost defaults.
// Tests assume the API is already running and that `seed_test_db` has been
// executed before the API started — the API caches the timeseries metadata
// at startup, so re-seeding mid-suite would not refresh that cache.

use std::env;

pub fn api_url() -> String {
    env::var("API_URL").unwrap_or_else(|_| "http://localhost:8080".to_string())
}

pub fn mongodb_uri() -> String {
    env::var("MONGODB_URI").unwrap_or_else(|_| "mongodb://localhost:27017".to_string())
}

/// Build a query URL: `{api_url}{path}?{key1}={val1}&...`
pub fn url_with_query(path: &str, params: &[(&str, &str)]) -> String {
    let qs = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    if qs.is_empty() {
        format!("{}{}", api_url(), path)
    } else {
        format!("{}{}?{}", api_url(), path, qs)
    }
}

fn urlencode(s: &str) -> String {
    // Minimal percent-encoding for the characters likely to appear in our
    // query strings: brackets, commas, quotes, spaces, colons.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
