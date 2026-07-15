//! Per-dataset configuration governing request-size limits.
//!
//! This is the seam where pagination decisions hang off the dataset
//! identity. `tile_degrees` drives spatial tile generation; `levels`
//! defines the discrete depth pages within each spatial tile;
//! `max_radius_meters` caps `center + radius` queries (which go through
//! MongoDB `$near` and aren't paginated, so the cap is the only thing
//! preventing a runaway disk-of-most-of-the-globe); `coverage_bbox`
//! tells the tile generator the lat/lon rectangle the dataset's data
//! actually lives inside, so we skip probing tiles outside it.
//!
//! `DatasetSource` is the sibling struct that names *where* each dataset
//! lives in Mongo (db, collection, meta collection, meta discriminator),
//! carries the per-dataset `mongodb::Client`, and carries the
//! startup-loaded metadata (the timeseries axis, the meta default
//! `data_info`). One `mongodb::Client` per dataset because the deployment
//! topology may put each dataset in a different Mongo instance (env vars
//! `MONGODB_URI_<DATASET>` per dataset). It can't be `const`/`static`
//! directly because the metadata fields are loaded from Mongo at
//! runtime; it's built once in `main()` via `load_dataset_source` and
//! stashed in a top-level `Lazy<Mutex<Option<DatasetSource>>>`. Handlers
//! clone it out of the lock at the top of each request, then read its
//! fields as plain owned data.

use mongodb::bson::DateTime as BsonDateTime;

use super::geometry::BoundingBox;
use super::schema::DataInfo;

/// Per-dataset request-size policy.
///
/// `tile_degrees`: edge length (degrees of longitude and latitude) of one
/// spatial pagination tile.
///
/// `max_radius_meters`: hard upper bound on the `radius` query parameter
/// for `center + radius` requests. The qsp itself is expressed in km
/// (2.x API format) and converted at validation; the cap stays in meters
/// to match Mongo's `$maxDistance`. These bypass tile pagination because
/// Mongo's `$near` enforces its own bound; we cap the bound so a
/// malicious or naive caller can't ask for a half-globe disk.
///
/// `levels`: the discrete vertical levels the dataset is sampled at, in
/// strictly increasing order (shallowest first). Pagination treats each
/// level as a separate page within a spatial tile. Datasets without a
/// vertical dimension can pass a single-element slice (effectively a
/// single "level" per tile).
///
/// `coverage_bbox`: optional rectangle the dataset's data is known to
/// live inside. The tile generator drops any spatial tile that doesn't
/// overlap this rectangle, so probe-forward never has to walk through
/// regions that *can't* contain data. `None` means "no a-priori bound"
/// — tile generation falls back to walking the whole globe. The
/// rectangle is treated as inclusive on its edges; a doc lying exactly
/// on the coverage boundary is preserved.
///
/// `allowed_data_vars`: the per-dataset variable names accepted in the
/// `data=` qsp. Used by `validate_data_param` to reject typos (and to
/// power the "did you mean" suggestion). Does not include the universal
/// tokens (`all`, `except-data-values`) or integer QC filters —
/// those are accepted regardless of dataset and handled by the
/// validator directly.
pub struct DatasetConfig {
    pub tile_degrees: f64,
    pub max_radius_meters: f64,
    pub levels: &'static [f64],
    pub coverage_bbox: Option<BoundingBox>,
    pub allowed_data_vars: &'static [&'static str],
}

/// Per-dataset identity + Mongo client + startup-loaded metadata, built
/// once in `main()`.
///
/// `client` is this dataset's dedicated `mongodb::Client`. Each dataset
/// gets its own URI (env `MONGODB_URI_<DATASET>`) and its own client —
/// the deployment topology we expect is "BSOSE deployed against one
/// Mongo, OI SST against another," so a single shared client doesn't
/// fit. `mongodb::Client` is internally Arc-backed, so cloning it (and
/// therefore cloning the whole `DatasetSource`) is cheap.
///
/// The next four fields are the dataset's Mongo identity:
///   - `db_name` / `collection`: where the data docs live.
///   - `meta_collection`: where the metadata doc lives. May or may not be
///     shared across datasets; today everything is in `timeseriesMeta`,
///     disambiguated by `meta_data_type`.
///   - `meta_data_type`: the `data_type` discriminator that selects this
///     dataset's meta doc out of `meta_collection`.
///
/// The last two fields are values we load *once* at startup from the
/// meta doc and read on every request. Plain owned types — no cells.
///
/// `Clone` is derived so handlers can copy a `DatasetSource` out of the
/// `Lazy<Mutex<Option<_>>>` static and use it locally without holding
/// the mutex across `.await` points. The clone is cheap: the client is
/// Arc-backed, identity strings are `&'static str`, and the metadata
/// fields are a few KB of dates plus a few short strings.
///
/// `data_info` is the *meta-level default* for the dataset. Per the
/// precedence rule documented on `transforms::transform_timeseries`, a
/// data doc that carries its own `data_info` wins over this default; if
/// the doc's `data_info` is empty, the meta-level value is stamped onto
/// the doc before column filtering runs. Datasets that put `data_info`
/// on every data doc (e.g. BSOSE today) leave this as the empty tuple —
/// it's still populated, just never consulted.
#[derive(Clone)]
pub struct DatasetSource {
    pub client: mongodb::Client,
    pub db_name: &'static str,
    pub collection: &'static str,
    pub meta_collection: &'static str,
    pub meta_data_type: &'static str,
    pub timeseries: Vec<BsonDateTime>,
    pub data_info: DataInfo,
}

/// BSOSE's 52 vertical levels, in metres (positive-downward), shallowest
/// first. From the dataset's published grid; should be updated if BSOSE
/// re-releases with a different vertical discretisation.
pub const BSOSE_LEVELS: &[f64] = &[2.1, 6.7, 12.15, 18.55, 26.25, 35.25, 45.0, 55.0, 65.0, 75.0, 85.0, 95.0, 105.0, 115.0, 125.0, 135.0, 146.5, 161.5, 180.0, 200.0, 220.0, 240.0, 260.0, 280.0, 301.0, 327.0, 361.0, 402.5, 450.0, 500.0, 551.5, 614.0, 700.0, 800.0, 900.0, 1000.0, 1100.0, 1225.0, 1400.0, 1600.0, 1800.0, 2010.0, 2270.0, 2610.0, 3000.0, 3400.0, 3800.0, 4200.0, 4600.0, 5000.0, 5400.0, 5800.0];

/// Configuration for the BSOSE timeseries dataset.
///
/// 5° tiles × 12 grid cells/degree = 60 × 60 = 3600 cells per (tile,
/// level), most less due to land/coastlines. `max_radius_meters` is
/// intentionally tight: BSOSE produces many docs even in a small disk
/// since `$near` isn't spatially tiled. `coverage_bbox` reflects that
/// BSOSE only has data south of 30°S — no point in probing northern
/// tiles that will never contain anything.
pub const BSOSE_CONFIG: DatasetConfig = DatasetConfig {
    tile_degrees: 5.0,
    max_radius_meters: 100_000.0, // 100 km — bump if users complain
    levels: BSOSE_LEVELS,
    coverage_bbox: Some(BoundingBox {
        sw: [-180.0, -90.0],
        ne: [180.0, -30.0],
    }),
    allowed_data_vars: &["THETA", "SALT"],
};

/// OI SST has a single vertical level (the sea surface). We model it as
/// a one-element levels array of 0.0 so the existing tile_generator /
/// filter_composer code path works unchanged: each spatial tile crosses
/// with `level_index = 0` to produce exactly one tile per spatial cell.
/// The filter composer emits `level: { $gte: 0.0 }` for the only-level
/// case (no upper bound — it's both the first and last level), which
/// matches any doc with `level >= 0`. OI SST docs all have `level = 0`.
pub const OISST_LEVELS: &[f64] = &[0.0];

/// Configuration for the NOAA OI SST v2 high-res timeseries dataset.
///
/// 1/4° grid × one level. 5° tiles give ~400 cells per tile (no level
/// multiplier, unlike BSOSE), matching BSOSE's tile size for uniformity
/// even though OI SST has lighter per-tile load. `max_radius_meters`
/// starts at the same 100 km cap as BSOSE — OI SST per-doc payload is
/// smaller (one variable, weekly cadence), so this cap can be relaxed
/// once we have real usage to size it against. `coverage_bbox: None`
/// because OI SST spans the entire globe.
pub const OISST_CONFIG: DatasetConfig = DatasetConfig {
    tile_degrees: 5.0,
    max_radius_meters: 100_000.0, // 100 km — relax once usage informs us
    levels: OISST_LEVELS,
    coverage_bbox: None,
    allowed_data_vars: &["sst"],
};

/// Copernicus SLA is a sea-surface product: like OI SST, a single
/// vertical level modeled as a one-element levels array of 0.0 so the
/// tile_generator / filter_composer path works unchanged. See the
/// comment on `OISST_LEVELS` for the mechanics.
pub const COPERNICUSSLA_LEVELS: &[f64] = &[0.0];

/// Configuration for the Copernicus sea level anomaly timeseries dataset.
///
/// Surface-only, global coverage. Tile size and radius cap deliberately
/// match OI SST (5° / 100 km) — same uniformity argument, same "relax
/// once usage informs us" caveat. Six variables: sea level anomaly,
/// absolute dynamic topography, and the geostrophic velocity components
/// for each (u/v, anomaly and absolute).
pub const COPERNICUSSLA_CONFIG: DatasetConfig = DatasetConfig {
    tile_degrees: 5.0,
    max_radius_meters: 100_000.0, // 100 km — same starting cap as OI SST
    levels: COPERNICUSSLA_LEVELS,
    coverage_bbox: None,
    allowed_data_vars: &["sla", "adt", "ugosa", "ugos", "vgosa", "vgos"],
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

    // ---- OI SST config invariants (mirror the BSOSE checks) ----------------

    #[test]
    fn oisst_tile_degrees_is_positive_and_divides_a_hemisphere() {
        assert!(OISST_CONFIG.tile_degrees > 0.0);
        assert!(
            (180.0_f64 % OISST_CONFIG.tile_degrees).abs() < 1e-9,
            "tile_degrees should evenly divide 180° for clean global coverage"
        );
        assert!(
            (360.0_f64 % OISST_CONFIG.tile_degrees).abs() < 1e-9,
            "tile_degrees should evenly divide 360° for clean global coverage"
        );
    }

    #[test]
    fn oisst_max_radius_is_positive_and_subhemispheric() {
        assert!(OISST_CONFIG.max_radius_meters > 0.0);
        assert!(OISST_CONFIG.max_radius_meters < 1.0e7);
    }

    #[test]
    fn oisst_levels_is_non_empty() {
        // Even a single-level dataset must have a one-element levels
        // array so the tile generator emits one tile per spatial cell.
        // An empty list would produce zero pages.
        assert!(!OISST_CONFIG.levels.is_empty());
    }

    #[test]
    fn oisst_has_exactly_one_level() {
        // OI SST is a surface-only dataset. The single-element levels
        // array is what makes the existing tile generator / filter
        // composer code path work without special-casing "no vertical
        // dimension". If this ever changes, we should pause and think
        // about what multi-level OI SST would mean physically.
        assert_eq!(OISST_CONFIG.levels.len(), 1);
        assert!((OISST_CONFIG.levels[0] - 0.0).abs() < 1e-9);
    }

    #[test]
    fn oisst_has_global_coverage() {
        // OI SST is global; no coverage_bbox skip available.
        assert!(OISST_CONFIG.coverage_bbox.is_none());
    }

    // ---- Copernicus SLA config invariants (mirror the OI SST checks) -------

    #[test]
    fn copernicussla_tile_degrees_is_positive_and_divides_a_hemisphere() {
        assert!(COPERNICUSSLA_CONFIG.tile_degrees > 0.0);
        assert!(
            (180.0_f64 % COPERNICUSSLA_CONFIG.tile_degrees).abs() < 1e-9,
            "tile_degrees should evenly divide 180° for clean global coverage"
        );
        assert!(
            (360.0_f64 % COPERNICUSSLA_CONFIG.tile_degrees).abs() < 1e-9,
            "tile_degrees should evenly divide 360° for clean global coverage"
        );
    }

    #[test]
    fn copernicussla_max_radius_is_positive_and_subhemispheric() {
        assert!(COPERNICUSSLA_CONFIG.max_radius_meters > 0.0);
        assert!(COPERNICUSSLA_CONFIG.max_radius_meters < 1.0e7);
    }

    #[test]
    fn copernicussla_has_exactly_one_surface_level() {
        // Sea level anomaly is by construction a surface product; the
        // single-element levels array keeps the tile generator on the
        // no-special-case path (see OI SST).
        assert_eq!(COPERNICUSSLA_CONFIG.levels.len(), 1);
        assert!((COPERNICUSSLA_CONFIG.levels[0] - 0.0).abs() < 1e-9);
    }

    #[test]
    fn copernicussla_has_global_coverage() {
        // Altimetry-derived SLA is global; no coverage_bbox skip available.
        assert!(COPERNICUSSLA_CONFIG.coverage_bbox.is_none());
    }

    #[test]
    fn copernicussla_advertises_all_six_variables() {
        // sla/adt plus u/v geostrophic velocities in anomaly and absolute
        // flavours. If the upstream product adds or drops a variable this
        // list (and the meta doc's data_info) must move together.
        assert_eq!(
            COPERNICUSSLA_CONFIG.allowed_data_vars,
            &["sla", "adt", "ugosa", "ugos", "vgosa", "vgos"]
        );
    }
}
