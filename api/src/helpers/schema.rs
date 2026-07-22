use serde::{Deserialize, Serialize};
use serde::ser::{Serializer, SerializeSeq};
use mongodb::bson::DateTime as BsonDateTime;

// generic structs ////////////////////////////////////////////////////////////

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeoJSONPoint {
    #[serde(rename = "type")]
    pub(crate) location_type: String,
    pub(crate) coordinates: [f64; 2],
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SourceMeta {
    pub(crate) source: Vec<String>,
    pub(crate) iter: String,
}

// type aliases ///////////////////////////////////////////////////////////////

/// Per-variable descriptor carried alongside a timeseries doc:
/// `(variable_names, info_fields, per_variable_info)`. The shape is
/// preserved as a tuple for backward-compatible JSON serialization (the
/// public response format encodes it as a 3-tuple), but giving it a name
/// makes function signatures and the per-dataset cache easier to read.
pub type DataInfo = (Vec<String>, Vec<String>, Vec<Vec<String>>);

/// The profile-family flavour of `DataInfo`. Same 3-tuple shape, but the
/// per-variable info cells are nullable: Argo stores `[null, null]` for
/// QC companion columns (a QC flag has no units and no data-keys mode),
/// so `Vec<Vec<String>>` would fail to deserialize a real Argo doc.
/// `None` cells round-trip as JSON `null`, matching the source
/// representation. The grids keep the non-nullable `DataInfo` — their
/// meta-authored descriptors never contain nulls, and keeping the types
/// separate means the profile family can't silently loosen grid
/// guarantees.
pub type ProfileDataInfo = (Vec<String>, Vec<String>, Vec<Vec<Option<String>>>);

// categroical traits /////////////////////////////////////////////////////////

pub trait IsTimeseries {
    fn get_timeseries(&self) -> bool;
    fn data(&mut self) -> &mut Vec<Vec<f64>>;
    fn set_data(&mut self, data: Vec<Vec<f64>>);
    fn timeseries(&mut self) -> Option<&mut Vec<String>>;
    fn set_timeseries(&mut self, timeseries: Vec<String>);
    /// `data_info` is `Option` because the response carries it only when
    /// the user's query has *materially altered* the data layout —
    /// concretely, when the `data=` qsp triggers column filtering. In
    /// the no-`data=` case `transform_timeseries` writes `None` here so
    /// the response omits the field, and clients fall back to the
    /// dataset-wide `data_info` on the meta endpoint.
    fn data_info(&mut self) -> Option<DataInfo>;
    fn set_data_info(&mut self, data_info: Option<DataInfo>);
    fn _id(&self) -> String;
    fn longitude(&self) -> f64;
    fn latitude(&self) -> f64;
    fn level(&self) -> f64;
    fn metadata(&self) -> Vec<String>;
}

pub trait IsTimeseriesMeta {
    fn get_timeseries_meta(&self) -> bool;
    /// Snapshot of the dataset's timestamp axis. Read once at startup and
    /// cached on `DatasetSource::timeseries` so request handling doesn't
    /// re-fetch it per request.
    fn timeseries(&self) -> Vec<BsonDateTime>;
    /// Per-dataset `data_info` default. Stamped onto a data doc by
    /// `transform_timeseries` only when that doc carries no `data_info` of
    /// its own (the precedence rule: doc-level wins over meta-level). May
    /// be the empty tuple — datasets that store `data_info` on every data
    /// doc (e.g. BSOSE today) leave the meta-level value blank.
    fn data_info(&self) -> DataInfo;
}

// bsose //////////////////////////////////////////////////////////////////////

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BsoseSchema {
    pub(crate) _id: String,
    // `metadata` is reachable from main.rs (the `batchmeta` branch builds a
    // unique-set out of it), so it stays fully `pub` rather than `pub(crate)`.
    pub metadata: Vec<String>,
    pub(crate) basin: f64,
    pub(crate) geolocation: GeoJSONPoint,
    pub(crate) level: f64,
    pub(crate) cell_vertical_fraction: f64,
    pub(crate) sea_binary_mask_at_t_locaiton: bool,
    pub(crate) cell_z_size: f64,
    pub(crate) reference_density_profile: f64,
    // `data` is the per-variable per-timestep array. Omitted from the
    // response when empty so the no-`data=` qsp response stays slim
    // (transform_timeseries clears it in that branch). When `data=` is
    // set and `data` ends up empty after filtering, the whole doc gets
    // dropped before serialization, so an empty array never ships.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) data: Vec<Vec<f64>>,
    // Both of the next two are response-shape-driven: present iff the
    // user's query made the dataset-wide default insufficient. Per the
    // response-shape rule, `data_info` appears only when `data=` qsp
    // triggered column filtering; `timeseries` appears only when the
    // user cut the time axis with `startDate` / `endDate`. Both
    // serialize-absent when None and default to None / empty on
    // deserialization, so a BSOSE source doc that carries `data_info`
    // (the per-cell default for BSOSE today) still reads cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timeseries: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data_info: Option<DataInfo>,
}

impl IsTimeseries for BsoseSchema {
    fn get_timeseries(&self) -> bool {
        return true;
    }

    fn data(&mut self) -> &mut Vec<Vec<f64>> {
        &mut self.data
    }

    fn set_data(&mut self, data: Vec<Vec<f64>>) {
        self.data = data;
    }

    fn timeseries(&mut self) -> Option<&mut Vec<String>> {
        self.timeseries.as_mut()
    }

    fn set_timeseries(&mut self, timeseries: Vec<String>) {
        self.timeseries = Some(timeseries);
    }

    fn data_info(&mut self) -> Option<DataInfo> {
        self.data_info.clone()
    }

    fn set_data_info(&mut self, data_info: Option<DataInfo>) {
        self.data_info = data_info;
    }

    fn _id(&self) -> String {
        self._id.clone()
    }

    fn longitude(&self) -> f64 {
        self.geolocation.coordinates[0]
    }

    fn latitude(&self) -> f64 {
        self.geolocation.coordinates[1]
    }

    fn level(&self) -> f64 {
        self.level
    }

    fn metadata(&self) -> Vec<String> {
        self.metadata.clone()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BsoseMeta {
    pub(crate) _id: String,
    pub(crate) data_type: String,
    pub(crate) date_updated_argovis: BsonDateTime,
    // `timeseries` and `data_info` are read at startup to populate the
    // per-dataset cache on `DatasetSource`, so both stay fully `pub`.
    pub timeseries: Vec<BsonDateTime>,
    pub(crate) source: Vec<SourceMeta>,
    pub(crate) cell_area: f64,
    pub(crate) ocean_depth: f64,
    pub(crate) depth_r0_to_bottom: f64,
    pub(crate) interior_2d_mask: bool,
    pub(crate) depth_r0_to_ref_surface: f64,
    // `data_info` may or may not be present on the BSOSE meta doc (today
    // it lives only on the data docs themselves). `#[serde(default)]`
    // makes deserialization tolerate either case: when absent the cache
    // becomes the empty sentinel and per-doc values keep taking
    // precedence, exactly the current behaviour.
    #[serde(default)]
    pub data_info: DataInfo,
}

impl IsTimeseriesMeta for BsoseMeta {
    fn get_timeseries_meta(&self) -> bool {
        return true;
    }

    fn timeseries(&self) -> Vec<BsonDateTime> {
        self.timeseries.clone()
    }

    fn data_info(&self) -> DataInfo {
        self.data_info.clone()
    }
}

// oi sst /////////////////////////////////////////////////////////////////////

/// `source` substructure for the OI SST metadata doc. Differs from
/// `SourceMeta` (used by BSOSE) — OI SST uses `url` where BSOSE uses
/// `iter`. Neither field is read by the handler today; modeled here so
/// deserialization of the meta doc succeeds.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OisstSourceMeta {
    pub(crate) source: Vec<String>,
    pub(crate) url: String,
}

/// Grid descriptor on the OI SST metadata doc. Captures the regular
/// lat/lon lattice the dataset is sampled on. Not consulted by the
/// handler today (the equivalent information lives in `OISST_CONFIG`),
/// but modeled here so deserialization succeeds. A future cleanup could
/// derive the dataset's `DatasetConfig.coverage_bbox` / `tile_degrees`
/// from this struct instead of duplicating the values in code.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Lattice {
    pub center: [f64; 2],
    pub spacing: [f64; 2],
    pub min_lat: f64,
    pub min_lon: f64,
    pub max_lat: f64,
    pub max_lon: f64,
}

/// One spatial cell of the NOAA OI SST v2 high-res grid. Surface-only
/// (no vertical dimension; `level` is always `0.0`). `data` holds the
/// timeseries per variable — there's exactly one variable (SST), so the
/// outer Vec always has length one.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OisstSchema {
    pub(crate) _id: String,
    // Reachable from main.rs (batchmeta branch reads `metadata()`), so
    // `pub` for symmetry with BsoseSchema.
    pub metadata: Vec<String>,
    pub(crate) basin: f64,
    pub(crate) geolocation: GeoJSONPoint,
    pub(crate) level: f64,
    // Omitted from the response when empty (no `data=` qsp); see the
    // matching annotation on `BsoseSchema.data` for the full reasoning.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) data: Vec<Vec<f64>>,
    // OI SST data docs don't carry `timeseries` or `data_info` of their
    // own — both are populated at request time per the response-shape
    // rule. `timeseries` is filled by `slice_timerange` iff the user
    // set `startDate` / `endDate`; `data_info` is stamped (from the
    // per-dataset cached default) by `transform_timeseries` iff the
    // user set `data=`. Both serialize-absent when None and default to
    // None on deserialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timeseries: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data_info: Option<DataInfo>,
}

impl IsTimeseries for OisstSchema {
    fn get_timeseries(&self) -> bool {
        return true;
    }

    fn data(&mut self) -> &mut Vec<Vec<f64>> {
        &mut self.data
    }

    fn set_data(&mut self, data: Vec<Vec<f64>>) {
        self.data = data;
    }

    fn timeseries(&mut self) -> Option<&mut Vec<String>> {
        self.timeseries.as_mut()
    }

    fn set_timeseries(&mut self, timeseries: Vec<String>) {
        self.timeseries = Some(timeseries);
    }

    fn data_info(&mut self) -> Option<DataInfo> {
        self.data_info.clone()
    }

    fn set_data_info(&mut self, data_info: Option<DataInfo>) {
        self.data_info = data_info;
    }

    fn _id(&self) -> String {
        self._id.clone()
    }

    fn longitude(&self) -> f64 {
        self.geolocation.coordinates[0]
    }

    fn latitude(&self) -> f64 {
        self.geolocation.coordinates[1]
    }

    fn level(&self) -> f64 {
        self.level
    }

    fn metadata(&self) -> Vec<String> {
        self.metadata.clone()
    }
}

/// Metadata doc for the OI SST dataset. Crucially, `data_info` lives
/// here (per-dataset default) rather than on every data doc — the
/// generic transform layer reads it from the cache and stamps it onto
/// each data doc before column filtering.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OisstMeta {
    pub(crate) _id: String,
    pub(crate) data_type: String,
    pub data_info: DataInfo,
    pub(crate) date_updated_argovis: BsonDateTime,
    pub timeseries: Vec<BsonDateTime>,
    pub(crate) source: Vec<OisstSourceMeta>,
    pub(crate) lattice: Lattice,
}

impl IsTimeseriesMeta for OisstMeta {
    fn get_timeseries_meta(&self) -> bool {
        return true;
    }

    fn timeseries(&self) -> Vec<BsonDateTime> {
        self.timeseries.clone()
    }

    fn data_info(&self) -> DataInfo {
        self.data_info.clone()
    }
}

// copernicus sla /////////////////////////////////////////////////////////////

/// One spatial cell of the Copernicus sea level anomaly grid. Surface-only
/// (no vertical dimension; `level` is always `0.0`), exactly the OI SST
/// shape. `data` holds the timeseries per variable — up to six (sla, adt,
/// ugosa, ugos, vgosa, vgos), ordered per the meta doc's `data_info`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CopernicusSlaSchema {
    pub(crate) _id: String,
    // Reachable from main.rs (batchmeta branch reads `metadata()`), so
    // `pub` for symmetry with the other schemas.
    pub metadata: Vec<String>,
    pub(crate) basin: f64,
    pub(crate) geolocation: GeoJSONPoint,
    pub(crate) level: f64,
    // Omitted from the response when empty (no `data=` qsp); see the
    // matching annotation on `BsoseSchema.data` for the full reasoning.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) data: Vec<Vec<f64>>,
    // Like OI SST, data docs don't carry `timeseries` or `data_info` of
    // their own — both are populated at request time per the
    // response-shape rule (see the annotations on `OisstSchema`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timeseries: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data_info: Option<DataInfo>,
}

impl IsTimeseries for CopernicusSlaSchema {
    fn get_timeseries(&self) -> bool {
        return true;
    }

    fn data(&mut self) -> &mut Vec<Vec<f64>> {
        &mut self.data
    }

    fn set_data(&mut self, data: Vec<Vec<f64>>) {
        self.data = data;
    }

    fn timeseries(&mut self) -> Option<&mut Vec<String>> {
        self.timeseries.as_mut()
    }

    fn set_timeseries(&mut self, timeseries: Vec<String>) {
        self.timeseries = Some(timeseries);
    }

    fn data_info(&mut self) -> Option<DataInfo> {
        self.data_info.clone()
    }

    fn set_data_info(&mut self, data_info: Option<DataInfo>) {
        self.data_info = data_info;
    }

    fn _id(&self) -> String {
        self._id.clone()
    }

    fn longitude(&self) -> f64 {
        self.geolocation.coordinates[0]
    }

    fn latitude(&self) -> f64 {
        self.geolocation.coordinates[1]
    }

    fn level(&self) -> f64 {
        self.level
    }

    fn metadata(&self) -> Vec<String> {
        self.metadata.clone()
    }
}

/// Metadata doc for the Copernicus SLA dataset. Same layout as
/// `OisstMeta` — `data_info` lives here (per-dataset default) rather than
/// on every data doc, and the `source` / `lattice` substructures follow
/// the same pipeline conventions, so those structs are reused directly.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CopernicusSlaMeta {
    pub(crate) _id: String,
    pub(crate) data_type: String,
    pub data_info: DataInfo,
    pub(crate) date_updated_argovis: BsonDateTime,
    pub timeseries: Vec<BsonDateTime>,
    pub(crate) source: Vec<OisstSourceMeta>,
    pub(crate) lattice: Lattice,
}

impl IsTimeseriesMeta for CopernicusSlaMeta {
    fn get_timeseries_meta(&self) -> bool {
        return true;
    }

    fn timeseries(&self) -> Vec<BsonDateTime> {
        self.timeseries.clone()
    }

    fn data_info(&self) -> DataInfo {
        self.data_info.clone()
    }
}

// ccmp wind //////////////////////////////////////////////////////////////////

/// One spatial cell of the CCMP wind grid. Surface-only (no vertical
/// dimension; `level` is always `0.0`), exactly the Copernicus SLA
/// shape. `data` holds the timeseries per variable — up to four (uwnd,
/// vwnd, ws, nobs), ordered per the meta doc's `data_info`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CcmpWindSchema {
    pub(crate) _id: String,
    // Reachable from main.rs (batchmeta branch reads `metadata()`), so
    // `pub` for symmetry with the other schemas.
    pub metadata: Vec<String>,
    pub(crate) basin: f64,
    pub(crate) geolocation: GeoJSONPoint,
    pub(crate) level: f64,
    // Omitted from the response when empty (no `data=` qsp); see the
    // matching annotation on `BsoseSchema.data` for the full reasoning.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) data: Vec<Vec<f64>>,
    // Like the other surface grids, data docs don't carry `timeseries`
    // or `data_info` of their own — both are populated at request time
    // per the response-shape rule (see the annotations on `OisstSchema`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timeseries: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data_info: Option<DataInfo>,
}

impl IsTimeseries for CcmpWindSchema {
    fn get_timeseries(&self) -> bool {
        return true;
    }

    fn data(&mut self) -> &mut Vec<Vec<f64>> {
        &mut self.data
    }

    fn set_data(&mut self, data: Vec<Vec<f64>>) {
        self.data = data;
    }

    fn timeseries(&mut self) -> Option<&mut Vec<String>> {
        self.timeseries.as_mut()
    }

    fn set_timeseries(&mut self, timeseries: Vec<String>) {
        self.timeseries = Some(timeseries);
    }

    fn data_info(&mut self) -> Option<DataInfo> {
        self.data_info.clone()
    }

    fn set_data_info(&mut self, data_info: Option<DataInfo>) {
        self.data_info = data_info;
    }

    fn _id(&self) -> String {
        self._id.clone()
    }

    fn longitude(&self) -> f64 {
        self.geolocation.coordinates[0]
    }

    fn latitude(&self) -> f64 {
        self.geolocation.coordinates[1]
    }

    fn level(&self) -> f64 {
        self.level
    }

    fn metadata(&self) -> Vec<String> {
        self.metadata.clone()
    }
}

/// Metadata doc for the CCMP wind dataset. Same layout as
/// `CopernicusSlaMeta` / `OisstMeta` — `data_info` lives here
/// (per-dataset default) rather than on every data doc, and the
/// `source` / `lattice` substructures follow the same pipeline
/// conventions, so those structs are reused directly.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CcmpWindMeta {
    pub(crate) _id: String,
    pub(crate) data_type: String,
    pub data_info: DataInfo,
    pub(crate) date_updated_argovis: BsonDateTime,
    pub timeseries: Vec<BsonDateTime>,
    pub(crate) source: Vec<OisstSourceMeta>,
    pub(crate) lattice: Lattice,
}

impl IsTimeseriesMeta for CcmpWindMeta {
    fn get_timeseries_meta(&self) -> bool {
        return true;
    }

    fn timeseries(&self) -> Vec<BsonDateTime> {
        self.timeseries.clone()
    }

    fn data_info(&self) -> DataInfo {
        self.data_info.clone()
    }
}

// ///////////////////////////////////////////////////////////////////////////

#[derive(Deserialize, Debug, Clone)]
pub struct TimeseriesStub {
    pub _id: String,
    pub longitude: f64,
    pub latitude: f64,
    pub level: f64,
    pub metadata: Vec<String>,
}

impl Serialize for TimeseriesStub {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(5))?;
        seq.serialize_element(&self._id)?;
        seq.serialize_element(&self.longitude)?;
        seq.serialize_element(&self.latitude)?;
        seq.serialize_element(&self.level)?;
        seq.serialize_element(&self.metadata)?;
        seq.end()
    }
}

// profiles (in situ) /////////////////////////////////////////////////////////
//
// The profile family covers in situ point observations — Argo first. See
// ARGO-DESIGN.md for the full contrast with the timeseries family; the
// short version is a transposition: a timeseries doc is variables × time
// at a fixed (point, level) with a shared time axis, while a profile doc
// is variables × depth at a fixed (point, time) with the depth axis
// private to each document (pressure is itself one of the variables).
//
// Consequences visible in the types below:
//
//   - `timestamp` is a scalar per doc (a BSON DateTime in Mongo). Date
//     windowing is a Mongo *filter* for profiles, not a transform — so
//     there is no `timeseries` field and no shared-axis machinery here.
//   - there is no scalar `level` field; the water column lives in the
//     rows of `data`.
//   - `data` cells are nullable. Real Argo profiles carry nulls at
//     levels where a variable wasn't measured or didn't survive
//     upstream processing; a non-nullable matrix would silently drop
//     those docs at deserialization (cursor error), which is exactly
//     the kind of quiet data loss we refuse to ship.
//   - `data_info` is per-doc and *required semantically* (every Argo doc
//     carries its own, and core/BGC/deep docs differ), but typed
//     `Option` so the response-shape rule — scrub it unless the user
//     sent `data=` — works exactly as it does for the grids.

/// Serialize a `BsonDateTime` as the API's one canonical datetime
/// rendering (`helpers::bsondate2string`: fixed-millis RFC-3339,
/// `1999-08-05T19:03:42.002Z`; whole seconds render `.000`).
///
/// Response docs must carry human-readable timestamps, but the field
/// arrives from Mongo as a real BSON DateTime — so this is a
/// `serialize_with` adapter only, deliberately asymmetric:
/// deserialization stays the plain `BsonDateTime` impl. (The grids
/// never face this — no grid *data* doc carries a date; their time
/// axes are rendered to strings in the transform layer, through the
/// same one formatter.)
fn serialize_bsondate<S>(dt: &BsonDateTime, ser: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    ser.serialize_str(&super::helpers::bsondate2string(dt))
}

/// `Option`-wrapped flavour of `serialize_bsondate`, for optional
/// datetime fields (used with `skip_serializing_if = "Option::is_none"`,
/// so the `None` arm is unreachable in practice but total anyway).
fn serialize_opt_bsondate<S>(
    dt: &Option<BsonDateTime>,
    ser: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match dt {
        Some(d) => serialize_bsondate(d, ser),
        None => ser.serialize_none(),
    }
}

/// The profile-family sibling of `IsTimeseries`. Deliberately *not* a
/// shoehorn into that trait: `IsTimeseries` bakes in the shared time
/// axis (`timeseries` accessors) and a scalar `level`, neither of which
/// exists for a profile. What the two traits share is the response-shape
/// contract: `data` / `data_info` accessors that the transform layer
/// uses to filter columns and enforce the appears-iff-asked-for rules.
///
/// `timestamp` sits where `level` sits on the timeseries side — the
/// scalar that, together with the geolocation, pins the doc to a point
/// in the dataset's (space, page-axis) plane.
pub trait IsProfile {
    fn get_profile(&self) -> bool;
    /// The variables × levels matrix. Cells are nullable — see the
    /// family comment above. Row order across variables is lockstep:
    /// index `j` in every column refers to the same vertical level.
    fn data(&mut self) -> &mut Vec<Vec<Option<f64>>>;
    fn set_data(&mut self, data: Vec<Vec<Option<f64>>>);
    /// Per-doc variable descriptors. `Some` on a doc fresh off the
    /// cursor; the transform scrubs it to `None` unless the user sent
    /// `data=` (same response-shape rule as the grids).
    fn data_info(&mut self) -> Option<ProfileDataInfo>;
    fn set_data_info(&mut self, data_info: Option<ProfileDataInfo>);
    fn _id(&self) -> String;
    fn longitude(&self) -> f64;
    fn latitude(&self) -> f64;
    fn timestamp(&self) -> BsonDateTime;
    fn metadata(&self) -> Vec<String>;
}

/// One entry of an Argo doc's `source` array: which upstream pipelines
/// produced this profile (`argo_core`, `argo_bgc`, `argo_deep`, …), the
/// upstream file, and when it was last updated there. `url` and
/// `date_updated` are optional for tolerance across DACs and vintages;
/// the `source` tags themselves are the part queries will care about
/// (phase 3 `source=` filters).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArgoSourceMeta {
    pub(crate) source: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_bsondate"
    )]
    pub(crate) date_updated: Option<BsonDateTime>,
}

/// One Argo profile: a single float cycle, as represented in the `argo`
/// collection. `_id` is `{platform}_{cycle}` (e.g. `13857_068`).
///
/// Required fields are the structural core every profile carries
/// (identity, position, time, the data matrix and its descriptors, the
/// metadata pointer). Everything else is `Option` + `serde(default)` so
/// a doc missing an annotation field deserializes rather than silently
/// vanishing from a cursor — tolerance matters more here than for the
/// grids because Argo spans thirty years of heterogeneous provenance.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArgoSchema {
    pub(crate) _id: String,
    // Reachable from the handler's batchmeta branch (same as the grid
    // schemas), so fully `pub`.
    pub metadata: Vec<String>,
    pub(crate) geolocation: GeoJSONPoint,
    #[serde(serialize_with = "serialize_bsondate")]
    pub(crate) timestamp: BsonDateTime,
    pub(crate) cycle_number: i64,
    pub(crate) basin: f64,
    // Omitted from the response when empty (no `data=` qsp); see the
    // matching annotation on `BsoseSchema.data`. Cells are nullable —
    // JSON `null` in, JSON `null` out.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) data: Vec<Vec<Option<f64>>>,
    // Always present on the stored doc; scrubbed to `None` by the
    // transform unless the user sent `data=` (response-shape rule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data_info: Option<ProfileDataInfo>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_bsondate"
    )]
    pub(crate) date_updated_argovis: Option<BsonDateTime>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) source: Vec<ArgoSourceMeta>,
    // Doc-level QC flags (position and time). Distinct from the
    // per-variable `<var>_argoqc` columns inside `data`; these become
    // queryable in phase 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) geolocation_argoqc: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timestamp_argoqc: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) profile_direction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) vertical_sampling_scheme: Option<String>,
}

impl IsProfile for ArgoSchema {
    fn get_profile(&self) -> bool {
        return true;
    }

    fn data(&mut self) -> &mut Vec<Vec<Option<f64>>> {
        &mut self.data
    }

    fn set_data(&mut self, data: Vec<Vec<Option<f64>>>) {
        self.data = data;
    }

    fn data_info(&mut self) -> Option<ProfileDataInfo> {
        self.data_info.clone()
    }

    fn set_data_info(&mut self, data_info: Option<ProfileDataInfo>) {
        self.data_info = data_info;
    }

    fn _id(&self) -> String {
        self._id.clone()
    }

    fn longitude(&self) -> f64 {
        self.geolocation.coordinates[0]
    }

    fn latitude(&self) -> f64 {
        self.geolocation.coordinates[1]
    }

    fn timestamp(&self) -> BsonDateTime {
        self.timestamp
    }

    fn metadata(&self) -> Vec<String> {
        self.metadata.clone()
    }
}

/// One per-platform metadata doc from the `argoMeta` collection (`_id`
/// like `13857_m0`, referenced by the data docs' `metadata` arrays).
///
/// Unlike the grid metas there is no `IsTimeseriesMeta` impl and no
/// startup load: Argo has thousands of these, one(ish) per platform,
/// and the only phase-1 consumer is the `batchmeta` branch — which
/// fetches raw `Document`s and doesn't need this type at all. The typed
/// struct exists for what comes later (a `/argo/meta` route, platform
/// validation), and to pin down the field vocabulary now while the
/// example doc is in front of us. Everything beyond the identity core
/// is `Option` — DAC practices vary.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArgoMeta {
    pub(crate) _id: String,
    pub(crate) data_type: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data_center: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) instrument: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pi_name: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) platform_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fleetmonitoring: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) oceanops: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) positioning_system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) wmo_inst_type: Option<String>,
}

/// The profile flavour of `TimeseriesStub` (`compression=minimal`):
/// `[_id, lon, lat, timestamp, metadata]` — timestamp (pre-rendered
/// RFC-3339 string) sits in the slot `level` occupies for the grids,
/// the same "scalar that pins the doc on the page axis" role.
#[derive(Deserialize, Debug, Clone)]
pub struct ProfileStub {
    pub _id: String,
    pub longitude: f64,
    pub latitude: f64,
    pub timestamp: String,
    pub metadata: Vec<String>,
}

impl ProfileStub {
    /// Project a profile down to its summary stub. Sibling of
    /// `transforms::timeseries_stub`; lives here (rather than in
    /// `transforms`) so the schema family is self-contained until
    /// `transform_profile` lands.
    pub fn from_profile<T: IsProfile>(p: &T) -> ProfileStub {
        ProfileStub {
            _id: p._id(),
            longitude: p.longitude(),
            latitude: p.latitude(),
            timestamp: super::helpers::bsondate2string(&p.timestamp()),
            metadata: p.metadata(),
        }
    }
}

impl Serialize for ProfileStub {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(5))?;
        seq.serialize_element(&self._id)?;
        seq.serialize_element(&self.longitude)?;
        seq.serialize_element(&self.latitude)?;
        seq.serialize_element(&self.timestamp)?;
        seq.serialize_element(&self.metadata)?;
        seq.end()
    }
}
// ///////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::{doc, from_document, Bson, Document};

    /// A trimmed-but-faithful replica of the example doc in
    /// `argo_schema_example.txt` (argo.findOne for 13857_068), built as a
    /// BSON `Document` because that is exactly what the Mongo cursor
    /// hands the deserializer. Three deliberate fidelity points:
    ///
    ///   - a `null` in the temperature column (real Argo data has them),
    ///   - `[null, null]` rows in `data_info`'s per-variable block (the
    ///     QC companion columns carry no units / data_keys_mode),
    ///   - `basin`, `cycle_number`, and the QC flags stored as BSON
    ///     Int32, the way the ingestion pipeline writes them.
    fn example_argo_doc() -> Document {
        doc! {
            "_id": "13857_068",
            "geolocation": { "type": "Point", "coordinates": [-26.257, 3.427] },
            "basin": 1_i32,
            "timestamp": BsonDateTime::from_millis(933879822002), // 1999-08-05T19:03:42.002Z
            "date_updated_argovis": BsonDateTime::from_millis(1771656291684),
            "source": [{
                "source": ["argo_core"],
                "url": "ftp://ftp.ifremer.fr/ifremer/argo/dac/aoml/13857/profiles/D13857_068.nc",
                "date_updated": BsonDateTime::from_millis(1771598133000),
            }],
            "cycle_number": 68_i32,
            "geolocation_argoqc": 1_i32,
            "profile_direction": "A",
            "timestamp_argoqc": 1_i32,
            "vertical_sampling_scheme": "Primary sampling: discrete []",
            "data": [
                [11.9, 17.0, 22.1],                     // pressure
                [2_i32, 2_i32, 2_i32],                  // pressure_argoqc (ints on disk)
                [28.021, Bson::Null, 27.969],           // temperature — with a real null
                [2_i32, 2_i32, 2_i32],                  // temperature_argoqc
            ],
            "data_info": [
                ["pressure", "pressure_argoqc", "temperature", "temperature_argoqc"],
                ["units", "data_keys_mode"],
                [
                    ["decibar", "D"],
                    [Bson::Null, Bson::Null],
                    ["degree_Celsius", "D"],
                    [Bson::Null, Bson::Null],
                ],
            ],
            "metadata": ["13857_m0"],
        }
    }

    // ---- deserialization ----------------------------------------------------

    #[test]
    fn argo_doc_deserializes_with_nulls_and_int32s() {
        let p: ArgoSchema = from_document(example_argo_doc()).expect("should deserialize");
        assert_eq!(p._id, "13857_068");
        assert!((p.geolocation.coordinates[0] - -26.257).abs() < 1e-9);
        assert!((p.geolocation.coordinates[1] - 3.427).abs() < 1e-9);
        assert_eq!(p.timestamp.timestamp_millis(), 933879822002);
        assert_eq!(p.cycle_number, 68);
        assert!((p.basin - 1.0).abs() < 1e-9);
        assert_eq!(p.geolocation_argoqc, Some(1));
        assert_eq!(p.timestamp_argoqc, Some(1));
        assert_eq!(p.profile_direction.as_deref(), Some("A"));
        assert_eq!(p.metadata, vec!["13857_m0".to_string()]);

        // The data matrix: nullable cells, int QC columns cast to f64.
        assert_eq!(p.data.len(), 4);
        assert_eq!(p.data[0][0], Some(11.9));
        assert_eq!(p.data[1][0], Some(2.0), "Int32 QC values cast to f64");
        assert_eq!(p.data[2][1], None, "null measurement survives as None");
        assert_eq!(p.data[2][2], Some(27.969));

        // data_info: names intact, per-variable info rows with None cells.
        let di = p.data_info.expect("stored doc carries data_info");
        assert_eq!(di.0[2], "temperature");
        assert_eq!(di.1, vec!["units".to_string(), "data_keys_mode".to_string()]);
        assert_eq!(di.2[0], vec![Some("decibar".to_string()), Some("D".to_string())]);
        assert_eq!(di.2[1], vec![None, None], "QC column info nulls survive");

        // source array.
        assert_eq!(p.source.len(), 1);
        assert_eq!(p.source[0].source, vec!["argo_core".to_string()]);
        assert!(p.source[0].url.as_deref().unwrap().starts_with("ftp://"));
    }

    #[test]
    fn argo_doc_missing_optional_fields_still_deserializes() {
        // Strip every tolerance-flagged field; the structural core must
        // still deserialize (a doc from an old vintage or a sparse DAC
        // must not silently vanish from a cursor).
        let mut d = example_argo_doc();
        for k in [
            "date_updated_argovis",
            "source",
            "geolocation_argoqc",
            "timestamp_argoqc",
            "profile_direction",
            "vertical_sampling_scheme",
        ] {
            d.remove(k);
        }
        let p: ArgoSchema = from_document(d).expect("sparse doc should deserialize");
        assert_eq!(p._id, "13857_068");
        assert_eq!(p.geolocation_argoqc, None);
        assert!(p.source.is_empty());
    }

    #[test]
    fn argo_doc_missing_structural_field_fails_loudly() {
        // Sanity check on the required/optional split: a doc with no
        // timestamp is malformed, not tolerable.
        let mut d = example_argo_doc();
        d.remove("timestamp");
        assert!(from_document::<ArgoSchema>(d).is_err());
    }

    // ---- response serialization ---------------------------------------------

    #[test]
    fn argo_response_timestamp_is_rfc3339_string_with_millis() {
        let p: ArgoSchema = from_document(example_argo_doc()).unwrap();
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v["timestamp"], "1999-08-05T19:03:42.002Z",
            "timestamp serializes as an RFC-3339 string, millis preserved"
        );
        assert_eq!(
            v["source"][0]["date_updated"], "2026-02-20T14:35:33.000Z",
            "whole-second datetimes pad to .000 — fixed-width millis, no mixed formats"
        );
    }

    #[test]
    fn argo_response_null_data_cells_serialize_as_json_null() {
        let p: ArgoSchema = from_document(example_argo_doc()).unwrap();
        let v = serde_json::to_value(&p).unwrap();
        assert!(v["data"][2][1].is_null());
        assert_eq!(v["data"][2][2], 27.969);
        // data_info None cells also round-trip as null.
        assert!(v["data_info"][2][1][0].is_null());
    }

    #[test]
    fn argo_response_omits_scrubbed_fields() {
        // Simulate the slim-listing shape the transform will produce:
        // data cleared, data_info scrubbed. Neither key may appear.
        let mut p: ArgoSchema = from_document(example_argo_doc()).unwrap();
        p.set_data(Vec::new());
        p.set_data_info(None);
        let v = serde_json::to_value(&p).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("data"), "empty data must be omitted");
        assert!(!obj.contains_key("data_info"), "scrubbed data_info must be omitted");
        // The identity core still rides along.
        assert!(obj.contains_key("_id"));
        assert!(obj.contains_key("timestamp"));
        assert!(obj.contains_key("metadata"));
    }

    // ---- IsProfile ----------------------------------------------------------

    #[test]
    fn is_profile_getters_project_the_doc() {
        let p: ArgoSchema = from_document(example_argo_doc()).unwrap();
        assert_eq!(p._id(), "13857_068");
        assert!((p.longitude() - -26.257).abs() < 1e-9);
        assert!((p.latitude() - 3.427).abs() < 1e-9);
        assert_eq!(IsProfile::timestamp(&p).timestamp_millis(), 933879822002);
        assert_eq!(IsProfile::metadata(&p), vec!["13857_m0".to_string()]);
    }

    // ---- ProfileStub --------------------------------------------------------

    #[test]
    fn profile_stub_serializes_as_five_element_array() {
        let p: ArgoSchema = from_document(example_argo_doc()).unwrap();
        let stub = ProfileStub::from_profile(&p);
        let v = serde_json::to_value(&stub).unwrap();
        let arr = v.as_array().expect("stub serializes as a JSON array");
        assert_eq!(arr.len(), 5);
        assert_eq!(arr[0], "13857_068");
        assert!((arr[1].as_f64().unwrap() - -26.257).abs() < 1e-9);
        assert!((arr[2].as_f64().unwrap() - 3.427).abs() < 1e-9);
        assert_eq!(arr[3], "1999-08-05T19:03:42.002Z", "timestamp in the level slot");
        assert_eq!(arr[4], serde_json::json!(["13857_m0"]));
    }

    // ---- ArgoMeta -----------------------------------------------------------

    #[test]
    fn argo_meta_deserializes_from_platform_doc() {
        // Faithful to the argoMeta.findOne example.
        let d = doc! {
            "_id": "13857_m0",
            "data_type": "oceanicProfile",
            "data_center": "AO",
            "instrument": "profiling_float",
            "pi_name": ["BOB MOLINARI"],
            "platform": "13857",
            "platform_type": "PALACE",
            "fleetmonitoring": "https://fleetmonitoring.euro-argo.eu/float/13857",
            "oceanops": "https://www.ocean-ops.org/board/wa/Platform?ref=13857",
            "positioning_system": "ARGOS",
            "wmo_inst_type": "845",
        };
        let m: ArgoMeta = from_document(d).expect("meta doc should deserialize");
        assert_eq!(m._id, "13857_m0");
        assert_eq!(m.platform, "13857");
        assert_eq!(m.pi_name, Some(vec!["BOB MOLINARI".to_string()]));
        assert_eq!(m.wmo_inst_type.as_deref(), Some("845"));
    }

    #[test]
    fn argo_meta_tolerates_sparse_docs() {
        let d = doc! {
            "_id": "999_m0",
            "data_type": "oceanicProfile",
            "platform": "999",
        };
        let m: ArgoMeta = from_document(d).expect("sparse meta should deserialize");
        assert_eq!(m.data_center, None);
        assert_eq!(m.pi_name, None);
    }
}
