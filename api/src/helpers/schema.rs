use serde::{Deserialize, Serialize};
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
}

pub trait IsTimeseriesMeta {
    fn get_timeseries_meta(&self) -> bool;
}

// bsose //////////////////////////////////////////////////////////////////////

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BsoseSchema {
    _id: String,
    metadata: Vec<String>,
    basin: f64,
    geolocation: GeoJSONPoint,
    level: f64,
    cell_vertical_fraction: f64,
    sea_binary_mask_at_t_locaiton: bool,
    ctrl_vector_3d_mask: bool,
    cell_z_size: f64,
    reference_density_profile: f64,
    data: Vec<Vec<f64>>,
    timeseries: Option<Vec<String>> // since this field isnt present in the data collection, but gets munged on later
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