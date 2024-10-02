use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeoJSONPoint {
    #[serde(rename = "type")]
    location_type: String,
    coordinates: [f64; 2],
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DataSchema {
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