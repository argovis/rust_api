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

Tile size is per-dataset:

- Spatial extent is `DatasetConfig::tile_degrees` (10° for BSOSE).
- Depth pages are the dataset's discrete `levels` (24 brackets for BSOSE).

For BSOSE that's up to 1600 docs per (tile × level) page, with the actual
count clipped by land, the user's filter, and the dataset's coverage.

Each HTTP request serves at most **one** non-empty tile. The server
**probes forward** from the requested `tile_index`, opening a small
cursor per candidate tile and advancing past empties until it finds one
that yields output (or runs out of tiles). `next_url` carries
`tile_index = served_idx + 1`, so the next request resumes one tile past
the one we just emitted. When the server runs out of tiles, `next_url`
is `null`.

This is naive plod-forward — there's no land-mask shortcut yet, so
whole-globe requests do walk a lot of empty tiles server-side. Clients
don't see that work; they only get one HTTP response per non-empty
tile.

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

`api/src/helpers/dataset_config.rs` defines a `DatasetConfig` struct
with the dataset's `tile_degrees`, `max_radius_meters`, and the discrete
`levels` array. The BSOSE handler binds `BSOSE_CONFIG` directly; adding
a new dataset means defining its config there and wiring its handler
through the same `tile_generator` / `filter_composer` machinery.
