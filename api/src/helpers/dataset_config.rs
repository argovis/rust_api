//! Per-dataset configuration governing request-size limits.
//!
//! This is the seam where pagination decisions hang off the dataset
//! identity. `tile_degrees` drives spatial tile generation; `levels`
//! defines the discrete depth pages within each spatial tile;
//! `max_radius_meters` caps `center + radius` queries (which go through
//! MongoDB `$near` and aren't paginated, so the cap is the only thing
//! preventing a runaway disk-of-most-of-the-globe).

/// Per-dataset request-size policy.
///
/// `tile_degrees`: edge length (degrees of longitude and latitude) of one
/// spatial pagination tile. For grid-uniform datasets, choose this so that
/// one (tile × single level) page contains at most ~1600 documents. For
/// BSOSE (1/4° grid) that means 10° tiles.
///
/// `max_radius_meters`: hard upper bound on the `radius` query parameter for
/// `center + radius` requests. These bypass tile pagination because Mongo's
/// `$near` enforces its own bound; we cap the bound so a malicious or naive
/// caller can't ask for a half-globe disk.
///
/// `levels`: the discrete vertical levels the dataset is sampled at, in
/// strictly increasing order (shallowest first). Pagination treats each
/// level as a separate page within a spatial tile. Datasets without a
/// vertical dimension can pass a single-element slice (effectively a single
/// "level" per tile).
pub struct DatasetConfig {
    pub tile_degrees: f64,
    pub max_radius_meters: f64,
    pub levels: &'static [f64],
}

/// Placeholder BSOSE level spectrum.
///
/// These are *not* the real BSOSE levels — Katie will overwrite them with
/// the actual depths once we have them in hand. The shape (roughly:
/// near-surface dense, deep-ocean coarse, ~5500 m bottom) is representative
/// of typical Southern Ocean gridded products so the rest of the pagination
/// machinery sees plausible input.
pub const BSOSE_LEVELS: &[f64] = &[
    5.0, 15.0, 25.0, 40.0, 60.0, 85.0, 120.0, 165.0, 220.0, 290.0,
    380.0, 490.0, 625.0, 790.0, 990.0, 1230.0, 1520.0, 1870.0,
    2290.0, 2790.0, 3380.0, 4070.0, 4870.0, 5575.0,
];

/// Configuration for the BSOSE timeseries dataset.
///
/// 10° tiles × 4 grid cells/degree = 40 × 40 = 1600 cells per (tile, level).
/// `max_radius_meters` is intentionally tight: BSOSE at 1/4° resolution
/// produces ~16 docs per 25 km × 25 km cell, so even a small disk pulls
/// thousands of docs out of `$near` (which isn't spatially tiled). 100 km
/// is a conservative starting point — easy to bump up if users complain.
pub const BSOSE_CONFIG: DatasetConfig = DatasetConfig {
    tile_degrees: 10.0,
    max_radius_meters: 100_000.0, // 100 km — bump if users complain
    levels: BSOSE_LEVELS,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bsose_tile_degrees_is_positive_and_divides_a_hemisphere() {
        assert!(BSOSE_CONFIG.tile_degrees > 0.0);
        // We don't strictly require integer-divisibility of 180/360 by
        // tile_degrees (the tile generator will handle ragged remainders),
        // but a divisor is a useful invariant to flag if someone bumps the
        // value to something exotic like 7.0.
        assert!(
            (180.0_f64 % BSOSE_CONFIG.tile_degrees).abs() < 1e-9,
            "tile_degrees should evenly divide 180° for clean global coverage"
        );
        assert!(
            (360.0_f64 % BSOSE_CONFIG.tile_degrees).abs() < 1e-9,
            "tile_degrees should evenly divide 360° for clean global coverage"
        );
    }

    #[test]
    fn bsose_max_radius_is_positive_and_subhemispheric() {
        assert!(BSOSE_CONFIG.max_radius_meters > 0.0);
        // Earth's mean radius is ~6.371e6 m; a half-circumference is ~2.0e7 m.
        // We want our cap well under that so we never approach the
        // antipode-degenerate case that breaks Mongo geo queries.
        assert!(BSOSE_CONFIG.max_radius_meters < 1.0e7);
    }

    #[test]
    fn bsose_levels_is_non_empty() {
        // Pagination treats each level as a page; an empty level list would
        // produce a dataset with zero pages, which is almost certainly a
        // misconfiguration rather than an intentional state.
        assert!(!BSOSE_CONFIG.levels.is_empty());
    }

    #[test]
    fn bsose_levels_is_strictly_increasing() {
        // The tile generator will rely on level order to map a level index
        // to a (lower, upper) depth bracket. If two levels collide or the
        // sequence reverses, that mapping is ambiguous.
        for w in BSOSE_CONFIG.levels.windows(2) {
            assert!(
                w[0] < w[1],
                "levels must be strictly increasing; found {} not < {}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn bsose_levels_are_all_non_negative() {
        // Depth is conventionally positive-downward in oceanographic data;
        // a negative value would indicate a sign-convention bug we'd want
        // to catch early.
        for &d in BSOSE_CONFIG.levels {
            assert!(d >= 0.0, "level depths should be non-negative; got {}", d);
        }
    }
}
