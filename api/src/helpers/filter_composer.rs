//! Compose a tile-aware Mongo filter from query params + a `TileSpec`.
//!
//! The composer is the seam between three pieces:
//!
//!   - `filters::filter_timeseries` builds the *user* filter from the raw
//!     query params (id, polygon, box, center+radius, verticalRange, …).
//!   - `tile_generator::generate_tiles` produces a sequence of `TileSpec`s
//!     describing one page of the result.
//!   - `dataset_config::DatasetConfig` knows the discrete level values that
//!     a `level_index` resolves to.
//!
//! This module's job is to translate a `TileSpec` into BSON predicates and
//! AND them with the user filter. Tile bbox becomes a GeoJSON Polygon on
//! `geolocation: { $geoWithin: { $geometry: Polygon } }` — same shape the
//! existing `polygon_filter` uses, so Mongo evaluates it via the 2dsphere
//! index that's actually present on `geolocation`. We deliberately do NOT
//! use the legacy `$box` shape on `geolocation.coordinates`, because that
//! path doesn't hit the 2dsphere index and can return duplicates under
//! multikey-array semantics when used alone (without an enclosing `$or`).
//! Level index becomes `level: { $gte: L_i, $lt: L_{i+1} }`, with the
//! upper bound omitted for the deepest level so the bracket extends to
//! +infinity (catches anything past the configured maximum).
//!
//! Combination rule: if either side is empty, return the non-empty side
//! directly; otherwise wrap both sides in `$and`. This keeps the resulting
//! query document compact for common cases (id lookups, whole-globe tiles
//! with no user filter) and only nests when we actually need to.
//!
//! Defensive on bounds: an `level_index` past the end of `config.levels` is
//! a programming error (the tile generator should never emit one), but if
//! one slips through we drop the level clause silently rather than panic.

use mongodb::bson::{self, doc, Bson, Document};
use serde_json::{json, Value};

use super::dataset_config::DatasetConfig;
use super::filters::filter_timeseries;
use super::tile_generator::TileSpec;

/// Build the Mongo filter document for one page of a paginated request.
pub fn compose_filter_with_tile(
    params: Value,
    tile: &TileSpec,
    config: &DatasetConfig,
) -> Document {
    let user = filter_timeseries(params);
    let tile_doc = build_tile_filter(tile, config);
    combine_user_and_tile(user, tile_doc)
}

/// Translate a `TileSpec` into the additional Mongo predicates it imposes
/// (spatial bounding box, level bracket). Returns an empty `Document` if
/// the tile is fully null (both fields `None`) — e.g. id lookups.
fn build_tile_filter(tile: &TileSpec, config: &DatasetConfig) -> Document {
    let mut out = Document::new();

    if let Some(bbox) = &tile.tile_bbox {
        // Build the tile as a 5-point GeoJSON Polygon ring (closed),
        // matching the shape `polygon_filter` uses for user polygons.
        // Mongo evaluates this via the 2dsphere index on `geolocation`.
        //
        // Half-open boundary handling: `$geoWithin` is boundary-inclusive
        // by GeoJSON spec — a point on a polygon edge counts as "within".
        // That means a doc sitting exactly on a four-corner meeting point
        // of the tile grid matches all four neighbouring tiles, and a
        // pagination walk emits it four times. We make tile membership
        // half-open ([sw, ne) on both axes) by shrinking the NE corner
        // inward by `TILE_EDGE_EPSILON`. SW is left at the raw boundary,
        // so each grid point ends up owned by exactly one tile — the one
        // whose SW corner it sits at. The epsilon is far smaller than any
        // realistic data resolution, so no real doc falls into the gap.
        //
        // Ring winding is CCW (SW → SE → NE → NW → SW), the GeoJSON
        // convention for the outer ring of a small polygon.
        const TILE_EDGE_EPSILON: f64 = 1.0e-6; // ~11 cm at the equator
        let ne_lon = bbox.ne[0] - TILE_EDGE_EPSILON;
        let ne_lat = bbox.ne[1] - TILE_EDGE_EPSILON;
        let polygon_geom = bson::to_bson(&json!({
            "type": "Polygon",
            "coordinates": [[
                [bbox.sw[0], bbox.sw[1]],
                [ne_lon, bbox.sw[1]],
                [ne_lon, ne_lat],
                [bbox.sw[0], ne_lat],
                [bbox.sw[0], bbox.sw[1]],
            ]],
        }))
        .expect("polygon geometry serialization is infallible for finite floats");
        out.insert(
            "geolocation",
            doc! { "$geoWithin": { "$geometry": polygon_geom } },
        );
    }

    if let Some(i) = tile.level_index {
        let levels = config.levels;
        if i < levels.len() {
            let lower = levels[i];
            let mut level_clause = doc! { "$gte": lower };
            if i + 1 < levels.len() {
                // Half-open bracket: [L_i, L_{i+1}).
                level_clause.insert("$lt", levels[i + 1]);
            }
            // For the final level we omit `$lt`, leaving the bracket as
            // [L_last, +∞) — anything at or below the configured deepest
            // level lands in this page.
            out.insert("level", level_clause);
        }
        // i out of bounds: silently drop the clause. The tile generator
        // shouldn't ever produce this; treating it as "no level filter" is
        // the most forgiving recovery if something upstream goes wrong.
    }

    out
}

/// Combine the user filter and the tile filter. Avoids `$and` wrapping
/// when one side is empty, both to keep query docs readable and to give
/// Mongo the simplest possible shape to plan against.
fn combine_user_and_tile(user: Document, tile: Document) -> Document {
    if user.is_empty() {
        return tile;
    }
    if tile.is_empty() {
        return user;
    }
    let mut combined = Document::new();
    combined.insert(
        "$and",
        Bson::Array(vec![Bson::Document(user), Bson::Document(tile)]),
    );
    combined
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::tile_generator::BoundingBox;
    use serde_json::json;

    /// Two-level dataset config: keeps test math simple and exercises both
    /// the bracketed-level case and the open-ended final-level case.
    const TEST_CONFIG: DatasetConfig = DatasetConfig {
        tile_degrees: 10.0,
        max_radius_meters: 1.0e6,
        levels: &[100.0, 500.0],
    };

    fn null_tile() -> TileSpec {
        TileSpec {
            tile_bbox: None,
            level_index: None,
        }
    }

    fn full_tile(level_index: usize) -> TileSpec {
        TileSpec {
            tile_bbox: Some(BoundingBox {
                sw: [0.0, 0.0],
                ne: [10.0, 10.0],
            }),
            level_index: Some(level_index),
        }
    }

    // ---- empty user filter shortcut ----------------------------------------

    #[test]
    fn empty_params_and_null_tile_yields_empty_filter() {
        let f = compose_filter_with_tile(json!({}), &null_tile(), &TEST_CONFIG);
        assert!(f.is_empty());
    }

    #[test]
    fn empty_params_with_bbox_only_returns_bbox_directly_no_and() {
        let tile = TileSpec {
            tile_bbox: Some(BoundingBox {
                sw: [0.0, 0.0],
                ne: [10.0, 10.0],
            }),
            level_index: None,
        };
        let f = compose_filter_with_tile(json!({}), &tile, &TEST_CONFIG);
        // No $and wrapping — empty user filter means we return tile alone.
        assert!(f.get_array("$and").is_err());
        let geo = f.get_document("geolocation").unwrap();
        let within = geo.get_document("$geoWithin").unwrap();
        let geom = within.get_document("$geometry").unwrap();
        assert_eq!(geom.get_str("type").unwrap(), "Polygon");
    }

    #[test]
    fn empty_params_with_level_only_returns_level_directly() {
        let tile = TileSpec {
            tile_bbox: None,
            level_index: Some(0),
        };
        let f = compose_filter_with_tile(json!({}), &tile, &TEST_CONFIG);
        let level = f.get_document("level").unwrap();
        assert!((level.get_f64("$gte").unwrap() - 100.0).abs() < 1e-9);
        assert!((level.get_f64("$lt").unwrap() - 500.0).abs() < 1e-9);
    }

    #[test]
    fn empty_params_with_full_tile_has_both_clauses_no_and() {
        let f = compose_filter_with_tile(json!({}), &full_tile(0), &TEST_CONFIG);
        assert!(f.get_array("$and").is_err());
        assert!(f.get_document("geolocation").is_ok());
        assert!(f.get_document("level").is_ok());
    }

    // ---- $and wrapping when user filter is non-empty -----------------------

    #[test]
    fn user_box_plus_tile_bbox_wraps_in_and() {
        let f = compose_filter_with_tile(
            json!({"box": "[[5.0, 5.0], [15.0, 15.0]]"}),
            &full_tile(0),
            &TEST_CONFIG,
        );
        let parts = f.get_array("$and").expect("should be $and-wrapped");
        assert_eq!(parts.len(), 2);
        // User filter has $or (from box_filter); tile filter has a
        // geolocation Polygon clause. Both should appear, one per element.
        let p0 = parts[0].as_document().unwrap();
        let p1 = parts[1].as_document().unwrap();
        assert!(
            p0.get_array("$or").is_ok() || p1.get_array("$or").is_ok(),
            "user box $or should land in one of the $and clauses"
        );
        assert!(
            p0.get_document("geolocation").is_ok()
                || p1.get_document("geolocation").is_ok(),
            "tile bbox should land in one of the $and clauses"
        );
    }

    #[test]
    fn user_polygon_plus_tile_bbox_wraps_in_and() {
        let f = compose_filter_with_tile(
            json!({"polygon": "[[0,0],[10,0],[10,10],[0,10],[0,0]]"}),
            &full_tile(1),
            &TEST_CONFIG,
        );
        let parts = f.get_array("$and").expect("should be $and-wrapped");
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn user_vertical_range_plus_tile_level_wraps_in_and_and_keeps_both() {
        let f = compose_filter_with_tile(
            json!({"verticalRange": "[0.0, 1000.0]"}),
            &TileSpec {
                tile_bbox: None,
                level_index: Some(0),
            },
            &TEST_CONFIG,
        );
        let parts = f.get_array("$and").expect("should be $and-wrapped");
        assert_eq!(parts.len(), 2);
        // Both clauses must constrain `level`. Mongo will intersect them.
        let p0 = parts[0].as_document().unwrap();
        let p1 = parts[1].as_document().unwrap();
        assert!(p0.get_document("level").is_ok());
        assert!(p1.get_document("level").is_ok());
    }

    #[test]
    fn user_id_with_null_tile_returns_user_filter_alone() {
        let f =
            compose_filter_with_tile(json!({"id": "doc1"}), &null_tile(), &TEST_CONFIG);
        assert!(f.get_array("$and").is_err());
        assert_eq!(f.get_str("_id").unwrap(), "doc1");
    }

    // ---- final-level open-ended bracket ------------------------------------

    #[test]
    fn final_level_omits_upper_bound() {
        let f = compose_filter_with_tile(
            json!({}),
            &TileSpec {
                tile_bbox: None,
                level_index: Some(TEST_CONFIG.levels.len() - 1),
            },
            &TEST_CONFIG,
        );
        let level = f.get_document("level").unwrap();
        assert!((level.get_f64("$gte").unwrap() - 500.0).abs() < 1e-9);
        assert!(
            level.get_f64("$lt").is_err(),
            "final level should have no $lt — bracket extends to +∞"
        );
    }

    // ---- defensive: out-of-bounds level_index ------------------------------

    #[test]
    fn out_of_bounds_level_index_drops_level_clause() {
        let f = compose_filter_with_tile(
            json!({}),
            &TileSpec {
                tile_bbox: None,
                level_index: Some(TEST_CONFIG.levels.len() + 5),
            },
            &TEST_CONFIG,
        );
        // No level clause emitted, and since user filter is empty too,
        // the whole filter is empty.
        assert!(f.is_empty());
    }

    // ---- bbox shape sanity --------------------------------------------------

    #[test]
    fn tile_bbox_emits_half_open_closed_ccw_ring() {
        let tile = TileSpec {
            tile_bbox: Some(BoundingBox {
                sw: [-10.0, -5.0],
                ne: [20.0, 15.0],
            }),
            level_index: None,
        };
        let f = compose_filter_with_tile(json!({}), &tile, &TEST_CONFIG);
        let geo = f.get_document("geolocation").unwrap();
        let within = geo.get_document("$geoWithin").unwrap();
        let geom = within.get_document("$geometry").unwrap();
        assert_eq!(geom.get_str("type").unwrap(), "Polygon");
        let rings = geom.get_array("coordinates").unwrap();
        assert_eq!(rings.len(), 1, "expected a single outer ring");
        let ring = rings[0].as_array().unwrap();
        assert_eq!(ring.len(), 5, "ring should be 5 points (closed)");

        // SW corner is the raw bbox SW (inclusive). The ring starts here
        // and ends here (closed).
        let sw = ring[0].as_array().unwrap();
        assert!((sw[0].as_f64().unwrap() - -10.0).abs() < 1e-12);
        assert!((sw[1].as_f64().unwrap() - -5.0).abs() < 1e-12);
        let last = ring[4].as_array().unwrap();
        assert_eq!(last[0].as_f64().unwrap(), sw[0].as_f64().unwrap());
        assert_eq!(last[1].as_f64().unwrap(), sw[1].as_f64().unwrap());

        // NE corner has been shrunk inward by a tiny epsilon so that tile
        // membership is half-open. We don't assert the exact epsilon
        // (it's a private constant), only that the NE corner is strictly
        // less than the raw bbox NE and not absurdly shrunk.
        let ne = ring[2].as_array().unwrap();
        let ne_lon = ne[0].as_f64().unwrap();
        let ne_lat = ne[1].as_f64().unwrap();
        assert!(ne_lon < 20.0, "NE lon should be shrunk: {}", ne_lon);
        assert!(ne_lon > 20.0 - 1.0e-3, "NE lon shouldn't be wildly shrunk: {}", ne_lon);
        assert!(ne_lat < 15.0, "NE lat should be shrunk: {}", ne_lat);
        assert!(ne_lat > 15.0 - 1.0e-3, "NE lat shouldn't be wildly shrunk: {}", ne_lat);

        // CCW corners (the SE and NW corners use one shrunk axis and one
        // raw axis — verify the pairing is right).
        let se = ring[1].as_array().unwrap();
        assert!((se[0].as_f64().unwrap() - ne_lon).abs() < 1e-12);
        assert!((se[1].as_f64().unwrap() - -5.0).abs() < 1e-12);
        let nw = ring[3].as_array().unwrap();
        assert!((nw[0].as_f64().unwrap() - -10.0).abs() < 1e-12);
        assert!((nw[1].as_f64().unwrap() - ne_lat).abs() < 1e-12);
    }
}
