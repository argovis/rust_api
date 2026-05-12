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
//! Known limitation: polygons that cross the antimeridian produce a naive
//! bbox spanning most of the globe (min_lon ≈ -180, max_lon ≈ +180),
//! which produces an excessive tile sequence. The existing user-polygon
//! filter has the same issue, so we match its behaviour for now.

use serde_json::Value;

use super::dataset_config::DatasetConfig;

/// A longitude/latitude bounding box. `sw` is the south-west corner
/// (min lon, min lat); `ne` is the north-east corner (max lon, max lat).
/// The box is *half-open* in both dimensions: `[sw_lon, ne_lon) × [sw_lat,
/// ne_lat)`. Documents on the south or west edge belong to the tile;
/// documents on the north or east edge belong to the next tile over. This
/// matters at tile boundaries for grid-aligned datasets like BSOSE — see
/// the box construction in `grid_aligned_tiles`.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundingBox {
    pub sw: [f64; 2],
    pub ne: [f64; 2],
}

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
    cross_levels(
        grid_aligned_tiles(
            BoundingBox {
                sw: [-180.0, -90.0],
                ne: [180.0, 90.0],
            },
            config.tile_degrees,
        ),
        config,
    )
}

fn box_tiles(boxregion: &str, config: &DatasetConfig) -> Vec<TileSpec> {
    let parsed: Vec<Vec<f64>> = match serde_json::from_str(boxregion) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    if parsed.len() != 2 || parsed[0].len() != 2 || parsed[1].len() != 2 {
        return Vec::new();
    }
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
    cross_levels(tiles, config)
}

fn polygon_tiles(polygon: &str, config: &DatasetConfig) -> Vec<TileSpec> {
    let coords: Vec<Vec<f64>> = match serde_json::from_str(polygon) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut min_lon = f64::INFINITY;
    let mut max_lon = f64::NEG_INFINITY;
    let mut min_lat = f64::INFINITY;
    let mut max_lat = f64::NEG_INFINITY;
    for pt in &coords {
        if pt.len() < 2 {
            continue;
        }
        if pt[0] < min_lon {
            min_lon = pt[0];
        }
        if pt[0] > max_lon {
            max_lon = pt[0];
        }
        if pt[1] < min_lat {
            min_lat = pt[1];
        }
        if pt[1] > max_lat {
            max_lat = pt[1];
        }
    }
    if !min_lon.is_finite() || !max_lat.is_finite() {
        return Vec::new();
    }

    cross_levels(
        grid_aligned_tiles(
            BoundingBox {
                sw: [min_lon, min_lat],
                ne: [max_lon, max_lat],
            },
            config.tile_degrees,
        ),
        config,
    )
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
        };
        let tiles = generate_tiles(
            &json!({"box": "[[0,0],[10,10]]"}),
            &EMPTY_LEVELS_CONFIG,
        );
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].level_index, None);
    }
}
