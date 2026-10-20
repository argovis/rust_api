/*
todo:

demo critical
 -- done --

production critical
rate limiting
unit testing
legacy search parameters: mostrecent

nice to have someday
transform logic as traits?
*/

use api::helpers::transforms;
use api::helpers::schema;
use api::helpers::helpers;
use api::helpers::dataset_config;
use api::helpers::tile_generator;
use api::helpers::filter_composer;
use api::helpers::pagination;

use mongodb::{options::FindOptions, bson::Document, error::Result};
use actix_web::{get, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use once_cell::sync::Lazy;
use std::sync::Mutex;
use futures::stream::StreamExt;
use std::env;
use std::convert::Infallible;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use async_stream::stream;
use serde_json::{json, Value};

use dataset_config::{DatasetConfig, DatasetSource, ProfileSource};

// Per-dataset source-of-truth: identity strings + the dataset's Mongo
// client + values loaded from the dataset's meta doc at startup. The
// `Lazy<Mutex<Option<T>>>` shape is what lets us declare a static that's
// initialized once, after main() connects to Mongo and reads the meta
// doc. Handlers clone the inner DatasetSource out of the lock at the top
// of each request and use it locally, so the mutex is never held across
// `.await`.
//
// One static per dataset. A dataset is "enabled" iff its
// `MONGODB_URI_<DATASET>` env var is set; if so, main() loads it and
// fills this static, and the route handler is registered on the App.
// If the env var is unset, the static stays `None` and the handler is
// never registered (no route, no surprises).
//
// Adding a new dataset is one more static here plus one more
// load-and-register block in main() and a route handler above.
static BSOSE_SOURCE: Lazy<Mutex<Option<DatasetSource>>> = Lazy::new(|| Mutex::new(None));
static OISST_SOURCE: Lazy<Mutex<Option<DatasetSource>>> = Lazy::new(|| Mutex::new(None));
static COPERNICUSSLA_SOURCE: Lazy<Mutex<Option<DatasetSource>>> = Lazy::new(|| Mutex::new(None));
static CCMPWIND_SOURCE: Lazy<Mutex<Option<DatasetSource>>> = Lazy::new(|| Mutex::new(None));
// The profile family's sources use the family's own struct (slab-start
// cache instead of time axis + data_info) but the identical
// Lazy<Mutex<Option<_>>> idiom.
static ARGO_SOURCE: Lazy<Mutex<Option<ProfileSource>>> = Lazy::new(|| Mutex::new(None));

// ---- route handlers --------------------------------------------------------
//
// Each dataset gets its own one-route handler that resolves the dataset
// generic and hands off to `serve_timeseries`. Keeps Actix's `#[get(...)]`
// attribute discoverable per-dataset and lets the routing table grow
// without touching the generic core. Adding a new dataset is: define its
// `*_CONFIG` in `dataset_config.rs`, define its schema + meta in
// `schema.rs`, add a `*_SOURCE` static + a `load_dataset_source` call
// in `main()`, and add a 4-line handler here.

#[get("/timeseries/bsose")]
async fn bsose_handler(
    req: HttpRequest,
    query_params: web::Query<serde_json::Value>,
) -> impl Responder {
    // Snapshot the source out of the lock before any `.await`. Holding
    // the mutex guard across an await would let other tasks on the same
    // worker deadlock on it, so the idiom is "lock, clone, unlock, then
    // do async work with the local copy."
    let source = BSOSE_SOURCE
        .lock()
        .unwrap()
        .as_ref()
        .expect("BSOSE_SOURCE not initialized at startup")
        .clone();

    serve_timeseries::<schema::BsoseSchema>(
        req,
        query_params.into_inner(),
        &dataset_config::BSOSE_CONFIG,
        &source,
    )
    .await
}

#[get("/timeseries/noaaoisst")]
async fn oisst_handler(
    req: HttpRequest,
    query_params: web::Query<serde_json::Value>,
) -> impl Responder {
    let source = OISST_SOURCE
        .lock()
        .unwrap()
        .as_ref()
        .expect("OISST_SOURCE not initialized at startup")
        .clone();

    serve_timeseries::<schema::OisstSchema>(
        req,
        query_params.into_inner(),
        &dataset_config::OISST_CONFIG,
        &source,
    )
    .await
}

#[get("/timeseries/copernicussla")]
async fn copernicussla_handler(
    req: HttpRequest,
    query_params: web::Query<serde_json::Value>,
) -> impl Responder {
    let source = COPERNICUSSLA_SOURCE
        .lock()
        .unwrap()
        .as_ref()
        .expect("COPERNICUSSLA_SOURCE not initialized at startup")
        .clone();

    serve_timeseries::<schema::CopernicusSlaSchema>(
        req,
        query_params.into_inner(),
        &dataset_config::COPERNICUSSLA_CONFIG,
        &source,
    )
    .await
}

#[get("/timeseries/ccmpwind")]
async fn ccmpwind_handler(
    req: HttpRequest,
    query_params: web::Query<serde_json::Value>,
) -> impl Responder {
    let source = CCMPWIND_SOURCE
        .lock()
        .unwrap()
        .as_ref()
        .expect("CCMPWIND_SOURCE not initialized at startup")
        .clone();

    serve_timeseries::<schema::CcmpWindSchema>(
        req,
        query_params.into_inner(),
        &dataset_config::CCMPWIND_CONFIG,
        &source,
    )
    .await
}

#[get("/argo")]
async fn argo_handler(
    req: HttpRequest,
    query_params: web::Query<serde_json::Value>,
) -> impl Responder {
    let source = ARGO_SOURCE
        .lock()
        .unwrap()
        .as_ref()
        .expect("ARGO_SOURCE not initialized at startup")
        .clone();

    serve_profile::<schema::ArgoSchema>(
        req,
        query_params.into_inner(),
        &dataset_config::ARGO_CONFIG,
        &source,
    )
    .await
}

// ---- generic timeseries handler --------------------------------------------

/// Generic body of the `/timeseries/{dataset}` endpoint. Parameterized by
/// `S`, the per-dataset data-doc schema (e.g. `BsoseSchema`). The dataset's
/// request-size policy comes in as `config` (tile size, level set, radius
/// cap, coverage); its Mongo identity and startup caches come in as
/// `source` (db/collection names, the cached `timeseries` axis and the
/// cached meta-level `data_info` default).
///
/// Behaviour is the same as the previous BSOSE-specific handler: validate
/// the query, generate the tile sequence, then probe-forward through
/// tiles serving at most one non-empty tile per request. The two branches
/// (streaming docs vs. `batchmeta` metadata lookup) are unchanged from
/// the original implementation.
async fn serve_timeseries<S>(
    req: HttpRequest,
    params: serde_json::Value,
    config: &DatasetConfig,
    source: &DatasetSource,
) -> HttpResponse
where
    S: schema::IsTimeseries
        + DeserializeOwned
        + Serialize
        + Send
        + Sync
        + Unpin
        + 'static,
{
    // The next_url we emit on success uses this request's own path, so the
    // generated URL stays correct even if the route is re-mounted later.
    let path = req.path().to_string();

    // ---- validation ---------------------------------------------------
    if let Err(response) = helpers::validate_query_params(&params) {
        return response;
    }
    if let Err(response) = helpers::validate_radius_cap(&params, config) {
        return response;
    }
    if let Err(response) = helpers::validate_data_param(&params, config) {
        return response;
    }

    let start_idx = match pagination::parse_tile_index(&params) {
        Ok(i) => i,
        Err(e) => return HttpResponse::BadRequest().json(json!({"error": e})),
    };

    // ---- tile sequence + startup-loaded data --------------------------
    let tiles = tile_generator::generate_tiles(&params, config);

    // Clone the cached values out of `source` locally so the streaming
    // branch can move owned copies into its async generator without
    // juggling lifetimes — both are small (timeseries is a few KB of
    // dates; data_info is a few strings), so cloning per request is
    // cheap relative to the actual query work.
    let timeseries = source.timeseries.clone();
    let cached_data_info = source.data_info.clone();

    let compression: Option<String> = params
        .get("compression")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let batchmeta: Option<String> = params
        .get("batchmeta")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let is_minimal = matches!(compression.as_deref(), Some("minimal"));

    // ---- probe-forward loop -------------------------------------------
    //
    // For each candidate tile (starting at the caller-supplied tile_index),
    // open a cursor and look for output. The flavour of "look" differs by
    // branch: streaming peeks for the first doc that survives transformation
    // (and keeps the cursor so the rest can be streamed straight through);
    // batchmeta drains the entire cursor to collect unique metadata IDs
    // (no streaming, but per-tile bounded by tile size). Either way, an
    // empty tile drops through to the next iteration. We plod through
    // empty tiles one at a time; a future land-mask short-circuit could
    // replace this with a smarter skip.
    for tile_idx in start_idx..tiles.len() {
        let tile = &tiles[tile_idx];
        let filter = filter_composer::compose_filter_with_tile(params.clone(), tile, config);
        let options = FindOptions::builder().build();

        let mut cursor = match generate_cursor::<S>(
            &source.client,
            source.db_name,
            source.collection,
            filter,
            Some(options),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error opening cursor for tile {}: {}", tile_idx, e);
                return HttpResponse::InternalServerError().finish();
            }
        };

        if batchmeta.is_some() {
            // ---- batchmeta branch --------------------------------------
            let mut unique_metadata: HashSet<String> = HashSet::new();
            while let Some(result) = cursor.next().await {
                match result {
                    Ok(doc) => {
                        if let Some(t) = transforms::transform_timeseries(
                            &params,
                            &timeseries,
                            &cached_data_info,
                            doc,
                        ) {
                            for m in t.metadata().into_iter() {
                                unique_metadata.insert(m);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Cursor error during batchmeta drain: {}", e);
                        break;
                    }
                }
            }

            if unique_metadata.is_empty() {
                continue; // tile produced no metadata — try the next one.
            }

            let meta_filter = mongodb::bson::doc! {
                "_id": { "$in": unique_metadata.into_iter().collect::<Vec<_>>() }
            };
            let meta_cursor = match generate_cursor::<Document>(
                &source.client,
                source.db_name,
                source.meta_collection,
                meta_filter,
                None,
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error opening metadata cursor: {}", e);
                    return HttpResponse::InternalServerError().finish();
                }
            };
            let docs: Vec<_> = meta_cursor.map(|d| d.unwrap()).collect().await;

            return HttpResponse::Ok().json(json!({
                "docs": docs,
                "next_url": next_url_value(&path, &params, tile_idx, tiles.len()),
                "message": format!("page {}", tile_idx),
            }));
        }

        // ---- streaming branch ------------------------------------------
        //
        // Peek-ahead until we find a doc that survives transformation.
        // If none, advance to the next tile. If found, keep the cursor —
        // we'll continue draining it from inside the response body.
        let mut first_doc: Option<S> = None;
        while let Some(result) = cursor.next().await {
            match result {
                Ok(doc) => {
                    if let Some(t) = transforms::transform_timeseries(
                        &params,
                        &timeseries,
                        &cached_data_info,
                        doc,
                    ) {
                        first_doc = Some(t);
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Cursor error during peek for tile {}: {}", tile_idx, e);
                    return HttpResponse::InternalServerError().finish();
                }
            }
        }

        let first_doc = match first_doc {
            Some(d) => d,
            None => continue, // tile contributed no surviving docs — try next.
        };

        let next_url = next_url_value(&path, &params, tile_idx, tiles.len());
        let page_message = format!("page {}", tile_idx);
        // Each of these gets moved into the stream! generator. params,
        // timeseries, and the data_info cache are needed to transform
        // each subsequent doc; the rest are emitted at the end of the
        // response.
        let params_for_stream = params.clone();
        let ts_for_stream = timeseries.clone();
        let data_info_for_stream = cached_data_info.clone();

        let body = stream! {
            yield Ok::<_, Infallible>(web::Bytes::from_static(b"{\"docs\":["));

            // Serialize the peeked first doc.
            let first_bytes = if is_minimal {
                let stub = transforms::timeseries_stub(&first_doc);
                serde_json::to_vec(&stub).expect("serializing one stub should not fail")
            } else {
                serde_json::to_vec(&first_doc)
                    .expect("serializing a timeseries doc should not fail")
            };
            yield Ok(web::Bytes::from(first_bytes));

            while let Some(result) = cursor.next().await {
                match result {
                    Ok(doc) => {
                        if let Some(t) = transforms::transform_timeseries(
                            &params_for_stream,
                            &ts_for_stream,
                            &data_info_for_stream,
                            doc,
                        ) {
                            let bytes = if is_minimal {
                                let stub = transforms::timeseries_stub(&t);
                                serde_json::to_vec(&stub)
                                    .expect("serializing one stub should not fail")
                            } else {
                                serde_json::to_vec(&t)
                                    .expect("serializing a timeseries doc should not fail")
                            };
                            yield Ok(web::Bytes::from_static(b","));
                            yield Ok(web::Bytes::from(bytes));
                        }
                    }
                    Err(e) => {
                        // Mid-stream error: status is already 200, so we
                        // can only close the JSON cleanly and stop.
                        eprintln!("Cursor error during stream: {}", e);
                        break;
                    }
                }
            }

            // Close the docs array and emit the trailer fields. The
            // serialize calls only fail on non-finite floats inside the
            // value; for the strings/Null we use here that's impossible,
            // but we fall back to safe bytes if for some reason it does.
            yield Ok(web::Bytes::from_static(b"],\"next_url\":"));
            yield Ok(web::Bytes::from(
                serde_json::to_vec(&next_url).unwrap_or_else(|_| b"null".to_vec()),
            ));
            yield Ok(web::Bytes::from_static(b",\"message\":"));
            yield Ok(web::Bytes::from(
                serde_json::to_vec(&page_message)
                    .unwrap_or_else(|_| b"\"\"".to_vec()),
            ));
            yield Ok(web::Bytes::from_static(b"}"));
        };

        return HttpResponse::Ok()
            .content_type("application/json")
            .streaming(body);
    }

    // ---- no non-empty tile in the requested range ---------------------
    //
    // Empty results no longer return 404 — the paginated contract is that
    // `next_url: null` means "no more data", so empty must remain 200.
    HttpResponse::Ok().json(json!({
        "docs": [],
        "next_url": Value::Null,
        "message": format!("no non-empty tiles from index {}", start_idx),
    }))
}

// ---- generic profile handler ------------------------------------------------

/// Generic body of the `/argo`-style profile endpoints — the profile
/// family's `serve_timeseries`. Deliberately a visible mirror rather
/// than a shared mega-generic: the skeleton (validate → tiles →
/// probe-forward → batchmeta | stream → envelope) is identical and the
/// two bodies share the extracted helpers (`generate_cursor`,
/// `next_url_value`, pagination, validation), but the five
/// family-specific joints differ in *signature*, not just behaviour —
/// transform (`transform_profile(params, doc)` vs the grids'
/// axis-and-cache version), stub, tile generation (`_with_slabs`),
/// filter composition (`compose_profile_filter_with_tile_slabs`), and
/// the source struct. Collapsing both into one function means a trait
/// over all five joints at once; worth it when a third family shows up,
/// premature before.
async fn serve_profile<S>(
    req: HttpRequest,
    params: serde_json::Value,
    config: &DatasetConfig,
    source: &ProfileSource,
) -> HttpResponse
where
    S: schema::IsProfile
        + DeserializeOwned
        + Serialize
        + Send
        + Sync
        + Unpin
        + 'static,
{
    let path = req.path().to_string();

    // ---- validation ---------------------------------------------------
    if let Err(response) = helpers::validate_query_params(&params) {
        return response;
    }
    if let Err(response) = helpers::validate_radius_cap(&params, config) {
        return response;
    }
    if let Err(response) = helpers::validate_data_param(&params, config) {
        return response;
    }

    let start_idx = match pagination::parse_tile_index(&params) {
        Ok(i) => i,
        Err(e) => return HttpResponse::BadRequest().json(json!({"error": e})),
    };

    // ---- tile sequence + startup-loaded state --------------------------
    //
    // The slab list is this request's page-axis source of truth: the
    // generator indexes pages against it and the composer resolves those
    // indexes back through it, so both get the same (cloned) list.
    let slab_starts = source.slab_starts.clone();
    let tiles = tile_generator::generate_tiles_with_slabs(&params, config, &slab_starts);

    let compression: Option<String> = params
        .get("compression")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let batchmeta: Option<String> = params
        .get("batchmeta")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let is_minimal = matches!(compression.as_deref(), Some("minimal"));

    // ---- probe-forward loop -------------------------------------------
    //
    // Same contract as the grids: serve at most one non-empty tile per
    // request, advancing past empties.
    for tile_idx in start_idx..tiles.len() {
        let tile = &tiles[tile_idx];
        let filter = filter_composer::compose_profile_filter_with_tile_slabs(
            params.clone(),
            tile,
            config,
            &slab_starts,
        );
        let options = FindOptions::builder().build();

        let mut cursor = match generate_cursor::<S>(
            &source.client,
            source.db_name,
            source.collection,
            filter,
            Some(options),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error opening cursor for tile {}: {}", tile_idx, e);
                return HttpResponse::InternalServerError().finish();
            }
        };

        if batchmeta.is_some() {
            // ---- batchmeta branch --------------------------------------
            let mut unique_metadata: HashSet<String> = HashSet::new();
            while let Some(result) = cursor.next().await {
                match result {
                    Ok(doc) => {
                        if let Some(t) = transforms::transform_profile(&params, doc) {
                            for m in t.metadata().into_iter() {
                                unique_metadata.insert(m);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Cursor error during batchmeta drain: {}", e);
                        break;
                    }
                }
            }

            if unique_metadata.is_empty() {
                continue; // tile produced no metadata — try the next one.
            }

            let meta_filter = mongodb::bson::doc! {
                "_id": { "$in": unique_metadata.into_iter().collect::<Vec<_>>() }
            };
            let meta_cursor = match generate_cursor::<Document>(
                &source.client,
                source.db_name,
                source.meta_collection,
                meta_filter,
                None,
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error opening metadata cursor: {}", e);
                    return HttpResponse::InternalServerError().finish();
                }
            };
            let docs: Vec<_> = meta_cursor.map(|d| d.unwrap()).collect().await;

            return HttpResponse::Ok().json(json!({
                "docs": docs,
                "next_url": next_url_value(&path, &params, tile_idx, tiles.len()),
                "message": format!("page {}", tile_idx),
            }));
        }

        // ---- streaming branch ------------------------------------------
        let mut first_doc: Option<S> = None;
        while let Some(result) = cursor.next().await {
            match result {
                Ok(doc) => {
                    if let Some(t) = transforms::transform_profile(&params, doc) {
                        first_doc = Some(t);
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Cursor error during peek for tile {}: {}", tile_idx, e);
                    return HttpResponse::InternalServerError().finish();
                }
            }
        }

        let first_doc = match first_doc {
            Some(d) => d,
            None => continue, // tile contributed no surviving docs — try next.
        };

        let next_url = next_url_value(&path, &params, tile_idx, tiles.len());
        let page_message = format!("page {}", tile_idx);
        let params_for_stream = params.clone();

        let body = stream! {
            yield Ok::<_, Infallible>(web::Bytes::from_static(b"{\"docs\":["));

            let first_bytes = if is_minimal {
                let stub = schema::ProfileStub::from_profile(&first_doc);
                serde_json::to_vec(&stub).expect("serializing one stub should not fail")
            } else {
                serde_json::to_vec(&first_doc)
                    .expect("serializing a profile doc should not fail")
            };
            yield Ok(web::Bytes::from(first_bytes));

            while let Some(result) = cursor.next().await {
                match result {
                    Ok(doc) => {
                        if let Some(t) =
                            transforms::transform_profile(&params_for_stream, doc)
                        {
                            let bytes = if is_minimal {
                                let stub = schema::ProfileStub::from_profile(&t);
                                serde_json::to_vec(&stub)
                                    .expect("serializing one stub should not fail")
                            } else {
                                serde_json::to_vec(&t)
                                    .expect("serializing a profile doc should not fail")
                            };
                            yield Ok(web::Bytes::from_static(b","));
                            yield Ok(web::Bytes::from(bytes));
                        }
                    }
                    Err(e) => {
                        // Mid-stream error: status is already 200, so we
                        // can only close the JSON cleanly and stop.
                        eprintln!("Cursor error during stream: {}", e);
                        break;
                    }
                }
            }

            yield Ok(web::Bytes::from_static(b"],\"next_url\":"));
            yield Ok(web::Bytes::from(
                serde_json::to_vec(&next_url).unwrap_or_else(|_| b"null".to_vec()),
            ));
            yield Ok(web::Bytes::from_static(b",\"message\":"));
            yield Ok(web::Bytes::from(
                serde_json::to_vec(&page_message)
                    .unwrap_or_else(|_| b"\"\"".to_vec()),
            ));
            yield Ok(web::Bytes::from_static(b"}"));
        };

        return HttpResponse::Ok()
            .content_type("application/json")
            .streaming(body);
    }

    // ---- no non-empty tile in the requested range ---------------------
    HttpResponse::Ok().json(json!({
        "docs": [],
        "next_url": Value::Null,
        "message": format!("no non-empty tiles from index {}", start_idx),
    }))
}

/// Build the JSON value emitted as `next_url`. Returns `Value::Null` when
/// the just-served tile was the last one (no further pages exist).
fn next_url_value(
    path: &str,
    params: &serde_json::Value,
    current_idx: usize,
    total_tiles: usize,
) -> Value {
    if current_idx + 1 < total_tiles {
        Value::String(pagination::build_next_url(path, params, current_idx + 1))
    } else {
        Value::Null
    }
}

/// Build a `DatasetSource` by reading the dataset's meta doc out of Mongo.
///
/// Called once per dataset at server startup. `client` is the dataset's
/// dedicated Mongo client (constructed from its `MONGODB_URI_<DATASET>`
/// env var); the four `'static str` arguments are the dataset's Mongo
/// identity. The resulting struct bundles the client + identity strings
/// with the values we read out of the meta doc (`timeseries`,
/// `data_info`). `main()` then stashes the returned struct into the
/// dataset's `*_SOURCE` static for handlers to read on each request.
///
/// Panics if the meta doc can't be found or the cursor errors. Both
/// indicate startup misconfiguration that should fail loudly rather
/// than serve stale or partial data.
async fn load_dataset_source<M>(
    client: mongodb::Client,
    db_name: &'static str,
    collection: &'static str,
    meta_collection: &'static str,
    meta_data_type: &'static str,
) -> Result<DatasetSource>
where
    M: schema::IsTimeseriesMeta + DeserializeOwned + Unpin + Send + Sync,
{
    let filter = mongodb::bson::doc! {"data_type": meta_data_type};
    let options = FindOptions::builder().limit(1).build();
    let mut cursor =
        generate_cursor::<M>(&client, db_name, meta_collection, filter, Some(options)).await?;

    let meta = match cursor.next().await {
        Some(Ok(m)) => m,
        Some(Err(e)) => panic!(
            "Error reading meta doc for {}.{} (data_type={}): {}",
            db_name, meta_collection, meta_data_type, e
        ),
        None => panic!(
            "No meta doc found in {}.{} matching data_type={}",
            db_name, meta_collection, meta_data_type
        ),
    };

    Ok(DatasetSource {
        client,
        db_name,
        collection,
        meta_collection,
        meta_data_type,
        timeseries: meta.timeseries(),
        data_info: meta.data_info(),
    })
}

/// Build a `ProfileSource` by reading the dataset's time span out of
/// Mongo and generating the slab-start list.
///
/// Called once per profile dataset at server startup — the profile
/// sibling of `load_dataset_source`. Where a grid reads its single meta
/// doc (time axis, `data_info` default), a profile dataset has neither;
/// its startup state is the `TimeSlabs` page-axis boundary list,
/// computed from the collection's `[earliest, latest]` timestamp span
/// (two index-backed sort+limit(1) queries) via
/// `tile_generator::time_slab_starts`.
///
/// An *empty* collection yields an empty slab list, which downstream
/// treats as the degenerate empty axis — we warn rather than panic so a
/// fresh deployment can boot before its first ingestion. A doc missing
/// the `timestamp` field, by contrast, is data corruption and panics
/// loudly. Panics on a `Levels` config — grids load through
/// `load_dataset_source`.
async fn load_profile_source(
    client: mongodb::Client,
    db_name: &'static str,
    collection: &'static str,
    meta_collection: &'static str,
    config: &DatasetConfig,
) -> Result<ProfileSource> {
    let width_days = match config.page_axis {
        dataset_config::PageAxis::TimeSlabs { width_days } => width_days,
        dataset_config::PageAxis::Levels(_) => panic!(
            "load_profile_source called for a Levels dataset ({}.{}) — \
             grids load through load_dataset_source",
            db_name, collection
        ),
    };

    // Earliest and latest timestamps, each via an index-backed
    // sort + limit(1). Typed as raw Documents: we only need one field.
    let mut span = Vec::with_capacity(2);
    for direction in [1_i32, -1_i32] {
        let options = FindOptions::builder()
            .sort(mongodb::bson::doc! { "timestamp": direction })
            .limit(1)
            .build();
        let mut cursor = generate_cursor::<Document>(
            &client,
            db_name,
            collection,
            mongodb::bson::doc! {},
            Some(options),
        )
        .await?;
        match cursor.next().await {
            Some(Ok(doc)) => {
                let ts = doc.get_datetime("timestamp").unwrap_or_else(|e| {
                    panic!(
                        "doc in {}.{} has no readable 'timestamp' ({}): {:?}",
                        db_name, collection, e, doc.get("_id")
                    )
                });
                span.push(*ts);
            }
            Some(Err(e)) => panic!(
                "Error reading time span from {}.{}: {}",
                db_name, collection, e
            ),
            None => {} // empty collection — handled below
        }
    }

    let slab_starts = if span.len() == 2 {
        tile_generator::time_slab_starts(span[0], span[1], width_days)
    } else {
        eprintln!(
            "warning: {}.{} is empty — no time slabs generated; every \
             paginated request will serve the degenerate empty page axis \
             until data is ingested and the API restarted",
            db_name, collection
        );
        Vec::new()
    };

    Ok(ProfileSource {
        client,
        db_name,
        collection,
        meta_collection,
        slab_starts,
    })
}

/// Read a per-dataset Mongo URI env var (e.g. `MONGODB_URI_BSOSE`) and
/// build a `mongodb::Client` from it. Returns `Some(client)` if the var
/// is set and parses cleanly, `None` if the var is unset or empty (the
/// dataset is simply disabled for this deployment), or panics on a
/// malformed URI / unreachable Mongo (treated as deployment-config
/// bugs that should fail loudly at startup).
async fn dataset_client(env_var: &str) -> Option<mongodb::Client> {
    let uri = match env::var(env_var) {
        Ok(u) if !u.is_empty() => u,
        _ => return None,
    };
    let opts = mongodb::options::ClientOptions::parse(&uri)
        .await
        .unwrap_or_else(|e| panic!("invalid {}: {}", env_var, e));
    let client = mongodb::Client::with_options(opts)
        .unwrap_or_else(|e| panic!("could not build Mongo client for {}: {}", env_var, e));
    Some(client)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {

    // Each dataset is enabled iff its `MONGODB_URI_<DATASET>` env var is
    // set. We build a Mongo client per enabled dataset, load its meta
    // doc into the corresponding `*_SOURCE` static, and record a bool
    // so the HttpServer factory below knows whether to register the
    // dataset's route. An unset env var means "this deployment doesn't
    // serve that dataset" — no load, no route, no surprises.
    let mut enabled_bsose = false;
    if let Some(client) = dataset_client("MONGODB_URI_BSOSE").await {
        let bsose = load_dataset_source::<schema::BsoseMeta>(
            client,
            "argo",
            "bsose",
            "timeseriesMeta",
            "BSOSE-profile",
        )
        .await
        .expect("failed to load BSOSE dataset source at startup");
        *BSOSE_SOURCE.lock().unwrap() = Some(bsose);
        enabled_bsose = true;
    }

    let mut enabled_oisst = false;
    if let Some(client) = dataset_client("MONGODB_URI_NOAAOISST").await {
        let oisst = load_dataset_source::<schema::OisstMeta>(
            client,
            "argo",
            "noaaOIsst",
            "timeseriesMeta",
            "noaa-oi-sst-v2-high-res",
        )
        .await
        .expect("failed to load OI SST dataset source at startup");
        *OISST_SOURCE.lock().unwrap() = Some(oisst);
        enabled_oisst = true;
    }

    let mut enabled_copernicussla = false;
    if let Some(client) = dataset_client("MONGODB_URI_COPERNICUSSLA").await {
        let copernicussla = load_dataset_source::<schema::CopernicusSlaMeta>(
            client,
            "argo",
            "copernicusSLA",
            "timeseriesMeta",
            "sea_level_anomaly",
        )
        .await
        .expect("failed to load Copernicus SLA dataset source at startup");
        *COPERNICUSSLA_SOURCE.lock().unwrap() = Some(copernicussla);
        enabled_copernicussla = true;
    }

    let mut enabled_ccmpwind = false;
    if let Some(client) = dataset_client("MONGODB_URI_CCMPWIND").await {
        let ccmpwind = load_dataset_source::<schema::CcmpWindMeta>(
            client,
            "argo",
            "ccmpwind",
            "timeseriesMeta",
            "ccmpwind",
        )
        .await
        .expect("failed to load CCMP wind dataset source at startup");
        *CCMPWIND_SOURCE.lock().unwrap() = Some(ccmpwind);
        enabled_ccmpwind = true;
    }

    let mut enabled_argo = false;
    if let Some(client) = dataset_client("MONGODB_URI_ARGO").await {
        let argo = load_profile_source(
            client,
            "argo",
            "argo",
            "argoMeta",
            &dataset_config::ARGO_CONFIG,
        )
        .await
        .expect("failed to load Argo profile source at startup");
        *ARGO_SOURCE.lock().unwrap() = Some(argo);
        enabled_argo = true;
    }

    if !enabled_bsose
        && !enabled_oisst
        && !enabled_copernicussla
        && !enabled_ccmpwind
        && !enabled_argo
    {
        eprintln!(
            "warning: no datasets enabled. Set at least one of \
             MONGODB_URI_BSOSE / MONGODB_URI_NOAAOISST / \
             MONGODB_URI_COPERNICUSSLA / MONGODB_URI_CCMPWIND / \
             MONGODB_URI_ARGO."
        );
    } else {
        println!(
            "Datasets enabled:{}{}{}{}{}",
            if enabled_bsose { " bsose" } else { "" },
            if enabled_oisst { " noaaoisst" } else { "" },
            if enabled_copernicussla { " copernicussla" } else { "" },
            if enabled_ccmpwind { " ccmpwind" } else { "" },
            if enabled_argo { " argo" } else { "" },
        );
    }

    // `App::configure` lets us register routes conditionally without
    // running into the "each `.service(...)` call returns a new App
    // type" problem.
    HttpServer::new(move || {
        App::new().configure(move |cfg| {
            if enabled_bsose {
                cfg.service(bsose_handler);
            }
            if enabled_oisst {
                cfg.service(oisst_handler);
            }
            if enabled_copernicussla {
                cfg.service(copernicussla_handler);
            }
            if enabled_ccmpwind {
                cfg.service(ccmpwind_handler);
            }
            if enabled_argo {
                cfg.service(argo_handler);
            }
        })
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}

/// Thin wrapper around `client.database(...).collection(...).find(...)`.
/// Takes the Mongo client by reference so callers can keep one client
/// per dataset (see `DatasetSource::client`) instead of reaching into a
/// global. The wrapper exists mostly so the call sites read uniformly
/// across the codebase.
async fn generate_cursor<T: DeserializeOwned>(
    client: &mongodb::Client,
    db_name: &str,
    collection_name: &str,
    filter: Document,
    options: Option<FindOptions>,
) -> Result<mongodb::Cursor<T>> {
    client
        .database(db_name)
        .collection::<T>(collection_name)
        .find(filter, options)
        .await
}