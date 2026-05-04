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

// categroical traits /////////////////////////////////////////////////////////

pub trait IsTimeseries {
    fn get_timeseries(&self) -> bool;
    fn data(&mut self) -> &mut Vec<Vec<f64>>;
    fn set_data(&mut self, data: Vec<Vec<f64>>);
    fn timeseries(&mut self) -> Option<&mut Vec<String>>;
    fn set_timeseries(&mut self, timeseries: Vec<String>);
    fn data_info(&mut self) -> (Vec<String>, Vec<String>, Vec<Vec<String>>);
    fn set_data_info(&mut self, data_info: (Vec<String>, Vec<String>, Vec<Vec<String>>));
    fn _id(&self) -> String;
    fn longitude(&self) -> f64;
    fn latitude(&self) -> f64;
    fn level(&self) -> f64;
    fn metadata(&self) -> Vec<String>;
}

pub trait IsTimeseriesMeta {
    fn get_timeseries_meta(&self) -> bool;
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
    pub(crate) sea_binary_mask_at_t_location: bool,
    pub(crate) cell_z_size: f64,
    pub(crate) reference_density_profile: f64,
    pub(crate) data: Vec<Vec<f64>>,
    // Not present in the source collection — gets populated by transforms.
    pub(crate) timeseries: Option<Vec<String>>,
    pub(crate) data_info: (Vec<String>, Vec<String>, Vec<Vec<String>>),
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

    fn data_info(&mut self) -> (Vec<String>, Vec<String>, Vec<Vec<String>>) {
        self.data_info.clone()
    }

    fn set_data_info(&mut self, data_info: (Vec<String>, Vec<String>, Vec<Vec<String>>)) {
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
    // `timeseries` is read from main.rs at startup to populate the cached
    // TIMESERIES global, so it stays fully `pub`.
    pub timeseries: Vec<BsonDateTime>,
    pub(crate) source: Vec<SourceMeta>,
    pub(crate) cell_area: f64,
    pub(crate) ocean_depth: f64,
    pub(crate) depth_r0_to_bottom: f64,
    pub(crate) interior_2d_mask: bool,
    pub(crate) depth_r0_to_ref_surface: f64,
}

impl IsTimeseriesMeta for BsoseMeta {
    fn get_timeseries_meta(&self) -> bool {
        return true;
    }
}

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