// Integration tests against a live API + MongoDB.
//
// Preconditions:
//   * `cargo run --bin seed_test_db` has been run against the same MongoDB
//     the API is connected to.
//   * The API has been (re)started AFTER the seed, so it caches the right
//     `timeseriesMeta.timeseries` vector at startup.
//   * API_URL points at the running API (default: http://localhost:8080).
//   * MONGODB_URI is reachable (default: mongodb://localhost:27017). It is
//     not used directly by these tests but is read so that misconfigured
//     environments fail loudly.
//
// Run with:
//   API_URL=http://localhost:8080 MONGODB_URI=mongodb://localhost:27017 \
//     cargo test --test integration -- --test-threads=1
//
// Tests are kept independent and read-only, so they could in principle run
// in parallel; we serialize them above just to keep ordering stable in CI
// logs.

mod common;

use common::url_with_query;
use serde_json::Value;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("reqwest client should build")
}

async fn get(path: &str, params: &[(&str, &str)]) -> reqwest::Response {
    let url = url_with_query(path, params);
    client()
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {} failed: {}", url, e))
}

// ---------------------------------------------------------------------------
// Basic shape & happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_filters_returns_all_seeded_documents() {
    // Without `data` set, slice_data drops the data field on each row but
    // keeps the rows themselves — so we should get 4 entries.
    let resp = get("/timeseries/bsose", &[]).await;
    assert_eq!(resp.status(), 200, "expected 200 OK with seeded DB");
    let body: Vec<Value> = resp.json().await.expect("body should be JSON array");
    assert_eq!(body.len(), 4, "expected all 4 seeded bsose docs");
    for row in &body {
        let data = row.get("data").expect("each row should have a data field");
        let outer = data.as_array().expect("data should be an array");
        assert!(
            outer.is_empty(),
            "data should be cleared when `data` query param is absent"
        );
    }
}

#[tokio::test]
async fn data_all_returns_full_timeseries() {
    let resp = get("/timeseries/bsose", &[("data", "all")]).await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 4);

    // Every row should have 2 variables × 4 timesteps.
    for row in &body {
        let outer = row["data"].as_array().expect("data array");
        assert_eq!(outer.len(), 2, "expected 2 variables per row");
        for inner in outer {
            assert_eq!(
                inner.as_array().unwrap().len(),
                4,
                "expected 4 timesteps per variable"
            );
        }
    }
}

#[tokio::test]
async fn data_specific_field_filters_columns() {
    let resp = get("/timeseries/bsose", &[("data", "salinity")]).await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = resp.json().await.unwrap();
    for row in &body {
        let names = &row["data_info"][0];
        assert_eq!(
            names.as_array().unwrap(),
            &vec![Value::String("salinity".to_string())]
        );
        assert_eq!(row["data"].as_array().unwrap().len(), 1);
    }
}

// ---------------------------------------------------------------------------
// id filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn id_filter_returns_single_document() {
    let resp = get(
        "/timeseries/bsose",
        &[("id", "bsose_doc_001"), ("data", "all")],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 1);
    assert_eq!(body[0]["_id"], "bsose_doc_001");
}

#[tokio::test]
async fn unknown_id_returns_404() {
    let resp = get("/timeseries/bsose", &[("id", "nope")]).await;
    assert_eq!(resp.status(), 404);
}

// ---------------------------------------------------------------------------
// verticalRange filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vertical_range_filters_by_level() {
    // levels in fixtures: 10, 10, 20, 50 — [0, 30) keeps the three with
    // level < 30.
    let resp = get(
        "/timeseries/bsose",
        &[("verticalRange", "[0, 30]"), ("data", "all")],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 3);
    for row in &body {
        let level = row["level"].as_f64().unwrap();
        assert!(level >= 0.0 && level < 30.0, "unexpected level: {}", level);
    }
}

// ---------------------------------------------------------------------------
// Geo filters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn box_filter_matches_seeded_points() {
    // Box covers (lon 15..45, lat 5..35) — should hit docs at (20,10) and
    // (40,30), which is doc_001, doc_002, doc_004 (doc_001 and doc_004
    // share coords but different levels).
    let resp = get(
        "/timeseries/bsose",
        &[("box", "[[15,5],[45,35]]"), ("data", "all")],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = body.iter().map(|r| r["_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"bsose_doc_001"), "ids: {:?}", ids);
    assert!(ids.contains(&"bsose_doc_002"), "ids: {:?}", ids);
    assert!(ids.contains(&"bsose_doc_004"), "ids: {:?}", ids);
    assert!(!ids.contains(&"bsose_doc_003"), "ids: {:?}", ids);
}

#[tokio::test]
async fn polygon_filter_matches_seeded_points() {
    // Polygon around (20, 10) — small square enclosing doc_001 / doc_004.
    let resp = get(
        "/timeseries/bsose",
        &[
            ("polygon", "[[15,5],[25,5],[25,15],[15,15],[15,5]]"),
            ("data", "all"),
        ],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = body.iter().map(|r| r["_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"bsose_doc_001"), "ids: {:?}", ids);
    assert!(ids.contains(&"bsose_doc_004"), "ids: {:?}", ids);
    assert!(!ids.contains(&"bsose_doc_002"));
}

#[tokio::test]
async fn center_radius_filter_matches_nearby_points() {
    // 5000 km radius around (20, 10) — should find doc_001/doc_004 and
    // possibly doc_002 (about 3100 km away). doc_003 sits on the other side
    // of the planet and should be excluded.
    let resp = get(
        "/timeseries/bsose",
        &[
            ("center", "[20.0, 10.0]"),
            ("radius", "5000000"), // 5000 km in metres
            ("data", "all"),
        ],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = body.iter().map(|r| r["_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"bsose_doc_001"), "ids: {:?}", ids);
    assert!(!ids.contains(&"bsose_doc_003"), "ids: {:?}", ids);
}

// ---------------------------------------------------------------------------
// Date range slicing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn date_range_slices_timeseries_columns() {
    // Seeded timeseries: Jan, Apr, Jul, Oct (2020). Asking for Apr → Sep
    // should keep Apr and Jul (end is exclusive, < Oct works too here).
    let resp = get(
        "/timeseries/bsose",
        &[
            ("id", "bsose_doc_001"),
            ("data", "all"),
            ("startDate", "2020-04-01T00:00:00Z"),
            ("endDate", "2020-09-01T00:00:00Z"),
        ],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 1);
    let outer = body[0]["data"].as_array().unwrap();
    for inner in outer {
        assert_eq!(
            inner.as_array().unwrap().len(),
            2,
            "Apr + Jul should be 2 timesteps"
        );
    }
    // The transformed `timeseries` field should also reflect the slice.
    let ts = body[0]["timeseries"].as_array().unwrap();
    assert_eq!(ts.len(), 2);
    assert!(ts[0].as_str().unwrap().starts_with("2020-04-15"));
    assert!(ts[1].as_str().unwrap().starts_with("2020-07-15"));
}

// ---------------------------------------------------------------------------
// compression=minimal & batchmeta
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compression_minimal_returns_stub_arrays() {
    let resp = get(
        "/timeseries/bsose",
        &[("compression", "minimal"), ("data", "all")],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = resp.json().await.unwrap();
    assert!(!body.is_empty());
    // Stubs serialize as 5-element arrays: [_id, lon, lat, level, metadata].
    for row in &body {
        let arr = row.as_array().expect("each stub should be an array");
        assert_eq!(arr.len(), 5);
        assert!(arr[0].is_string()); // _id
        assert!(arr[1].is_number()); // longitude
        assert!(arr[2].is_number()); // latitude
        assert!(arr[3].is_number()); // level
        assert!(arr[4].is_array()); // metadata
    }
}

#[tokio::test]
async fn batchmeta_returns_metadata_documents() {
    let resp = get(
        "/timeseries/bsose",
        &[("batchmeta", "true"), ("data", "all")],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = resp.json().await.unwrap();
    // Our seeded bsose docs all reference one meta doc.
    assert_eq!(body.len(), 1);
    assert_eq!(body[0]["_id"], "bsose-profile-meta-2020");
    assert_eq!(body[0]["data_type"], "BSOSE-profile");
}

// ---------------------------------------------------------------------------
// Validation errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rejects_box_and_polygon_together() {
    let resp = get(
        "/timeseries/bsose",
        &[
            ("box", "[[0,0],[10,10]]"),
            ("polygon", "[[0,0],[10,0],[10,10],[0,0]]"),
        ],
    )
    .await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn rejects_center_without_radius() {
    let resp = get("/timeseries/bsose", &[("center", "[0,0]")]).await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn rejects_malformed_polygon() {
    let resp = get(
        "/timeseries/bsose",
        &[("polygon", "[[0,0],[1,0],[0,0]]")], // < 4 points
    )
    .await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn rejects_unparseable_start_date() {
    let resp = get("/timeseries/bsose", &[("startDate", "yesterday")]).await;
    assert_eq!(resp.status(), 400);
}
