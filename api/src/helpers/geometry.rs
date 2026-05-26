//! Shared spatial primitives used across the tile generator, the filter
//! composer, and the dataset config.
//!
//! Kept in its own module so that `dataset_config` can express coverage
//! regions without depending on `tile_generator`, and vice versa — both
//! reach for `BoundingBox` for unrelated reasons (one to describe a tile,
//! one to describe where a dataset has data), and neither should be a
//! parent of the other in the module graph.

/// A longitude/latitude bounding box. `sw` is the south-west corner
/// (min lon, min lat); `ne` is the north-east corner (max lon, max lat).
///
/// Tile bboxes use this type as half-open intervals: `[sw, ne)` on both
/// axes (the half-open behaviour is enforced by `filter_composer` shrinking
/// the NE corner of the underlying GeoJSON polygon, not by this type).
///
/// Dataset coverage bboxes use this type as inclusive intervals: a doc at
/// lat == cov.ne[1] is considered "covered" so it isn't dropped at the
/// dataset's edge.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundingBox {
    pub sw: [f64; 2],
    pub ne: [f64; 2],
}

impl BoundingBox {
    /// Permissive overlap test using closed-interval semantics on both
    /// boxes. Returns true if the two bboxes share any region, including
    /// touching only at an edge or a corner.
    ///
    /// Closed semantics are deliberately chosen for the
    /// tile-vs-coverage-bbox use case: a tile whose SW edge sits exactly
    /// on the coverage's NE edge gets *kept*, so a grid-aligned dataset
    /// whose data extends to (and includes) the coverage boundary doesn't
    /// lose its edge cells. The cost is one extra tile worth of probing
    /// at the boundary, which is negligible.
    pub fn overlaps(&self, other: &BoundingBox) -> bool {
        self.sw[0] <= other.ne[0]
            && self.ne[0] >= other.sw[0]
            && self.sw[1] <= other.ne[1]
            && self.ne[1] >= other.sw[1]
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bb(sw: [f64; 2], ne: [f64; 2]) -> BoundingBox {
        BoundingBox { sw, ne }
    }

    #[test]
    fn overlaps_disjoint_returns_false() {
        let a = bb([0.0, 0.0], [10.0, 10.0]);
        let b = bb([20.0, 20.0], [30.0, 30.0]);
        assert!(!a.overlaps(&b));
        assert!(!b.overlaps(&a));
    }

    #[test]
    fn overlaps_fully_contained_returns_true() {
        let outer = bb([0.0, 0.0], [100.0, 100.0]);
        let inner = bb([10.0, 10.0], [20.0, 20.0]);
        assert!(outer.overlaps(&inner));
        assert!(inner.overlaps(&outer));
    }

    #[test]
    fn overlaps_partial_returns_true() {
        let a = bb([0.0, 0.0], [10.0, 10.0]);
        let b = bb([5.0, 5.0], [15.0, 15.0]);
        assert!(a.overlaps(&b));
    }

    #[test]
    fn overlaps_touching_at_edge_returns_true() {
        // The whole point of the permissive ≤/≥ test: bboxes that share
        // exactly an edge count as overlapping. Used by the coverage
        // filter so tiles at the coverage boundary aren't dropped.
        let a = bb([0.0, 0.0], [10.0, 10.0]);
        let b = bb([10.0, 0.0], [20.0, 10.0]);
        assert!(a.overlaps(&b));
    }

    #[test]
    fn overlaps_touching_at_corner_returns_true() {
        let a = bb([0.0, 0.0], [10.0, 10.0]);
        let b = bb([10.0, 10.0], [20.0, 20.0]);
        assert!(a.overlaps(&b));
    }
}
