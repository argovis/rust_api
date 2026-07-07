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