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
// Every response is the paginated envelope:
//   { "docs": [...], "next_url": "<rel path?…>" | null, "message": "..." }
// Multi-tile queries (anything spanning multiple grid cells or vertical
// levels) span multiple pages — use `get_paged` to follow `next_url` and
// accumulate docs across pages. See api/PAGINATION.md for the full
// contract.

mod common;

use common::url_with_query;
use serde_json::Value;

/// Generous timeout: the naive plod-forward through empty tiles can take a
/// few seconds on the first page of a whole-globe request, even with the
/// tiny seeded corpus. We can tighten this once we have a land-mask shortcut.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
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

/// One paginated response: assert 200, parse as Value, return the body.
async fn get_envelope(path: &str, params: &[(&str, &str)]) -> Value {
    let resp = get(path, params).await;
    assert_eq!(resp.status(), 200, "expected 200 OK from {}", path);
    resp.json().await.expect("response should be JSON")
}

/// Follow `next_url` across pages, accumulating every doc returned. Stops
/// once a page returns `next_url: null`. The relative `next_url` is
/// resolved against `API_URL`.
async fn get_paged(path: &str, params: &[(&str, &str)]) -> Vec<Value> {
    let mut all_docs: Vec<Value> = Vec::new();
    let mut url = url_with_query(path, params);

    loop {
        let resp = client()
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {} failed: {}", url, e));
        assert_eq!(
            resp.status(),
            200,
            "expected 200, got {} for {}",
            resp.status(),
            url
        );
        let body: Value = resp.json().await.expect("response should be JSON");

        let docs = body["docs"]
            .as_array()
            .expect("response.docs should be an array");
        all_docs.extend(docs.iter().cloned());

        match body["next_url"].as_str() {
            // `null` next_url means we've reached the end. (`as_str()` returns
            // None for both Value::Null and missing keys; both should
            // terminate.)
            None => break,
            Some(rel) => {
                url = format!("{}{}", common::api_url(), rel);
            }
        }
    }

    all_docs
}

// ---------------------------------------------------------------------------
// Basic shape & happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_filters_returns_all_seeded_documents_across_pages() {
    // Whole-globe queries paginate through many tiles. After walking the
    // full sequence we should get every seeded doc back exactly once.
    let docs = get_paged("/timeseries/bsose", &[]).await;
    assert_eq!(docs.len(), 4, "expected all 4 seeded docs across pages");

    // Without `data` set, slice_data clears the data field but keeps rows.
    for row in &docs {
        let data = row.get("data").expect("each row should have a data field");
        let outer = data.as_array().expect("data should be an array");
        assert!(
            outer.is_empty(),
            "data should be cleared when `data` query param is absent"
        );
    }
}

#[tokio::test]
async fn data_all_returns_full_timeseries_across_pages() {
    let docs = get_paged("/timeseries/bsose", &[("data", "all")]).await;
    assert_eq!(docs.len(), 4);

    for row in &docs {
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
async fn data_specific_field_filters_columns_across_pages() {
    let docs = get_paged("/timeseries/bsose", &[("data", "salinity")]).await;
    assert!(!docs.is_empty());
    for row in &docs {
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
    // id lookups produce a single passthrough tile, so the response fits in
    // one page with a null next_url.
    let body = get_envelope(
        "/timeseries/bsose",
        &[("id", "bsose_doc_001"), ("data", "all")],
    )
    .await;
    let docs = body["docs"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["_id"], "bsose_doc_001");
    assert!(
        body["next_url"].is_null(),
        "single-page result should report no further pages"
    );
}

#[tokio::test]
async fn unknown_id_returns_empty_docs_and_null_next_url() {
    // Pre-pagination this returned 404. The paginated contract is that
    // empty results return 200 + empty docs + null next_url.
    let body = get_envelope("/timeseries/bsose", &[("id", "nope")]).await;
    let docs = body["docs"].as_array().unwrap();
    assert!(docs.is_empty(), "no docs for an unknown id");
    assert!(body["next_url"].is_null(), "no further pages either");
}

// ---------------------------------------------------------------------------
// verticalRange filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vertical_range_filters_by_level_across_pages() {
    // levels in fixtures: 10, 10, 20, 50 — [0, 30) keeps the three with
    // level < 30. Pagination iterates the tile×level grid; verticalRange
    // restricts which level brackets contribute docs.
    let docs = get_paged(
        "/timeseries/bsose",
        &[("verticalRange", "[0, 30]"), ("data", "all")],
    )
    .await;
    assert_eq!(docs.len(), 3);
    for row in &docs {
        let level = row["level"].as_f64().unwrap();
        assert!(level >= 0.0 && level < 30.0, "unexpected level: {}", level);
    }
}

// ---------------------------------------------------------------------------
// Geo filters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn box_filter_matches_seeded_points_across_pages() {
    // Box covers (lon 15..45, lat 5..35) — should hit docs at (20,10) and
    // (40,30), which is doc_001, doc_002, doc_004 (doc_001 and doc_004
    // share coords but different levels — they land in different
    // level-brackets, so they show up on different pages).
    let docs = get_paged(
        "/timeseries/bsose",
        &[("box", "[[15,5],[45,35]]"), ("data", "all")],
    )
    .await;
    let ids: Vec<&str> = docs.iter().map(|r| r["_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"bsose_doc_001"), "ids: {:?}", ids);
    assert!(ids.contains(&"bsose_doc_002"), "ids: {:?}", ids);
    assert!(ids.contains(&"bsose_doc_004"), "ids: {:?}", ids);
    assert!(!ids.contains(&"bsose_doc_003"), "ids: {:?}", ids);
}

#[tokio::test]
async fn polygon_filter_matches_seeded_points_across_pages() {
    // Polygon around (20, 10) — small square enclosing doc_001 / doc_004.
    // The polygon's bbox spans four spatial tiles ([10-20, 0-10],
    // [20-30, 0-10], [10-20, 10-20], [20-30, 10-20]) — multi-tile case.
    // doc_001 (level 10 → L0) and doc_004 (level 50 → L3) share the same
    // spatial tile but land in different level pages, so we expect
    // exactly 2 docs across 2 non-empty pages.
    let docs = get_paged(
        "/timeseries/bsose",
        &[
            ("polygon", "[[15,5],[25,5],[25,15],[15,15],[15,5]]"),
            ("data", "all"),
        ],
    )
    .await;
    let ids: Vec<&str> = docs.iter().map(|r| r["_id"].as_str().unwrap()).collect();
    assert_eq!(docs.len(), 2, "expected exactly 2 docs, got {:?}", ids);
    assert!(ids.contains(&"bsose_doc_001"), "ids: {:?}", ids);
    assert!(ids.contains(&"bsose_doc_004"), "ids: {:?}", ids);
    assert!(!ids.contains(&"bsose_doc_002"));
}

#[tokio::test]
async fn box_crossing_dateline_finds_antimeridian_docs() {
    // Dateline-crossing box: sw_lon (170) > ne_lon (-160), so the box
    // wraps the antimeridian. Tile generation splits it into an eastern
    // sub-box (170..180) and a western sub-box (-180..-160). doc_003 at
    // (-170, 50) lives in the western band; the other seeded docs are
    // far from this box and should be excluded.
    let docs = get_paged(
        "/timeseries/bsose",
        &[("box", "[[170,40],[-160,60]]"), ("data", "all")],
    )
    .await;
    let ids: Vec<&str> = docs.iter().map(|r| r["_id"].as_str().unwrap()).collect();
    assert_eq!(
        docs.len(),
        1,
        "expected exactly one doc (doc_003), got {:?}",
        ids
    );
    assert!(ids.contains(&"bsose_doc_003"), "ids: {:?}", ids);
    assert!(!ids.contains(&"bsose_doc_001"));
    assert!(!ids.contains(&"bsose_doc_002"));
    assert!(!ids.contains(&"bsose_doc_004"));
}

#[tokio::test]
async fn polygon_crossing_antimeridian_finds_seeded_doc() {
    // Polygon straddles the dateline: vertices at (170, 45), (-160, 45),
    // (-160, 55), (170, 55). The tile generator should detect the
    // antimeridian crossing and emit east + west sub-bboxes covering the
    // narrow band rather than the naive 330°-wide bbox.
    //
    // doc_003 at (-170, 50) sits inside the western piece. The other
    // seeded docs are far from this band and should be excluded.
    let docs = get_paged(
        "/timeseries/bsose",
        &[
            ("polygon", "[[170,45],[-160,45],[-160,55],[170,55],[170,45]]"),
            ("data", "all"),
        ],
    )
    .await;
    let ids: Vec<&str> = docs.iter().map(|r| r["_id"].as_str().unwrap()).collect();
    assert_eq!(
        docs.len(),
        1,
        "expected exactly doc_003 in antimeridian polygon, got {:?}",
        ids
    );
    assert!(ids.contains(&"bsose_doc_003"), "ids: {:?}", ids);
}

#[tokio::test]
async fn center_radius_filter_matches_nearby_points_across_pages() {
    // 100 km radius around (20, 10) — at the BSOSE radius cap.
    // center+radius gets level-only pagination (no spatial tiling), so
    // we still need to walk pages to hit each level bracket that
    // contains data. doc_001 / doc_004 sit exactly at the center so any
    // positive radius catches them; doc_003 is on the other side of the
    // planet and is excluded by any sane radius.
    let docs = get_paged(
        "/timeseries/bsose",
        &[
            ("center", "[20.0, 10.0]"),
            ("radius", "100000"), // 100 km — at the cap
            ("data", "all"),
        ],
    )
    .await;
    let ids: Vec<&str> = docs.iter().map(|r| r["_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"bsose_doc_001"), "ids: {:?}", ids);
    assert!(!ids.contains(&"bsose_doc_003"), "ids: {:?}", ids);
}

// ---------------------------------------------------------------------------
// Date range slicing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn date_range_slices_timeseries_columns() {
    // Seeded timeseries: Jan, Apr, Jul, Oct (2020). Asking for Apr → Sep
    // should keep Apr and Jul (end is exclusive). id lookup is single-page.
    let body = get_envelope(
        "/timeseries/bsose",
        &[
            ("id", "bsose_doc_001"),
            ("data", "all"),
            ("startDate", "2020-04-01T00:00:00Z"),
            ("endDate", "2020-09-01T00:00:00Z"),
        ],
    )
    .await;
    let docs = body["docs"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    let outer = docs[0]["data"].as_array().unwrap();
    for inner in outer {
        assert_eq!(
            inner.as_array().unwrap().len(),
            2,
            "Apr + Jul should be 2 timesteps"
        );
    }
    let ts = docs[0]["timeseries"].as_array().unwrap();
    assert_eq!(ts.len(), 2);
    assert!(ts[0].as_str().unwrap().starts_with("2020-04-15"));
    assert!(ts[1].as_str().unwrap().starts_with("2020-07-15"));
}

// ---------------------------------------------------------------------------
// compression=minimal & batchmeta
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compression_minimal_returns_stub_arrays_across_pages() {
    let docs = get_paged(
        "/timeseries/bsose",
        &[("compression", "minimal"), ("data", "all")],
    )
    .await;
    // The 4 seeded docs live in 4 distinct (spatial, level) tiles, so
    // pagination should yield exactly 4 stubs total.
    assert_eq!(docs.len(), 4, "expected one stub per seeded doc, got {:?}", docs);
    // Stubs serialize as 5-element arrays: [_id, lon, lat, level, metadata].
    for row in &docs {
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
async fn compression_minimal_with_id_returns_single_page_stub() {
    // id lookup produces a passthrough tile; combining it with minimal
    // should yield exactly one stub in a single-page response.
    let body = get_envelope(
        "/timeseries/bsose",
        &[
            ("id", "bsose_doc_001"),
            ("compression", "minimal"),
            ("data", "all"),
        ],
    )
    .await;
    let docs = body["docs"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    let stub = docs[0].as_array().expect("minimal response is a stub array");
    assert_eq!(stub.len(), 5);
    assert_eq!(stub[0].as_str().unwrap(), "bsose_doc_001");
    assert!(body["next_url"].is_null(), "id lookups don't paginate further");
}

#[tokio::test]
async fn batchmeta_with_id_returns_single_page_metadata() {
    // id + batchmeta also passthrough-tiled: one metadata doc, single page.
    let body = get_envelope(
        "/timeseries/bsose",
        &[
            ("id", "bsose_doc_001"),
            ("batchmeta", "true"),
            ("data", "all"),
        ],
    )
    .await;
    let docs = body["docs"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["_id"], "bsose-profile-meta-2020");
    assert_eq!(docs[0]["data_type"], "BSOSE-profile");
    assert!(body["next_url"].is_null());
}

#[tokio::test]
async fn batchmeta_takes_precedence_over_minimal() {
    // The handler dispatches into the batchmeta branch before the
    // streaming branch consults compression=minimal. With both set,
    // batchmeta wins and the returned docs are metadata objects, not
    // 5-element stubs.
    let body = get_envelope(
        "/timeseries/bsose",
        &[
            ("id", "bsose_doc_001"),
            ("batchmeta", "true"),
            ("compression", "minimal"),
            ("data", "all"),
        ],
    )
    .await;
    let docs = body["docs"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    // A metadata doc is a JSON object with a data_type field; a stub is
    // a 5-element array. Verify we got the object form.
    let first = &docs[0];
    assert!(
        first.is_object(),
        "expected metadata object when batchmeta is set, got {:?}",
        first
    );
    assert_eq!(first["data_type"], "BSOSE-profile");
}

#[tokio::test]
async fn batchmeta_returns_metadata_documents_across_pages() {
    // batchmeta aggregates per-page: each non-empty (spatial, level) tile
    // returns the metadata docs referenced by that tile's bsose docs.
    // Our 4 seeded docs each live in their own tile and all reference the
    // same metadata id, so we get 4 page-level metadata responses each
    // containing that one metadata doc — 4 returned docs, 1 unique id.
    let docs = get_paged(
        "/timeseries/bsose",
        &[("batchmeta", "true"), ("data", "all")],
    )
    .await;
    assert_eq!(
        docs.len(),
        4,
        "expected one batchmeta response per non-empty tile, got {:?}",
        docs
    );

    let mut unique_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for d in &docs {
        unique_ids.insert(d["_id"].as_str().unwrap().to_string());
    }
    assert_eq!(unique_ids.len(), 1, "all seeded docs share one metadata id");
    assert!(unique_ids.contains("bsose-profile-meta-2020"));
    // Every returned doc should be a metadata document, not a bsose doc.
    for d in &docs {
        assert_eq!(d["data_type"], "BSOSE-profile");
    }
}

// ---------------------------------------------------------------------------
// Pagination protocol
// ---------------------------------------------------------------------------

#[tokio::test]
async fn polygon_over_empty_region_returns_empty_envelope() {
    // Polygon in the Indian Ocean (60-70°E, 5-15°N) — far from any
    // seeded doc. Probe-forward should walk every candidate tile, find
    // none non-empty, and return a 200 envelope with an empty docs array
    // and null next_url instead of 404 or any other error code.
    let body = get_envelope(
        "/timeseries/bsose",
        &[
            ("polygon", "[[60,5],[70,5],[70,15],[60,15],[60,5]]"),
            ("data", "all"),
        ],
    )
    .await;
    let docs = body["docs"].as_array().unwrap();
    assert!(
        docs.is_empty(),
        "expected no docs in empty region, got {:?}",
        docs
    );
    assert!(body["next_url"].is_null());
}

#[tokio::test]
async fn tile_index_beyond_end_returns_empty_with_null_next_url() {
    // Tile sequence for a small box is short; an absurdly large tile_index
    // is past the end. The server should return 200 + empty docs +
    // null next_url, not an error.
    let body = get_envelope(
        "/timeseries/bsose",
        &[
            ("box", "[[0,0],[10,10]]"),
            ("tile_index", "9999999"),
        ],
    )
    .await;
    let docs = body["docs"].as_array().unwrap();
    assert!(docs.is_empty());
    assert!(body["next_url"].is_null());
}

#[tokio::test]
async fn invalid_tile_index_returns_400() {
    let resp = get(
        "/timeseries/bsose",
        &[("box", "[[0,0],[10,10]]"), ("tile_index", "not-a-number")],
    )
    .await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn negative_tile_index_returns_400() {
    let resp = get(
        "/timeseries/bsose",
        &[("box", "[[0,0],[10,10]]"), ("tile_index", "-1")],
    )
    .await;
    assert_eq!(resp.status(), 400);
}

/// Pull `tile_index` out of a next_url query string. Panics if absent —
/// only used in tests where the URL was just emitted by the server, so a
/// missing index is itself a bug worth surfacing.
fn tile_index_from(url: &str) -> usize {
    let query = url.split('?').nth(1).unwrap_or("");
    for pair in query.split('&') {
        if let Some(("tile_index", v)) = pair.split_once('=') {
            return v
                .parse()
                .unwrap_or_else(|e| panic!("tile_index in {} failed to parse: {}", url, e));
        }
    }
    panic!("no tile_index in url: {}", url);
}

#[tokio::test]
async fn next_url_round_trips_cleanly() {
    // Issue a multi-page request, GET its next_url directly (not via
    // get_paged), and verify the server returns a valid envelope and the
    // tile_index has advanced. Confirms that build_next_url's output
    // survives the round-trip through the URL parser back into the
    // handler — catches percent-encoding bugs, param dropping, etc.
    let body = get_envelope(
        "/timeseries/bsose",
        &[("box", "[[15,5],[45,35]]"), ("data", "all")],
    )
    .await;
    let next = body["next_url"]
        .as_str()
        .expect("first page of multi-tile request should advertise next_url");
    let initial_idx = tile_index_from(next);

    let url = format!("{}{}", common::api_url(), next);
    let resp = client()
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {} failed: {}", url, e));
    assert_eq!(resp.status(), 200, "next_url should yield 200");
    let next_body: Value = resp.json().await.expect("response should be JSON");

    // Envelope shape preserved.
    assert!(next_body["docs"].is_array());
    assert!(next_body["message"].is_string());

    // If there are still more pages after this one, the new next_url
    // should reference a tile_index strictly greater than the one we just
    // requested (server probed forward to find a non-empty tile).
    if let Some(further) = next_body["next_url"].as_str() {
        let further_idx = tile_index_from(further);
        assert!(
            further_idx > initial_idx,
            "further next_url tile_index ({}) should advance past {}",
            further_idx,
            initial_idx
        );
    }
}

#[tokio::test]
async fn first_page_carries_a_next_url_when_more_pages_remain() {
    // The (20,10)/(40,30) box has docs at multiple level brackets, so the
    // first page should not be the last. next_url must carry both the
    // user's params (so the next request hits the same filter) and an
    // advanced tile_index.
    let body = get_envelope(
        "/timeseries/bsose",
        &[("box", "[[15,5],[45,35]]"), ("data", "all")],
    )
    .await;
    assert!(
        body["next_url"].is_string(),
        "first page of a multi-tile request should advertise next_url"
    );
    let next = body["next_url"].as_str().unwrap();
    assert!(next.contains("tile_index="), "next_url: {}", next);
    assert!(next.contains("box="), "next_url should preserve box param: {}", next);
    assert!(next.contains("data="), "next_url should preserve data param: {}", next);
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

#[tokio::test]
async fn rejects_radius_above_cap() {
    // BSOSE_CONFIG.max_radius_meters is 5_000_000. Asking for 10_000_000
    // should be rejected before the cursor opens.
    let resp = get(
        "/timeseries/bsose",
        &[("center", "[0.0, 0.0]"), ("radius", "10000000")],
    )
    .await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn rejects_non_numeric_radius() {
    let resp = get(
        "/timeseries/bsose",
        &[("center", "[0.0, 0.0]"), ("radius", "huge")],
    )
    .await;
    assert_eq!(resp.status(), 400);
}
