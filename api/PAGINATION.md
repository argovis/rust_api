# Pagination

Every response from the `/timeseries/...` endpoints is a JSON envelope:

```json
{
  "docs": [...],
  "next_url": "/timeseries/bsose?...&tile_index=N",
  "message": "page N"
}
```

- `docs` — array of result documents (or stubs / metadata documents,
  depending on the mode flags). May be empty.
- `next_url` — relative path + query for the next page. `null` when this
  is the last page. Clients resolve it against the original request's
  origin and follow it until they see `null`.
- `message` — human-readable status, currently the served tile index.

There is no separate "no results" status code: an empty response is
`200 + {docs: [], next_url: null, ...}`, never `404`.

## How pagination walks

Server-side, each request's spatial parameters define a sequence of
**tiles**. A tile is one spatial sub-region paired with one discrete
depth level. Tiles are ordered *spatial outer, level inner*: all levels
for one (lon, lat) cell come out before moving to the next cell.

Tile size and extent are per-dataset:

- Spatial extent is `DatasetConfig::tile_degrees` (5° for BSOSE).
- Depth pages are the dataset's discrete `levels` (52 brackets for BSOSE).
- The tile sequence is clipped to `DatasetConfig::coverage_bbox`, an
  optional rectangle that tells the generator where the dataset has
  data. For BSOSE that's `[-180,-90]→[180,-30]` (south of 30°S); for
  datasets without an a-priori coverage bound, it can be `None` and
  the generator walks the whole globe.

Each HTTP request serves at most **one** non-empty tile. The server
**probes forward** from the requested `tile_index`, opening a small
cursor per candidate tile and advancing past empties until it finds one
that yields output (or runs out of tiles). `next_url` carries
`tile_index = served_idx + 1`, so the next request resumes one tile past
the one we just emitted. When the server runs out of tiles, `next_url`
is `null`.

The coverage bbox is the cheap way to keep probe-forward sane: tiles
that fall entirely outside the coverage are never probed at all, so
e.g. a BSOSE whole-globe walk doesn't have to confirm that the entire
Northern Hemisphere is empty before terminating. Probe-forward is still
linear in the number of *candidate* tiles after coverage filtering, so
sparse datasets within their coverage area can still incur empty
probes — a denser secondary mask (e.g. land/ocean per cell) would help
here but isn't implemented.

## Tile membership

Tiles are **half-open** — `[sw, ne)` on both lon and lat axes — so each
grid point is owned by exactly one tile (the one whose SW corner it sits
at). Without this, a doc at the corner where four tiles meet would be
emitted four times. The half-open behaviour is implemented by shrinking
each tile's NE corner inward by a sub-cm epsilon, *except* at the global
east meridian (`lon=180`) and the north pole (`lat=90`), where there's
no neighbouring tile to overlap with — those edges remain inclusive so
antimeridian / north-pole docs aren't lost.

## Special spatial modes

| Mode | Tile sequence |
|------|---------------|
| `id` | A single passthrough tile — no spatial or level constraint is added. |
| `center + radius` | No spatial tiling; pagination is level-only. Radius must satisfy `radius ≤ max_radius_meters` (100 km for BSOSE today). |
| `polygon` | Tile the polygon's bounding box. Mongo `$geoWithin` does the actual polygon intersection per tile. |
| `box` | Tile the box. A dateline-crossing box (`sw_lon > ne_lon`) is split into east and west sub-boxes; tile generation runs on each. |
| no spatial param | Tile the whole globe. |

## Mode flags

- `compression=minimal` — each doc is serialized as a compact 5-element
  array `[_id, lon, lat, level, metadata]` rather than the full
  measurement document.
- `batchmeta` — instead of measurement docs, return the *metadata*
  documents referenced by the matching docs (looked up in
  `timeseriesMeta`). Aggregates per-page; clients union across pages.
  Takes precedence over `compression=minimal` if both are set.

## Query parameters

| Param | Type | Notes |
|-------|------|-------|
| `id` | string | Exact match on `_id`. |
| `box` | JSON `[[sw_lon, sw_lat], [ne_lon, ne_lat]]` | Bounding box. Wraps the dateline if `sw_lon > ne_lon`. |
| `polygon` | JSON `[[lon, lat], ...]` | Closed ring of vertices (first = last), ≥ 4 points. |
| `center` + `radius` | JSON `[lon, lat]` + meters | Disk query. Radius capped at the dataset's `max_radius_meters`. |
| `verticalRange` | JSON `[lo, hi]` | Half-open depth range applied on top of tile-level pagination. |
| `startDate` / `endDate` | RFC-3339 string | Slices each doc's timeseries to this window. |
| `data` | comma-separated | Variables to include. `all` keeps everything. `except_data_values` keeps the schema but clears values. |
| `compression` | `minimal` | See mode flags. |
| `batchmeta` | any | See mode flags. |
| `tile_index` | non-negative integer | Pagination cursor. Default `0`. Almost always supplied by the previous response's `next_url`. |

## Validation errors (HTTP 400)

- More than one of `polygon` / `box` / `center` set.
- `center` set without `radius`, or vice versa.
- `radius` non-numeric, negative, non-finite, or above the dataset's cap.
- `polygon` malformed: fewer than 4 points, not closed, or any vertex
  that isn't a 2-element pair.
- `startDate` or `endDate` not RFC-3339.
- `tile_index` present but not a non-negative integer.

## Known limitations

- **Polygons spanning more than a hemisphere or with multiple
  antimeridian crossings.** Single-crossing antimeridian polygons are
  detected and split into east + west sub-bboxes (no globe-spanning
  over-tile). Polygons with two or more antimeridian crossings, or
  polygons covering more than half the sphere, may over-tile — the
  result is still correct (Mongo's `$geoWithin` does the actual
  polygon intersection), just slower than ideal.
- **Grid-aligned user box NE corner.** A user-supplied box whose NE
  corner sits exactly on a tile grid line (e.g.
  `box=[[20,10],[40,30]]`) will lose docs at that exact NE corner,
  because the rightmost/topmost tile's NE is shrunk by the half-open
  mechanism. Workaround on the client: pad the NE by a tiny amount.
- **Antipodal docs at `lon=±180` stored as distinct values.** Docs at
  `lon=+180` land in the easternmost tile, docs at `lon=-180` in the
  westernmost — same physical meridian, two different pages. Data
  providers should normalise to one convention on insertion.

## Per-dataset configuration

Two per-dataset structs sit side by side in `api/src/helpers/dataset_config.rs`:

- **`DatasetConfig`** — *request-size policy*. `tile_degrees` (spatial
  page size), `max_radius_meters` (cap for `center + radius` queries),
  `levels` (discrete vertical pages — single-element `&[0.0]` for
  surface-only datasets like OI SST), and an optional `coverage_bbox`
  (rectangle the dataset's data lives inside; `None` means walk the
  whole globe). Declared as a `pub const` per dataset.

- **`DatasetSource`** — *Mongo identity* (`db_name`, `collection`,
  `meta_collection`, `meta_data_type`) plus the values read once at
  startup from the meta doc (`timeseries` axis, `data_info` default).
  Built at runtime by `main()` via `load_dataset_source::<MetaSchema>`
  and stashed in a `Lazy<Mutex<Option<DatasetSource>>>` static (same
  pattern as the Mongo `CLIENT` static).

The generic handler `serve_timeseries::<S>` in `main.rs` consumes
`(&DatasetConfig, &DatasetSource)` plus a schema generic `S` that
implements `IsTimeseries`.

### Recipe for adding a new dataset

1. Define the data-doc schema (`S`) and meta-doc schema (`M`) in
   `api/src/helpers/schema.rs`. `S` implements `IsTimeseries`; `M`
   implements `IsTimeseriesMeta`.
2. Define `<NAME>_CONFIG: DatasetConfig` (and `<NAME>_LEVELS` if depth
   discretisation is non-trivial) in `dataset_config.rs`.
3. Add `<NAME>_SOURCE: Lazy<Mutex<Option<DatasetSource>>>` in
   `main.rs`, next to the existing dataset statics.
4. In `main()`, gate on the dataset's URI env var and load conditionally:
   ```
   let mut enabled_<name> = false;
   if let Some(client) = dataset_client("MONGODB_URI_<NAME>").await {
       let x = load_dataset_source::<M>(client, ...).await?;
       *<NAME>_SOURCE.lock().unwrap() = Some(x);
       enabled_<name> = true;
   }
   ```
5. Add a 4-line route handler annotated with `#[get("/timeseries/<name>")]`
   that clones the source out of the lock and forwards to
   `serve_timeseries::<S>`.
6. Register the handler inside the `App::configure` callback, gated on
   `enabled_<name>`. A dataset whose URI env var is unset stays
   unregistered: no route, no startup work, no panic.

### Per-deployment configuration

Each dataset is enabled iff its `MONGODB_URI_<DATASET>` env var is set
when `main()` runs. URI presence is the enable signal — there's no
separate `DATASETS=` list to keep in sync. A deployment serving only
one dataset just sets one env var:

```
MONGODB_URI_BSOSE=mongodb://bsose-mongo/ cargo run     # BSOSE-only
MONGODB_URI_NOAAOISST=mongodb://noaa-mongo/ cargo run  # OI SST-only
```

For local dev / tests where one Mongo serves both, point both env vars
at the same URI:

```
MONGODB_URI_BSOSE=mongodb://localhost:27017 \
MONGODB_URI_NOAAOISST=mongodb://localhost:27017 \
  cargo run
```

### `data_info` precedence rule

`data_info` (variable names, units, per-variable descriptors) may
appear on either the data doc, the meta doc, or both:

- **Doc-level wins.** If a data doc carries its own non-empty
  `data_info` (BSOSE today), that's what `slice_data` filters
  against. The cached meta-level default is ignored.
- **Cache fallback.** If the data doc has no `data_info` (OI SST: the
  field lives only on the meta doc), the per-dataset cached default —
  loaded from the meta doc at startup — is stamped onto the doc
  before column filtering runs.

The cache for a dataset whose meta doc has no `data_info` is the empty
tuple, and `transform_timeseries` treats empty as "no default to
apply". So a dataset can store `data_info` per data doc, per meta doc,
or per both — the response carries the right thing in each case.
