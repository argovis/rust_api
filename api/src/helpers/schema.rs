use serde::{Deserialize, Serialize};
use serde::ser::{Serializer, SerializeSeq};
use mongodb::bson::DateTime as BsonDateTime;

// generic structs ////////////////////////////////////////////////////////////

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeoJSONPoint {
    #[serde(rename = "type")]
    location_type: String,
    coordinates: [f64; 2],
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SourceMeta { 
    source: Vec<String>,
    file: String
}

// categroical traits /////////////////////////////////////////////////////////

pub trait IsTimeseries {
    fn get_timeseries(&self) -> bool;
    fn data(&mut self) -> &mut Vec<Vec<f64>>;
    fn set_data(&mut self, data: Vec<Vec<f64>>);
    fn timeseries(&mut self) -> Option<&mut Vec<String>>;
    fn set_timeseries(&mut self, timeseries: Vec<String>);
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
    _id: String,
    pub metadata: Vec<String>,
    basin: f64,
    geolocation: GeoJSONPoint,
    level: f64,
    cell_vertical_fraction: f64,
    sea_binary_mask_at_t_locaiton: bool,
    ctrl_vector_3d_mask: bool,
    cell_z_size: f64,
    reference_density_profile: f64,
    data: Vec<Vec<f64>>,
    timeseries: Option<Vec<String>>, // since this field isnt present in the data collection, but gets munged on later
    data_info: Option<(Vec<String>, Vec<String>, Vec<Vec<String>>)>,
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

    fn set_data_info(&mut self, data_info: (Vec<String>, Vec<String>, Vec<Vec<String>>)) {
        self.data_info = Some(data_info);
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
    _id: String,
    data_type: String,
    pub data_info: (Vec<String>, Vec<String>, Vec<Vec<String>>),
    date_updated_argovis: BsonDateTime,
    pub timeseries: Vec<BsonDateTime>,
    source: Vec<SourceMeta>,
    cell_area: f64,
    ocean_depth: f64,
    depth_r0_to_bottom: f64,
    interior_2d_mask: bool,
    depth_r0_to_ref_surface: f64
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