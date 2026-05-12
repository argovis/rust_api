//! Per-dataset configuration governing request-size limits.
//!
//! This module is the seam where pagination decisions hang off the dataset
//! identity. Future steps will consult `tile_degrees` to generate spatial
//! pagination tiles, and `max_radius_meters` to reject oversize `center +
//! radius` queries (which go through MongoDB `$near` / `$geoNear` and aren't
//! paginated).
//!
//! Step 1 (current): introduce the type and a BSOSE-specific instance. No
//! behaviour change yet — the handler binds the config but does not act on it.

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
pub struct DatasetConfig {
    pub tile_degrees: f64,
    pub max_radius_meters: f64,
}

/// Configuration for the BSOSE timeseries dataset.
///
/// 10° tiles × 4 grid cells/degree = 40 × 40 = 1600 cells per (tile, level).
/// `max_radius_meters` is a placeholder; revisit with a real
/// operational limit once we have request-distribution data.
pub const BSOSE_CONFIG: DatasetConfig = DatasetConfig {
    tile_degrees: 10.0,
    max_radius_meters: 2_000_000.0, // 2000 km — placeholder
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
}
