//! Pure-function tile generator for paginated geo+depth queries.
//!
//! Given the user's query params and a per-dataset config, produce the
//! ordered sequence of `TileSpec`s that pagination will walk through. Each
//! `TileSpec` carries an optional grid-aligned bounding box (the spatial
//! page boundary) and an optional level index (the depth page).
//!
//! Iteration order is **spatial outer, level inner**: for each spatial tile,
//! all dataset levels are emitted before moving to the next spatial tile.
//! This means a client paginating linearly sees the full water column at
//! one location before getting any data from the next location — friendlier
//! for vertical-profile analysis than horizontal-slab analysis.
//!
//! The tiles are *grid-aligned* multiples of `config.tile_degrees`. We do
//! NOT clip tiles to the user's box or polygon: the user's geo filter stays
//! in the Mongo query, and Mongo intersects it with the tile bbox at query
//! time. This means a 10°×10° tile gives the BSOSE-class upper bound of
//! ~1600 docs even when the user's query covers only a sliver of it.
//!
//! This module is a pure function over its inputs. It doesn't touch
//! MongoDB, doesn't decide which tiles are non-empty, and doesn't compose
//! its output with the user filter. `filter_composer` does the BSON
//! composition, and the handler in `main.rs` drives the probe-forward
//! walk that skips empty tiles.
//!
//! Antimeridian-crossing polygons (any edge spanning > 180° in longitude)
//! are detected and split into an east sub-bbox and a west sub-bbox so we
//! don't over-tile a thin strip across the dateline. Polygons with
//! multiple antimeridian crossings or those spanning more than a
//! hemisphere may still over-tile — the unwrapping is intentionally
//! simple-minded and assumes the polygon has at most one crossing.

use serde_json::Value;

use super::dataset_config::DatasetConfig;
use super::geometry::BoundingBox;
use super::helpers::validlonlat;

/// One unit of pagination. Both fields are `Option` because some query
/// shapes naturally suppress one or the other:
///
///   - `id` lookups have no spatial or level constraint beyond the id
///     itself, so both fields are `None`.
///   - `center + radius` is level-paginated but not spatially tiled, so
///     `tile_bbox` is `None` and `level_index` walks the dataset's levels.
///   - polygon / box / whole-globe requests produce a full grid of
///     `Some(bbox) + Some(level_index)` tiles.
#[derive(Debug, Clone, PartialEq)]
pub struct TileSpec {
    pub tile_bbox: Option<BoundingBox>,
    pub level_index: Option<usize>,
}

/// Produce the ordered tile sequence for one request.
///
/// Precondition: the caller has already validated `params` via
/// `helpers::validate_query_params`, so polygon strings are parseable, box
/// arrays have the right shape, and center/radius are paired. This function
/// is defensive about parse failures (returns an empty vec) but does not
/// re-validate semantics.
pub fn generate_tiles(params: &Value, config: &DatasetConfig) -> Vec<TileSpec> {
    // id wins over everything else: it's a primary-key lookup, no tiling
    // adds value. One TileSpec with no extra constraints.
    if params.get("id").is_some() {
        return vec![TileSpec {
            tile_bbox: None,
            level_index: None,
        }];
    }

    // center + radius: level-only pagination. The `$near` query is bounded
    // by `max_radius_meters` (enforced in helpers::validate_radius_cap),
    // so we don't tile it spatially.
    if params.get("center").is_some() {
        return level_only_tiles(config);
    }

    // polygon: tile the polygon's naive bbox. The polygon itself remains in
    // the Mongo filter; Mongo computes (polygon ∩ tile_bbox) per page.
    if let Some(polygon) = params.get("polygon").and_then(|v| v.as_str()) {
        return polygon_tiles(polygon, config);
    }

    // box: tile the box, splitting at the antimeridian if needed.
    if let Some(boxregion) = params.get("box").and_then(|v| v.as_str()) {
        return box_tiles(boxregion, config);
    }

    // no spatial filter: tile the whole globe.
    whole_globe_tiles(config)
}

// ---------------------------------------------------------------------------

fn level_only_tiles(config: &DatasetConfig) -> Vec<TileSpec> {
    (0..config.levels.len())
        .map(|i| TileSpec {
            tile_bbox: None,
            level_index: Some(i),
        })
        .collect()
}

fn whole_globe_tiles(config: &DatasetConfig) -> Vec<TileSpec> {
    let spatial = grid_aligned_tiles(
        BoundingBox {
            sw: [-180.0, -90.0],
            ne: [180.0, 90.0],
        },
        config.tile_degrees,
    );
    cross_levels(apply_coverage(spatial, config), config)
}

fn box_tiles(boxregion: &str, config: &DatasetConfig) -> Vec<TileSpec> {
    let parsed: Vec<Vec<f64>> = match serde_json::from_str(boxregion) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    if parsed.len() != 2 || parsed[0].len() != 2 || parsed[1].len() != 2 {
        return Vec::new();
    }
    // Normalize coords into [-180, 180] / [-90, 90] so they match what
    // filters::box_filter sends to Mongo. Without this, out-of-range
    // inputs like lon=181 stay raw here and produce tile bboxes outside
    // the valid coordinate range, which Mongo then rejects.
    let parsed = validlonlat(parsed);
    let sw = [parsed[0][0], parsed[0][1]];
    let ne = [parsed[1][0], parsed[1][1]];

    // Dateline-crossing box: sw lon > ne lon means the box wraps the
    // antimeridian. We split it into two grid-aligned pieces, one ending at
    // +180 and one starting at -180. This mirrors what the existing
    // `box_filter` does for the Mongo predicate.
    let sub_boxes: Vec<BoundingBox> = if sw[0] > ne[0] {
        vec![
            BoundingBox {
                sw,
                ne: [180.0, ne[1]],
            },
            BoundingBox {
                sw: [-180.0, sw[1]],
                ne,
            },
        ]
    } else {
        vec![BoundingBox { sw, ne }]
    };

    let mut tiles = Vec::new();
    for bbox in sub_boxes {
        tiles.extend(grid_aligned_tiles(bbox, config.tile_degrees));
    }
    cross_levels(apply_coverage(tiles, config), config)
}

fn polygon_tiles(polygon: &str, config: &DatasetConfig) -> Vec<TileSpec> {
    let coords: Vec<Vec<f64>> = match serde_json::from_str(polygon) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    // Same normalization filters::polygon_filter applies — keeps our tile
    // bboxes aligned with the user filter Mongo actually evaluates, and
    // ensures the antimeridian-detection edges below see wrapped values
    // (e.g. lon=181 becomes -179, so an edge that straddles the dateline
    // genuinely has |lon_diff| > 180°).
    let coords = validlonlat(coords);

    let mut spatial = Vec::new();
    for bbox in polygon_bboxes(&coords) {
        spatial.extend(grid_aligned_tiles(bbox, config.tile_degrees));
    }
    cross_levels(apply_coverage(spatial, config), config)
}

/// Drop tiles that don't overlap the dataset's known coverage region.
/// `config.coverage_bbox = None` means "no a-priori bound" — every tile
/// passes through. With a coverage set, this is where pagination saves
/// the most work: we never even probe regions that *can't* contain data
/// (latitudes north of BSOSE's domain, far-from-the-mooring tiles for
/// some hypothetical regional dataset, etc.).
fn apply_coverage(spatial: Vec<BoundingBox>, config: &DatasetConfig) -> Vec<BoundingBox> {
    match &config.coverage_bbox {
        None => spatial,
        Some(coverage) => spatial.into_iter().filter(|t| t.overlaps(coverage)).collect(),
    }
}

/// Compute the bounding box(es) covering a polygon's vertices, handling
/// antimeridian-crossing polygons by emitting two sub-bboxes (one east of
/// the dateline, one west) instead of one bbox that naively spans most of
/// the globe.
///
/// Detection: any edge of the polygon with `|lon_diff| > 180°` must be the
/// short way around the sphere — i.e. it crosses the antimeridian. (An
/// edge of exactly 180° is ambiguous and treated as non-crossing.)
///
/// Splitting: when crossing is detected, longitudes are *unwrapped* by
/// adding 360° to negative values, putting everything in `[0°, 540°]`.
/// The polygon then has a contiguous lon range `[min, max]` in that
/// space. If `max > 180°` the polygon crosses, and we split into:
///   - east bbox: `[min, 180°]`
///   - west bbox: `[-180°, max - 360°]`
/// Both sub-bboxes share the same lat range (computed across all vertices).
///
/// This assumes at most one antimeridian crossing per polygon. Polygons
/// with multiple crossings or those covering more than a hemisphere will
/// over-tile but stay correct (Mongo's `$geoWithin` does the actual
/// polygon intersection per tile).
fn polygon_bboxes(coords: &[Vec<f64>]) -> Vec<BoundingBox> {
    if coords.is_empty() {
        return Vec::new();
    }

    let crosses_antimeridian = coords.windows(2).any(|w| {
        w[0].len() >= 2 && w[1].len() >= 2 && (w[0][0] - w[1][0]).abs() > 180.0
    });

    let mut min_lat = f64::INFINITY;
    let mut max_lat = f64::NEG_INFINITY;
    for pt in coords {
        if pt.len() < 2 {
            continue;
        }
        if pt[1] < min_lat {
            min_lat = pt[1];
        }
        if pt[1] > max_lat {
            max_lat = pt[1];
        }
    }
    if !min_lat.is_finite() {
        return Vec::new();
    }

    if !crosses_antimeridian {
        let mut min_lon = f64::INFINITY;
        let mut max_lon = f64::NEG_INFINITY;
        for pt in coords {
            if pt.len() < 2 {
                continue;
            }
            if pt[0] < min_lon {
                min_lon = pt[0];
            }
            if pt[0] > max_lon {
                max_lon = pt[0];
            }
        }
        if !min_lon.is_finite() {
            return Vec::new();
        }
        return vec![BoundingBox {
            sw: [min_lon, min_lat],
            ne: [max_lon, max_lat],
        }];
    }

    // Unwrapped longitudes: shift negatives into [180°, 360°] so the
    // polygon becomes contiguous in lon space.
    let mut min_lon = f64::INFINITY;
    let mut max_lon = f64::NEG_INFINITY;
    for pt in coords {
        if pt.len() < 2 {
            continue;
        }
        let lon = if pt[0] < 0.0 { pt[0] + 360.0 } else { pt[0] };
        if lon < min_lon {
            min_lon = lon;
        }
        if lon > max_lon {
            max_lon = lon;
        }
    }
    if !min_lon.is_finite() {
        return Vec::new();
    }

    if max_lon > 180.0 && min_lon < 180.0 {
        // Genuine antimeridian crossing — split.
        vec![
            BoundingBox {
                sw: [min_lon, min_lat],
                ne: [180.0, max_lat],
            },
            BoundingBox {
                sw: [-180.0, min_lat],
                ne: [max_lon - 360.0, max_lat],
            },
        ]
    } else {
        // Detection fired but the unwrapped polygon doesn't actually
        // straddle the dateline (rare, e.g. all vertices in [-180, 0]
        // but with an edge near the dateline whose lon_diff just
        // exceeds 180°). Fall back to the unwrapped bbox un-split —
        // an over-cover that Mongo's polygon filter will trim anyway.
        vec![BoundingBox {
            sw: [min_lon, min_lat],
            ne: [max_lon, max_lat],
        }]
    }
}

/// Tile a bbox into grid-aligned cells of side `tile_degrees`. The grid is
/// anchored at integer multiples of `tile_degrees` from the origin (so for
/// tile_degrees=10, edges are at ...,-20, -10, 0, 10, 20,...). Each emitted
/// tile is half-open: `[lon, lon+T) × [lat, lat+T)`.
fn grid_aligned_tiles(bbox: BoundingBox, tile_degrees: f64) -> Vec<BoundingBox> {
    let mut out = Vec::new();
    // Snap the bbox's SW corner down to the nearest grid line. These are
    // the *first* tile's SW corner. Subsequent tiles step by tile_degrees.
    let lat_start = (bbox.sw[1] / tile_degrees).floor() * tile_degrees;
    let lon_start = (bbox.sw[0] / tile_degrees).floor() * tile_degrees;

    let mut lat = lat_start;
    while lat < bbox.ne[1] {
        let mut lon = lon_start;
        while lon < bbox.ne[0] {
            out.push(BoundingBox {
                sw: [lon, lat],
                ne: [lon + tile_degrees, lat + tile_degrees],
            });
            lon += tile_degrees;
        }
        lat += tile_degrees;
    }

    out
}

/// Cross a list of spatial tiles with the dataset's levels in
/// spatial-outer / level-inner order.
fn cross_levels(spatial: Vec<BoundingBox>, config: &DatasetConfig) -> Vec<TileSpec> {
    let n_levels = config.levels.len();
    let mut out = Vec::with_capacity(spatial.len() * n_levels.max(1));
    for bbox in spatial {
        if n_levels == 0 {
            // Defensive: a dataset with zero levels shouldn't pass our
            // config tests, but if it did we'd still emit one tile per
            // spatial cell so the request returns *something*.
            out.push(TileSpec {
                tile_bbox: Some(bbox),
                level_index: None,
            });
        } else {
            for i in 0..n_levels {
                out.push(TileSpec {
                    tile_bbox: Some(bbox.clone()),
                    level_index: Some(i),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A small, hand-checkable config. Two levels keeps the level multiplier
    /// out of the way; 10° tiles match BSOSE.
    const TEST_CONFIG: DatasetConfig = DatasetConfig {
        tile_degrees: 10.0,
        max_radius_meters: 1.0e6,
        levels: &[0.0, 100.0],
        coverage_bbox: None,
    };

    // ---- top-level dispatch --------------------------------------------------

    #[test]
    fn id_short_circuits_to_single_null_tile() {
        let tiles = generate_tiles(&json!({"id": "doc1"}), &TEST_CONFIG);
        assert_eq!(
            tiles,
            vec![TileSpec {
                tile_bbox: None,
                level_index: None
            }]
        );
    }

    #[test]
    fn id_wins_over_other_geo_params() {
        // If the caller passes id alongside a box, id should still
        // short-circuit. (Validate-step normally forbids combining geo
        // filters, but `id` isn't in that mutual-exclusion check.)
        let tiles = generate_tiles(
            &json!({"id": "doc1", "box": "[[0,0],[10,10]]"}),
            &TEST_CONFIG,
        );
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].tile_bbox, None);
    }

    #[test]
    fn center_radius_is_level_only() {
        let tiles = generate_tiles(
            &json!({"center": "[0,0]", "radius": "1000"}),
            &TEST_CONFIG,
        );
        assert_eq!(tiles.len(), TEST_CONFIG.levels.len());
        for (i, t) in tiles.iter().enumerate() {
            assert_eq!(t.tile_bbox, None, "no spatial tile for center+radius");
            assert_eq!(t.level_index, Some(i), "level pages in order");
        }
    }

    // ---- whole-globe ---------------------------------------------------------

    #[test]
    fn empty_params_tile_whole_globe() {
        let tiles = generate_tiles(&json!({}), &TEST_CONFIG);
        // 18 lat rows × 36 lon cols × 2 levels = 1296
        assert_eq!(tiles.len(), 18 * 36 * TEST_CONFIG.levels.len());
    }

    #[test]
    fn whole_globe_first_tile_is_southwest_corner_level_zero() {
        let tiles = generate_tiles(&json!({}), &TEST_CONFIG);
        assert_eq!(
            tiles[0],
            TileSpec {
                tile_bbox: Some(BoundingBox {
                    sw: [-180.0, -90.0],
                    ne: [-170.0, -80.0],
                }),
                level_index: Some(0),
            }
        );
    }

    #[test]
    fn whole_globe_last_tile_is_northeast_corner_top_level() {
        let tiles = generate_tiles(&json!({}), &TEST_CONFIG);
        let last = tiles.last().unwrap();
        assert_eq!(
            last,
            &TileSpec {
                tile_bbox: Some(BoundingBox {
                    sw: [170.0, 80.0],
                    ne: [180.0, 90.0],
                }),
                level_index: Some(TEST_CONFIG.levels.len() - 1),
            }
        );
    }

    // ---- ordering: spatial outer, level inner --------------------------------

    #[test]
    fn ordering_is_spatial_outer_level_inner() {
        let tiles = generate_tiles(&json!({"box": "[[0,0],[20,10]]"}), &TEST_CONFIG);
        // 2 spatial tiles × 2 levels = 4 specs, in order:
        //   (tile0, lvl0), (tile0, lvl1), (tile1, lvl0), (tile1, lvl1)
        assert_eq!(tiles.len(), 4);
        assert_eq!(tiles[0].level_index, Some(0));
        assert_eq!(tiles[1].level_index, Some(1));
        assert_eq!(tiles[2].level_index, Some(0));
        assert_eq!(tiles[3].level_index, Some(1));
        // Same spatial tile for indices 0,1; same for 2,3; different
        // between the pairs.
        assert_eq!(tiles[0].tile_bbox, tiles[1].tile_bbox);
        assert_eq!(tiles[2].tile_bbox, tiles[3].tile_bbox);
        assert_ne!(tiles[0].tile_bbox, tiles[2].tile_bbox);
    }

    // ---- box -----------------------------------------------------------------

    #[test]
    fn single_cell_box_grid_aligned() {
        let tiles = generate_tiles(&json!({"box": "[[0,0],[10,10]]"}), &TEST_CONFIG);
        assert_eq!(tiles.len(), TEST_CONFIG.levels.len());
        assert_eq!(
            tiles[0].tile_bbox,
            Some(BoundingBox {
                sw: [0.0, 0.0],
                ne: [10.0, 10.0]
            })
        );
    }

    #[test]
    fn multi_cell_box_emits_grid_tiles() {
        // User box (3.5, 7.2) → (24.8, 19.1). Snaps to grid lines at 0, 10,
        // 20, 30 longitude and 0, 10, 20 latitude. Tiles covering the box:
        // 3 cols × 2 rows = 6 tiles.
        let tiles = generate_tiles(
            &json!({"box": "[[3.5,7.2],[24.8,19.1]]"}),
            &TEST_CONFIG,
        );
        assert_eq!(tiles.len(), 6 * TEST_CONFIG.levels.len());

        // First spatial tile should be the SW grid cell [0..10, 0..10].
        // Tiles are NOT clipped to the user box — the user box stays in the
        // Mongo filter.
        assert_eq!(
            tiles[0].tile_bbox,
            Some(BoundingBox {
                sw: [0.0, 0.0],
                ne: [10.0, 10.0]
            })
        );

        // Distinct spatial tiles. BoundingBox holds f64 so we can't put it
        // in a HashSet (no Eq/Hash); a linear dedup against PartialEq is
        // fine for a 12-element vector.
        let mut spatial: Vec<Option<BoundingBox>> = Vec::new();
        for t in &tiles {
            if !spatial.iter().any(|b| b == &t.tile_bbox) {
                spatial.push(t.tile_bbox.clone());
            }
        }
        assert_eq!(spatial.len(), 6);
    }

    #[test]
    fn dateline_crossing_box_splits_into_two_bands() {
        // SW lon 170 > NE lon -170 — the box wraps the antimeridian.
        let tiles = generate_tiles(
            &json!({"box": "[[170,10],[-170,20]]"}),
            &TEST_CONFIG,
        );
        // Two sub-boxes, each one 10°×10° = one grid cell, × 2 levels = 4.
        assert_eq!(tiles.len(), 2 * TEST_CONFIG.levels.len());

        let bboxes: Vec<_> = tiles.iter().map(|t| t.tile_bbox.clone()).collect();
        // East band tile
        assert!(bboxes.contains(&Some(BoundingBox {
            sw: [170.0, 10.0],
            ne: [180.0, 20.0],
        })));
        // West band tile
        assert!(bboxes.contains(&Some(BoundingBox {
            sw: [-180.0, 10.0],
            ne: [-170.0, 20.0],
        })));
    }

    #[test]
    fn malformed_box_returns_empty() {
        // Wrong nesting depth
        let tiles = generate_tiles(&json!({"box": "[1,2,3,4]"}), &TEST_CONFIG);
        assert!(tiles.is_empty());
    }

    // ---- polygon -------------------------------------------------------------

    #[test]
    fn polygon_uses_naive_bbox() {
        // Diamond polygon centred at (15, 15). Bbox is [10,10]→[20,20].
        let tiles = generate_tiles(
            &json!({"polygon": "[[10,15],[15,10],[20,15],[15,20],[10,15]]"}),
            &TEST_CONFIG,
        );
        // bbox spans one 10° cell, so 1 spatial tile × 2 levels = 2 specs.
        assert_eq!(tiles.len(), TEST_CONFIG.levels.len());
        assert_eq!(
            tiles[0].tile_bbox,
            Some(BoundingBox {
                sw: [10.0, 10.0],
                ne: [20.0, 20.0]
            })
        );
    }

    #[test]
    fn polygon_spanning_multiple_cells() {
        // L-shaped-ish polygon. Bbox is [0,0]→[25,15].
        let tiles = generate_tiles(
            &json!({"polygon":
                "[[0,0],[25,0],[25,5],[15,5],[15,15],[0,15],[0,0]]"
            }),
            &TEST_CONFIG,
        );
        // Bbox tile count: 3 cols (0,10,20) × 2 rows (0,10) = 6 cells.
        assert_eq!(tiles.len(), 6 * TEST_CONFIG.levels.len());
    }

    #[test]
    fn malformed_polygon_returns_empty() {
        let tiles = generate_tiles(&json!({"polygon": "not json"}), &TEST_CONFIG);
        assert!(tiles.is_empty());
    }

    #[test]
    fn polygon_crossing_antimeridian_splits_into_two_sub_bboxes() {
        // Polygon straddles the antimeridian: vertices at 170°E and 170°W,
        // i.e. the polygon is a thin strip across the dateline. Naive
        // bbox would give [-170, 0]→[170, 10] (340° wide); we want two
        // bboxes covering [170, 0]→[180, 10] and [-180, 0]→[-170, 10].
        let tiles = generate_tiles(
            &json!({"polygon": "[[170,0],[-170,0],[-170,10],[170,10],[170,0]]"}),
            &TEST_CONFIG,
        );

        // 2 spatial sub-bboxes × 1 spatial tile each × 2 levels = 4 specs.
        // (Each sub-bbox is exactly 10°×10°, so one tile.)
        assert_eq!(tiles.len(), 2 * TEST_CONFIG.levels.len());

        let bboxes: Vec<_> = tiles.iter().map(|t| t.tile_bbox.clone()).collect();
        assert!(bboxes.contains(&Some(BoundingBox {
            sw: [170.0, 0.0],
            ne: [180.0, 10.0],
        })));
        assert!(bboxes.contains(&Some(BoundingBox {
            sw: [-180.0, 0.0],
            ne: [-170.0, 10.0],
        })));
    }

    #[test]
    fn polygon_entirely_east_of_dateline_is_not_split() {
        // All vertices positive, no edge spans > 180°. Single bbox.
        let tiles = generate_tiles(
            &json!({"polygon": "[[10,0],[20,0],[20,10],[10,10],[10,0]]"}),
            &TEST_CONFIG,
        );
        assert_eq!(tiles.len(), TEST_CONFIG.levels.len());
        assert_eq!(
            tiles[0].tile_bbox,
            Some(BoundingBox {
                sw: [10.0, 0.0],
                ne: [20.0, 10.0],
            })
        );
    }

    #[test]
    fn polygon_entirely_west_of_dateline_is_not_split() {
        // All negatives, no edge spans > 180°. Single bbox.
        let tiles = generate_tiles(
            &json!({"polygon": "[[-20,0],[-10,0],[-10,10],[-20,10],[-20,0]]"}),
            &TEST_CONFIG,
        );
        assert_eq!(tiles.len(), TEST_CONFIG.levels.len());
        assert_eq!(
            tiles[0].tile_bbox,
            Some(BoundingBox {
                sw: [-20.0, 0.0],
                ne: [-10.0, 10.0],
            })
        );
    }

    // ---- coverage_bbox filtering --------------------------------------------

    /// Test config restricted to a southern band, matching BSOSE's shape.
    const COVERAGE_TEST_CONFIG: DatasetConfig = DatasetConfig {
        tile_degrees: 10.0,
        max_radius_meters: 1.0e6,
        levels: &[0.0, 100.0],
        coverage_bbox: Some(BoundingBox {
            sw: [-180.0, -90.0],
            ne: [180.0, -30.0],
        }),
    };

    #[test]
    fn coverage_bbox_drops_tiles_entirely_outside() {
        // Whole-globe walk against a southern-band coverage. Every emitted
        // tile must overlap the coverage region — no tiles north of -30°.
        let tiles = generate_tiles(&json!({}), &COVERAGE_TEST_CONFIG);
        assert!(!tiles.is_empty(), "coverage band should still produce tiles");
        for t in &tiles {
            let bb = t.tile_bbox.as_ref().expect("whole-globe → bbox tiles");
            // Tile must have at least some range at lat ≤ -30°.
            assert!(
                bb.sw[1] <= -30.0,
                "tile {:?} should be entirely south of -30° (sw_lat <= -30)",
                bb
            );
        }
        // 6 lat rows × 36 lon cols × 2 levels = 432 specs. The 6 rows are
        // those whose sw_lat is one of -90,-80,-70,-60,-50,-40 (each
        // overlaps the coverage [-90,-30]). Row sw_lat=-30 is also kept
        // by the permissive overlap test (sw_lat=-30 == ne_lat of cov).
        // So 7 rows × 36 cols × 2 levels = 504.
        assert_eq!(tiles.len(), 7 * 36 * COVERAGE_TEST_CONFIG.levels.len());
    }

    #[test]
    fn coverage_bbox_keeps_tile_at_coverage_boundary() {
        // The southernmost "non-covered" tile is the one whose sw_lat
        // equals the coverage's ne_lat. Our permissive overlap test keeps
        // it so that data sitting exactly on the boundary lat doesn't
        // fall in a gap.
        let tiles = generate_tiles(&json!({}), &COVERAGE_TEST_CONFIG);
        let bboxes: Vec<_> = tiles.iter().filter_map(|t| t.tile_bbox.clone()).collect();
        // Boundary tile starts at lat=-30, runs to lat=-20. Should be
        // present in the sequence.
        let boundary_present = bboxes.iter().any(|b| b.sw[1] == -30.0);
        assert!(
            boundary_present,
            "tile whose SW touches the coverage NE should be kept; got {:?}",
            bboxes
        );
    }

    #[test]
    fn coverage_bbox_none_preserves_global_walk() {
        // Sanity: the TEST_CONFIG (coverage_bbox: None) still produces
        // the full 648-tile whole-globe sequence we asserted elsewhere.
        let tiles = generate_tiles(&json!({}), &TEST_CONFIG);
        assert_eq!(tiles.len(), 18 * 36 * TEST_CONFIG.levels.len());
    }

    #[test]
    fn polygon_entirely_outside_coverage_produces_zero_tiles() {
        // Polygon over the Sahara — well north of BSOSE's coverage.
        // Every candidate spatial tile is dropped by the coverage filter,
        // so generate_tiles returns an empty sequence. The handler will
        // fall through to its empty-response path.
        let tiles = generate_tiles(
            &json!({"polygon": "[[0,15],[20,15],[20,25],[0,25],[0,15]]"}),
            &COVERAGE_TEST_CONFIG,
        );
        assert!(
            tiles.is_empty(),
            "polygon outside coverage should produce no tiles, got {:?}",
            tiles
        );
    }

    #[test]
    fn box_partially_outside_coverage_keeps_only_overlapping_tiles() {
        // Box straddling the coverage boundary: lat range [-40, -20].
        // The southern half (lat in [-40,-30]) is inside coverage; the
        // northern half (lat in [-30,-20]) is outside. Only southern
        // tiles should survive.
        let tiles = generate_tiles(
            &json!({"box": "[[10,-40],[20,-20]]"}),
            &COVERAGE_TEST_CONFIG,
        );
        assert!(!tiles.is_empty());
        for t in &tiles {
            let bb = t.tile_bbox.as_ref().unwrap();
            assert!(bb.sw[1] <= -30.0, "tile {:?} should be at or south of -30°", bb);
        }
    }

    #[test]
    fn polygon_with_out_of_range_longitude_gets_normalized_and_split() {
        // The user's exact failure shape: a thin strip straddling the
        // antimeridian, expressed with lon=181 instead of lon=-179. Before
        // the validlonlat call, this looked like a non-crossing polygon
        // with bbox [179, -60]→[181, -58], and tile generation walked off
        // the right edge of the world. After normalization, lon=181
        // becomes lon=-179, the edge 179→-179 is detected as crossing,
        // and we get a sane east/west split.
        //
        // Uses BSOSE_CONFIG so the test reflects the deployment's actual
        // grid alignment — the bbox assertions below assume the current
        // tile_degrees (5°) and will fail with a useful "expected this
        // bbox, got these instead" message if BSOSE_CONFIG changes.
        let tiles = generate_tiles(
            &json!({"polygon": "[[179,-60],[181,-60],[181,-58],[179,-58],[179,-60]]"}),
            &crate::helpers::dataset_config::BSOSE_CONFIG,
        );

        // 2 spatial sub-bboxes × 1 spatial tile each × N levels.
        assert_eq!(
            tiles.len(),
            2 * crate::helpers::dataset_config::BSOSE_CONFIG.levels.len()
        );

        let bboxes: Vec<_> = tiles.iter().map(|t| t.tile_bbox.clone()).collect();
        // East tile catches the 179..180 sliver (5° grid cell [175,180]).
        assert!(
            bboxes.contains(&Some(BoundingBox {
                sw: [175.0, -60.0],
                ne: [180.0, -55.0],
            })),
            "expected east tile [175,-60]→[180,-55] in {:?}",
            bboxes
        );
        // West tile catches the -180..-179 sliver (5° cell [-180,-175]).
        // No tile should have lon outside [-180, 180] — that was the
        // pre-fix symptom.
        assert!(
            bboxes.contains(&Some(BoundingBox {
                sw: [-180.0, -60.0],
                ne: [-175.0, -55.0],
            })),
            "expected west tile [-180,-60]→[-175,-55] in {:?}",
            bboxes
        );
        // And nothing pathological in either direction.
        for b in &bboxes {
            if let Some(bb) = b {
                assert!(
                    bb.sw[0] >= -180.0 && bb.ne[0] <= 180.0,
                    "tile bbox out of range: {:?}",
                    bb
                );
            }
        }
    }

    #[test]
    fn box_with_out_of_range_longitude_gets_normalized_and_split() {
        // Same diagnosis for the box mode: a 2° wide strip across the
        // antimeridian, expressed with lon=181. Before normalization,
        // sw_lon=179 < ne_lon=181 looks like an ordinary non-crossing
        // box. After normalization, lon=181 → -179, sw_lon=179 > -179,
        // dateline split fires.
        let tiles = generate_tiles(
            &json!({"box": "[[179,-60],[181,-58]]"}),
            &crate::helpers::dataset_config::BSOSE_CONFIG,
        );
        assert_eq!(
            tiles.len(),
            2 * crate::helpers::dataset_config::BSOSE_CONFIG.levels.len()
        );

        let bboxes: Vec<_> = tiles.iter().map(|t| t.tile_bbox.clone()).collect();
        assert!(bboxes.contains(&Some(BoundingBox {
            sw: [175.0, -60.0],
            ne: [180.0, -55.0],
        })));
        assert!(bboxes.contains(&Some(BoundingBox {
            sw: [-180.0, -60.0],
            ne: [-175.0, -55.0],
        })));
        for b in &bboxes {
            if let Some(bb) = b {
                assert!(
                    bb.sw[0] >= -180.0 && bb.ne[0] <= 180.0,
                    "tile bbox out of range: {:?}",
                    bb
                );
            }
        }
    }

    #[test]
    fn polygon_edge_at_exactly_180_lon_diff_is_treated_as_non_crossing() {
        // Edge from (90, 0) to (-90, 0) has |lon_diff| = 180 exactly,
        // which is ambiguous (the polygon could go the short way around
        // either hemisphere). We treat exact-180 as non-crossing, which
        // gives a 180°-wide bbox covering the eastern hemisphere; Mongo's
        // $geoWithin will pick its own interpretation.
        let tiles = generate_tiles(
            &json!({"polygon": "[[90,0],[-90,0],[-90,10],[90,10],[90,0]]"}),
            &TEST_CONFIG,
        );
        // One bbox spanning [-90, 0]→[90, 10] = 9 lon cells × 1 lat cell.
        let distinct_bboxes: Vec<_> = {
            let mut v: Vec<Option<BoundingBox>> = Vec::new();
            for t in &tiles {
                if !v.iter().any(|b| b == &t.tile_bbox) {
                    v.push(t.tile_bbox.clone());
                }
            }
            v
        };
        // Some number of distinct tiles, but no SW negative-to-positive split.
        // We just assert we didn't accidentally split (would produce a
        // BoundingBox with sw=[-180, _]).
        assert!(
            !distinct_bboxes.iter().any(|b| {
                b.as_ref().map(|bb| bb.sw[0] == -180.0).unwrap_or(false)
            }),
            "exact-180° edge shouldn't have triggered an antimeridian split"
        );
    }

    // ---- defensive: dataset with no levels ----------------------------------

    #[test]
    fn empty_level_list_still_emits_per_spatial_tile() {
        // Pathological config (the production configs have non-empty
        // levels guarded by a test), but the tile generator should still
        // not crash and should emit one spec per spatial tile.
        const EMPTY_LEVELS_CONFIG: DatasetConfig = DatasetConfig {
            tile_degrees: 10.0,
            max_radius_meters: 1.0e6,
            levels: &[],
            coverage_bbox: None,
        };
        let tiles = generate_tiles(
            &json!({"box": "[[0,0],[10,10]]"}),
            &EMPTY_LEVELS_CONFIG,
        );
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].level_index, None);
    }
}
