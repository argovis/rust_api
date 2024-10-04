use serde::{Deserialize, Serialize};
use mongodb::bson::DateTime;

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
    data: Vec<Vec<f64>>
}

impl IsTimeseries for BsoseSchema {
    fn get_timeseries(&self) -> bool {
        return true;
    }

    fn data(&mut self) -> &mut Vec<Vec<f64>> {
        &mut self.data
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BsoseMeta { 
    _id: String,
    data_type: String,
    pub data_info: (Vec<String>, Vec<String>, Vec<Vec<String>>),
    date_updated_argovis: DateTime,
    pub timeseries: Vec<DateTime>,
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