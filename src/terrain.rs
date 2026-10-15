//! Per-cell terrain facts derived from the `.smf` grids and `mapinfo` terrain types.
//!
//! Analysis runs on the map's half-resolution grid, the native resolution of the
//! typemap and metalmap: one cell spans two heightmap squares, so `CELL_SIZE`
//! world units. Height and slope are reduced from the heightmap corners covering
//! each cell.

use crate::map::LoadedMap;
use crate::mapinfo::TerrainType;
use crate::smf::SQUARE_SIZE;

/// World units spanned by one analysis cell.
pub const CELL_SIZE: i32 = SQUARE_SIZE * 2;

/// How a cell relates to the waterline, in the bands that matter for movement.
///
/// Bands are ordered by depth. Note that ground and naval access overlap: a hull
/// floats in less water than a bot can wade through, so [`Band::Shoal`] admits
/// both. Boundaries come from [`Thresholds`] rather than unit definitions,
/// because the map file cannot know which units a game enables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    /// At or above the waterline.
    Land,
    /// Wadeable by ground units, too shallow to float a hull.
    Shallows,
    /// Deep enough for hulls, still shallow enough to wade.
    Shoal,
    /// Navigable by ships only.
    Deep,
}

impl Band {
    pub fn is_water(self) -> bool {
        self != Band::Land
    }

    /// Whether ground units can stand or wade here.
    pub fn is_wadeable(self) -> bool {
        matches!(self, Band::Land | Band::Shallows | Band::Shoal)
    }

    /// Whether a hull floats here.
    pub fn is_navigable(self) -> bool {
        matches!(self, Band::Shoal | Band::Deep)
    }
}

/// Depth and slope boundaries used to classify cells.
///
/// Slopes are given in degrees and converted internally to the engine's
/// `1 - cos(theta)` measure, which is what `slopeMap` stores and what MoveDef
/// `maxslope` is compared against.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Depth below which ground units can still wade, in world units.
    pub wade_depth: f32,
    /// Minimum depth a hull needs to float.
    pub hull_depth: f32,
    /// Steepest ground tracked and wheeled units accept, in degrees.
    pub vehicle_max_slope_deg: f32,
    /// Steepest ground legged units accept, in degrees.
    pub bot_max_slope_deg: f32,
}

impl Default for Thresholds {
    /// Taken from Beyond All Reason's own `gamedata/movedefs.lua` constants:
    /// `DEPTH.MAX_SHALLOW` (20) is the default wading limit for land units,
    /// `DEPTH.MIN_SHALLOW` (8) the default draft for ships, `SLOPE.MINIMUM` (27)
    /// the common tank limit and `SLOPE.DIFFICULT` (54) the common bot limit.
    fn default() -> Self {
        Self {
            wade_depth: 20.0,
            hull_depth: 8.0,
            vehicle_max_slope_deg: 27.0,
            bot_max_slope_deg: 54.0,
        }
    }
}

impl Thresholds {
    fn vehicle_max_slope(self) -> f32 {
        degrees_to_slope(self.vehicle_max_slope_deg)
    }

    fn bot_max_slope(self) -> f32 {
        degrees_to_slope(self.bot_max_slope_deg)
    }
}

/// Convert an incline in degrees to the engine's slope measure, mirroring
/// `DegreesToMaxSlope` in `MoveDefHandler.cpp`.
pub fn degrees_to_slope(degrees: f32) -> f32 {
    1.0 - degrees.clamp(0.0, 90.0).to_radians().cos()
}

/// Movement classes the map data can speak about, matching `mapinfo.terrainTypes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveClass {
    Bot,
    Vehicle,
    Hover,
    Ship,
}

impl MoveClass {
    pub const ALL: [MoveClass; 4] =
        [MoveClass::Bot, MoveClass::Vehicle, MoveClass::Hover, MoveClass::Ship];

    /// Terrain-type speed multiplier for this class.
    fn speed(self, terrain: &TerrainType) -> f32 {
        match self {
            MoveClass::Bot => terrain.kbot,
            MoveClass::Vehicle => terrain.tank,
            MoveClass::Hover => terrain.hover,
            MoveClass::Ship => terrain.ship,
        }
    }
}

/// Derived terrain fields at analysis resolution.
pub struct TerrainGrid {
    pub width: usize,
    pub height: usize,
    pub thresholds: Thresholds,
    /// Mean surface height per cell, world units, positive above the waterline.
    pub elevation: Vec<f32>,
    /// Cell steepness in the engine's `1 - cos(theta)` measure, matching
    /// `slopeMap`, which is stored at exactly this resolution.
    pub slope: Vec<f32>,
    pub band: Vec<Band>,
    /// Terrain type index from the typemap.
    pub type_index: Vec<u8>,
    /// Raw metal density, as stored in the metalmap.
    pub metal: Vec<u8>,
}

impl TerrainGrid {
    pub fn len(&self) -> usize {
        self.width * self.height
    }

    pub fn index(&self, x: usize, z: usize) -> usize {
        z * self.width + x
    }

    /// World-space centre of a cell.
    pub fn cell_centre(&self, x: usize, z: usize) -> (f32, f32) {
        let half = CELL_SIZE as f32 / 2.0;
        (x as f32 * CELL_SIZE as f32 + half, z as f32 * CELL_SIZE as f32 + half)
    }

    /// Cell containing a world position, clamped to the grid.
    pub fn cell_at(&self, x: f32, z: f32) -> (usize, usize) {
        let cx = (x / CELL_SIZE as f32).floor().max(0.0) as usize;
        let cz = (z / CELL_SIZE as f32).floor().max(0.0) as usize;
        (cx.min(self.width - 1), cz.min(self.height - 1))
    }

    /// Whether `class` can occupy a cell, combining terrain-type speed, depth band
    /// and slope. Speed of zero means the terrain type forbids the class outright.
    pub fn passable(&self, index: usize, class: MoveClass, types: &[TerrainType]) -> bool {
        let terrain = &types[self.type_index[index] as usize];
        if class.speed(terrain) <= 0.0 {
            return false;
        }
        let band = self.band[index];
        let slope = self.slope[index];

        match class {
            MoveClass::Ship => band.is_navigable(),
            // Hovercraft ignore the waterline but still cannot climb cliffs.
            MoveClass::Hover => slope <= self.thresholds.vehicle_max_slope(),
            MoveClass::Bot => band.is_wadeable() && slope <= self.thresholds.bot_max_slope(),
            MoveClass::Vehicle => {
                band.is_wadeable() && slope <= self.thresholds.vehicle_max_slope()
            }
        }
    }

    /// Mean traversal speed for `class` over cells it can occupy, and the fraction
    /// of the map it can occupy at all.
    pub fn mobility(&self, class: MoveClass, types: &[TerrainType]) -> Mobility {
        let mut reachable = 0usize;
        let mut speed_sum = 0.0f64;

        for index in 0..self.len() {
            if !self.passable(index, class, types) {
                continue;
            }
            reachable += 1;
            speed_sum += class.speed(&types[self.type_index[index] as usize]) as f64;
        }

        Mobility {
            class,
            coverage: reachable as f32 / self.len() as f32,
            mean_speed: if reachable == 0 { 0.0 } else { (speed_sum / reachable as f64) as f32 },
        }
    }
}

/// How well a movement class gets around this map.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Mobility {
    pub class: MoveClass,
    /// Fraction of cells the class can occupy.
    pub coverage: f32,
    /// Mean terrain-type speed multiplier over occupiable cells.
    pub mean_speed: f32,
}

/// Build the analysis grid for a loaded map.
pub fn build(map: &LoadedMap, thresholds: Thresholds) -> TerrainGrid {
    let width = map.smf.half_width();
    let height = map.smf.half_height();
    let (min_height, max_height) = map.height_range();

    // `mapinfo.smf` may override the header's height range; rescale the decoded
    // samples so the waterline lands where the engine would put it.
    let header = &map.smf.header;
    let rescale = |h: f32| {
        let span = header.max_height - header.min_height;
        if span.abs() < f32::EPSILON {
            return h;
        }
        let unit = (h - header.min_height) / span;
        min_height + unit * (max_height - min_height)
    };

    let mut elevation = Vec::with_capacity(width * height);
    let mut slope = Vec::with_capacity(width * height);
    let mut band = Vec::with_capacity(width * height);

    // Surface normal of one heightmap square, from central differences over its
    // four corners. Only the y component matters: it is the cosine of the
    // incline, which is what the engine's slope map is built from.
    let normal_y = |sx: usize, sz: usize| {
        let h00 = rescale(map.smf.height_at_corner(sx, sz));
        let h10 = rescale(map.smf.height_at_corner(sx + 1, sz));
        let h01 = rescale(map.smf.height_at_corner(sx, sz + 1));
        let h11 = rescale(map.smf.height_at_corner(sx + 1, sz + 1));
        let run = 2.0 * SQUARE_SIZE as f32;
        let dx = ((h10 + h11) - (h00 + h01)) / run;
        let dz = ((h01 + h11) - (h00 + h10)) / run;
        (dx * dx + dz * dz + 1.0).sqrt().recip()
    };

    for cz in 0..height {
        for cx in 0..width {
            // Each cell spans two squares, so three heightmap corners per axis.
            let mut sum = 0.0f32;
            for dz in 0..=2 {
                for dx in 0..=2 {
                    sum += rescale(map.smf.height_at_corner(cx * 2 + dx, cz * 2 + dz));
                }
            }
            let mean = sum / 9.0;

            // `ReadMap::UpdateHeightMapSynced` blends the steepest face normal in
            // the cell toward the cell average, so a single narrow crack does not
            // read as impassable. Reproduce that blend over the four squares.
            let mut steepest = f32::INFINITY;
            let mut total = 0.0f32;
            for dz in 0..2 {
                for dx in 0..2 {
                    let y = normal_y(cx * 2 + dx, cz * 2 + dz);
                    steepest = steepest.min(y);
                    total += y;
                }
            }
            let average = total / 4.0;
            let blended = steepest + (average - steepest) * (steepest / average);

            elevation.push(mean);
            slope.push(1.0 - blended);
            band.push(classify(mean, thresholds));
        }
    }

    TerrainGrid {
        width,
        height,
        thresholds,
        elevation,
        slope,
        band,
        type_index: map.smf.type_map.clone(),
        metal: map.smf.metal_map.clone(),
    }
}

fn classify(elevation: f32, thresholds: Thresholds) -> Band {
    let depth = -elevation;
    // Guard against a caller inverting the two depths, which would otherwise
    // silently produce a band that can never occur.
    let float_at = thresholds.hull_depth.min(thresholds.wade_depth);
    let wade_to = thresholds.wade_depth.max(thresholds.hull_depth);

    if depth <= 0.0 {
        Band::Land
    } else if depth <= float_at {
        Band::Shallows
    } else if depth <= wade_to {
        Band::Shoal
    } else {
        Band::Deep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> Thresholds {
        Thresholds::default()
    }

    #[test]
    fn slope_conversion_matches_engine_measure() {
        assert!((degrees_to_slope(0.0) - 0.0).abs() < 1e-6);
        assert!((degrees_to_slope(60.0) - 0.5).abs() < 1e-6);
        assert!((degrees_to_slope(90.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bands_follow_depth_order() {
        assert_eq!(classify(10.0, thresholds()), Band::Land);
        assert_eq!(classify(0.0, thresholds()), Band::Land);
        assert_eq!(classify(-5.0, thresholds()), Band::Shallows);
        assert_eq!(classify(-15.0, thresholds()), Band::Shoal);
        assert_eq!(classify(-40.0, thresholds()), Band::Deep);
    }

    #[test]
    fn ground_and_naval_access_overlap_in_the_shoal() {
        assert!(Band::Shoal.is_wadeable());
        assert!(Band::Shoal.is_navigable());
        assert!(!Band::Shallows.is_navigable());
        assert!(!Band::Deep.is_wadeable());
    }

    #[test]
    fn inverted_thresholds_still_yield_every_band() {
        let inverted = Thresholds { wade_depth: 8.0, hull_depth: 20.0, ..thresholds() };
        assert_eq!(classify(-5.0, inverted), Band::Shallows);
        assert_eq!(classify(-15.0, inverted), Band::Shoal);
        assert_eq!(classify(-40.0, inverted), Band::Deep);
    }
}
