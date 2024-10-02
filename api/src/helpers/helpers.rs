pub fn validlonlat(coords: Vec<Vec<f64>>) -> Vec<Vec<f64>> {
    coords.into_iter().map(|mut pair| {
        if pair.len() == 2 {
            pair[0] = pair[0] % 360.0;
            pair[0] = if pair[0] > 180.0 { pair[0] - 360.0 } else if pair[0] < -180.0 { pair[0] + 360.0 } else { pair[0] };
            pair[1] = pair[1] % 180.0;
            pair[1] = if pair[1] > 90.0 { 90.0 } else if pair[1] < -90.0 { -90.0 } else { pair[1] };
        }
        pair
    }).collect()
}