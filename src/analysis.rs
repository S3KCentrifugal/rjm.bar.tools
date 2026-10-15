//! Derived map facts: connectivity, metal spots, and per-start-position terrain.
//!
//! Everything here is a pure function of the archive contents, so two runs over
//! the same archive produce byte-identical output. Where a choice is arbitrary
//! (tie-breaking, iteration order) the lowest cell index wins.

use crate::map::LoadedMap;
use crate::mapinfo::TerrainType;
use crate::terrain::{Band, CELL_SIZE, MoveClass, Mobility, TerrainGrid};
use serde::Serialize;
use std::collections::VecDeque;

/// Radius in world units over which a start position's surroundings are sampled.
/// Roughly the area an opening commits to before the first engagement.
const START_SAMPLE_RADIUS: f32 = 1500.0;

/// Connected components of cells passable by one movement class.
pub struct Components {
    /// Component id per cell; `None` where the class cannot go.
    pub label: Vec<Option<u32>>,
    /// Cell count per component id.
    pub sizes: Vec<usize>,
}

impl Components {
    /// Fraction of the whole grid occupied by component `id`.
    pub fn coverage(&self, id: u32, grid_len: usize) -> f32 {
        self.sizes.get(id as usize).copied().unwrap_or(0) as f32 / grid_len as f32
    }
}

/// Label connected regions passable by `class`, using 4-connectivity.
pub fn components(grid: &TerrainGrid, class: MoveClass, types: &[TerrainType]) -> Components {
    let mut label = vec![None; grid.len()];
    let mut sizes = Vec::new();

    for start in 0..grid.len() {
        if label[start].is_some() || !grid.passable(start, class, types) {
            continue;
        }

        let id = sizes.len() as u32;
        let mut size = 0usize;
        let mut queue = VecDeque::from([start]);
        label[start] = Some(id);

        while let Some(index) = queue.pop_front() {
            size += 1;
            let x = index % grid.width;
            let z = index / grid.width;

            for (nx, nz) in neighbours(x, z, grid.width, grid.height) {
                let next = grid.index(nx, nz);
                if label[next].is_none() && grid.passable(next, class, types) {
                    label[next] = Some(id);
                    queue.push_back(next);
                }
            }
        }
        sizes.push(size);
    }

    Components { label, sizes }
}

fn neighbours(x: usize, z: usize, width: usize, height: usize) -> impl Iterator<Item = (usize, usize)> {
    let mut out = Vec::with_capacity(4);
    if x > 0 {
        out.push((x - 1, z));
    }
    if z > 0 {
        out.push((x, z - 1));
    }
    if x + 1 < width {
        out.push((x + 1, z));
    }
    if z + 1 < height {
        out.push((x, z + 1));
    }
    out.into_iter()
}

/// How a map distributes metal, which changes how expansion should be reasoned
/// about entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetalDistribution {
    /// Discrete deposits an extractor is placed on; the usual case.
    Spots,
    /// Metal covers so much of the surface that individual spots are meaningless
    /// and an extractor can be placed almost anywhere. Beyond All Reason ships
    /// several of these ("Full Metal Plate", "SpeedMetal", the metal-island maps).
    Field,
    /// The map carries no metal at all.
    Barren,
}

/// Fraction of cells holding metal above which a map is treated as a field.
const METAL_FIELD_FRACTION: f32 = 0.20;

/// An extractable metal deposit.
#[derive(Debug, Clone, Serialize)]
pub struct MetalSpot {
    pub x: f32,
    pub z: f32,
    /// Metal within one extractor radius, in engine income units.
    pub income: f32,
}

/// Locate metal spots by peak extraction over the metalmap.
///
/// Each candidate is scored by the metal an extractor placed there would capture,
/// then peaks are taken greedily, clearing their radius so neighbouring cells do
/// not report the same deposit twice. This mirrors how extractor coverage works
/// rather than reporting raw metalmap pixels.
pub fn metal_spots(grid: &TerrainGrid, map: &LoadedMap) -> (MetalDistribution, Vec<MetalSpot>) {
    let radius_cells = (map.info.extractor_radius / CELL_SIZE as f32).ceil().max(1.0) as isize;
    let radius_sq = (map.info.extractor_radius / CELL_SIZE as f32).powi(2);

    // Only cells holding metal contribute, and they are sparse, so splat their
    // contribution outward instead of scanning a disc around every cell.
    let sources: Vec<usize> = (0..grid.len()).filter(|&i| grid.metal[i] > 0).collect();
    if sources.is_empty() {
        return (MetalDistribution::Barren, Vec::new());
    }

    // On a saturated map every cell is a viable extractor site, so enumerating
    // "spots" would emit tens of thousands of meaningless entries. Report the
    // archetype instead and let the consumer treat placement as unconstrained.
    if sources.len() as f32 / grid.len() as f32 >= METAL_FIELD_FRACTION {
        return (MetalDistribution::Field, Vec::new());
    }

    let mut score = vec![0.0f32; grid.len()];
    for &source in &sources {
        let sx = (source % grid.width) as isize;
        let sz = (source / grid.width) as isize;
        let amount = grid.metal[source] as f32;

        for dz in -radius_cells..=radius_cells {
            for dx in -radius_cells..=radius_cells {
                if (dx * dx + dz * dz) as f32 > radius_sq {
                    continue;
                }
                let (x, z) = (sx + dx, sz + dz);
                if x < 0 || z < 0 || x >= grid.width as isize || z >= grid.height as isize {
                    continue;
                }
                score[z as usize * grid.width + x as usize] += amount;
            }
        }
    }

    // `maxMetal` is what a full metalmap byte is worth per extractor.
    let unit = map.info.max_metal / 255.0;
    // Ignore trace deposits an extractor would never be placed on.
    let floor = peak_floor(&score);

    let mut remaining = grid.metal.clone();
    let mut spots = Vec::new();

    loop {
        // Lowest index wins ties, keeping the result order-independent.
        let Some(peak) = (0..grid.len())
            .filter(|&i| score[i] > floor)
            .max_by(|&a, &b| score[a].total_cmp(&score[b]).then(b.cmp(&a)))
        else {
            break;
        };

        let (x, z) = (peak % grid.width, peak / grid.width);
        let (wx, wz) = grid.cell_centre(x, z);
        spots.push(MetalSpot { x: wx, z: wz, income: score[peak] * unit });

        // Consume the deposit this extractor would capture. Zeroing a disc of the
        // score field is not enough: cells just outside it still hold the same
        // sources' contributions and would re-report the deposit. Remove each
        // captured source's contribution wherever it reaches instead.
        for source in disc(x, z, radius_cells, radius_sq, grid.width, grid.height) {
            let amount = remaining[source];
            if amount == 0 {
                continue;
            }
            remaining[source] = 0;

            let (sx, sz) = (source % grid.width, source / grid.width);
            for affected in disc(sx, sz, radius_cells, radius_sq, grid.width, grid.height) {
                score[affected] = (score[affected] - amount as f32).max(0.0);
            }
        }
    }

    (MetalDistribution::Spots, spots)
}

/// Cell indices within `radius_cells` of a centre, clipped to the grid.
fn disc(
    cx: usize,
    cz: usize,
    radius_cells: isize,
    radius_sq: f32,
    width: usize,
    height: usize,
) -> impl Iterator<Item = usize> {
    let (cx, cz) = (cx as isize, cz as isize);
    (-radius_cells..=radius_cells).flat_map(move |dz| {
        (-radius_cells..=radius_cells).filter_map(move |dx| {
            if (dx * dx + dz * dz) as f32 > radius_sq {
                return None;
            }
            let (x, z) = (cx + dx, cz + dz);
            (x >= 0 && z >= 0 && x < width as isize && z < height as isize)
                .then(|| z as usize * width + x as usize)
        })
    })
}

/// Minimum score a cell must reach to count as a spot.
///
/// Maps differ by orders of magnitude in how much metal they scatter, so the
/// floor is relative: a fixed fraction of the richest extractor position. This
/// keeps dense "metal everywhere" maps from reporting a spot per cell while
/// still admitting the weaker outlying deposits on ordinary maps.
fn peak_floor(score: &[f32]) -> f32 {
    const RELATIVE_FLOOR: f32 = 0.25;
    let best = score.iter().copied().fold(0.0f32, f32::max);
    best * RELATIVE_FLOOR
}

/// Terrain around one start position, and what it implies.
#[derive(Debug, Clone, Serialize)]
pub struct StartAnalysis {
    pub team: u32,
    pub x: f32,
    pub z: f32,
    /// Fraction of sampled cells under water.
    pub water_fraction: f32,
    /// Mean slope over sampled land cells.
    pub mean_slope: f32,
    /// Metal spots within the sample radius.
    pub nearby_spots: usize,
    /// Income from those spots.
    pub nearby_income: f32,
    /// Share of the map reachable on foot from here.
    pub ground_reach: f32,
    /// No other start position is reachable from here by ground.
    pub land_locked: bool,
    /// Terrain-justified role suggestion. See [`suggest_role`].
    pub suggested_role: Role,
}

/// Roles the s3k CircuitAI map configs assign to a start position.
///
/// Only the terrain-determined ones are inferred. `TECH` and `AIR` are doctrine
/// choices about how a team divides labour, not properties of the ground, so they
/// are never suggested here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Role {
    Front,
    Sea,
    Tactical,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Front => "FRONT",
            Role::Sea => "SEA",
            Role::Tactical => "TACTICAL",
        }
    }
}

/// Classify a start by how much water surrounds it.
///
/// Thresholds are deliberately coarse: a mostly-wet start wants a shipyard, a
/// mixed shoreline start is where hovercraft earn their place, and anything else
/// is an ordinary land start.
fn suggest_role(water_fraction: f32, land_locked: bool) -> Role {
    match water_fraction {
        f if f >= 0.55 => Role::Sea,
        f if f >= 0.25 => Role::Tactical,
        _ if land_locked => Role::Tactical,
        _ => Role::Front,
    }
}

fn analyse_start(
    grid: &TerrainGrid,
    ground: &Components,
    spots: &[MetalSpot],
    position: crate::mapinfo::StartPos,
    others: &[crate::mapinfo::StartPos],
) -> StartAnalysis {
    let (cx, cz) = grid.cell_at(position.x, position.z);
    let radius_cells = (START_SAMPLE_RADIUS / CELL_SIZE as f32) as isize;
    let radius_sq = (START_SAMPLE_RADIUS / CELL_SIZE as f32).powi(2);

    let mut sampled = 0usize;
    let mut water = 0usize;
    let mut land_slope_sum = 0.0f64;
    let mut land_cells = 0usize;

    for dz in -radius_cells..=radius_cells {
        for dx in -radius_cells..=radius_cells {
            if (dx * dx + dz * dz) as f32 > radius_sq {
                continue;
            }
            let (x, z) = (cx as isize + dx, cz as isize + dz);
            if x < 0 || z < 0 || x >= grid.width as isize || z >= grid.height as isize {
                continue;
            }
            let index = z as usize * grid.width + x as usize;
            sampled += 1;
            if grid.band[index].is_water() {
                water += 1;
            } else {
                land_slope_sum += grid.slope[index] as f64;
                land_cells += 1;
            }
        }
    }

    let nearby: Vec<&MetalSpot> = spots
        .iter()
        .filter(|s| {
            let (dx, dz) = (s.x - position.x, s.z - position.z);
            dx * dx + dz * dz <= START_SAMPLE_RADIUS * START_SAMPLE_RADIUS
        })
        .collect();

    // A start is land-locked when no other declared start shares its ground
    // component: the army cannot walk to an opponent without crossing water.
    let home = ground.label[grid.index(cx, cz)];
    let land_locked = others.iter().filter(|o| o.team != position.team).all(|other| {
        let (ox, oz) = grid.cell_at(other.x, other.z);
        home.is_none() || ground.label[grid.index(ox, oz)] != home
    });

    let water_fraction = if sampled == 0 { 0.0 } else { water as f32 / sampled as f32 };

    StartAnalysis {
        team: position.team,
        x: position.x,
        z: position.z,
        water_fraction,
        mean_slope: if land_cells == 0 { 0.0 } else { (land_slope_sum / land_cells as f64) as f32 },
        nearby_spots: nearby.len(),
        nearby_income: nearby.iter().map(|s| s.income).sum(),
        ground_reach: home.map_or(0.0, |id| ground.coverage(id, grid.len())),
        land_locked,
        suggested_role: suggest_role(water_fraction, land_locked),
    }
}

/// Complete analysis of one map.
#[derive(Debug, Clone, Serialize)]
pub struct MapAnalysis {
    pub engine_name: String,
    /// Unversioned map name. The engine appends the version, so this is always a
    /// prefix of every release of the map and is what a config should match on.
    pub match_name: String,
    pub archive: String,
    pub world_width: f32,
    pub world_height: f32,
    pub min_height: f32,
    pub max_height: f32,
    /// Height range as stored in the `.smf` header, before any `mapinfo` override.
    pub header_min_height: f32,
    pub header_max_height: f32,
    /// Observed elevation range across the analysis grid.
    pub elevation_min: f32,
    pub elevation_max: f32,
    pub elevation_mean: f32,
    /// Fraction of cells above the waterline.
    pub land_fraction: f32,
    /// Mean slope over land cells.
    pub mean_land_slope: f32,
    pub mobility: Vec<Mobility>,
    /// Largest ground component as a fraction of the map; low values mean the
    /// land is fragmented into islands.
    pub largest_ground_component: f32,
    pub ground_component_count: usize,
    pub metal_distribution: MetalDistribution,
    /// Individual deposits. Empty when `metal_distribution` is not `Spots`.
    pub metal_spots: Vec<MetalSpot>,
    /// Fraction of cells holding any metal.
    pub metal_coverage: f32,
    pub total_metal_income: f32,
    pub starts: Vec<StartAnalysis>,
    pub water_damage: f32,
    pub tidal_strength: f32,
    pub gravity: f32,
    pub max_metal: f32,
    pub extractor_radius: f32,
    /// The info script consulted map or mod options, so these results describe
    /// the default option set only.
    pub option_sensitive: bool,
    pub notes: Vec<String>,
}

/// Run every derivation for a loaded map.
pub fn analyse(map: &LoadedMap, grid: &TerrainGrid) -> MapAnalysis {
    let types = &map.info.terrain_types;

    let land_cells: Vec<usize> =
        (0..grid.len()).filter(|&i| grid.band[i] == Band::Land).collect();
    let mean_land_slope = if land_cells.is_empty() {
        0.0
    } else {
        (land_cells.iter().map(|&i| grid.slope[i] as f64).sum::<f64>() / land_cells.len() as f64)
            as f32
    };

    let ground = components(grid, MoveClass::Bot, types);
    let largest = ground.sizes.iter().copied().max().unwrap_or(0);

    let (metal_distribution, spots) = metal_spots(grid, map);
    let starts = map
        .info
        .start_positions
        .iter()
        .map(|&position| {
            analyse_start(grid, &ground, &spots, position, &map.info.start_positions)
        })
        .collect();

    let (world_width, world_height) = map.smf.world_size();
    let (min_height, max_height) = map.height_range();

    MapAnalysis {
        engine_name: map.info.versioned_name.clone(),
        match_name: map.info.name.clone(),
        archive: map.archive.clone(),
        world_width,
        world_height,
        min_height,
        max_height,
        header_min_height: map.smf.header.min_height,
        header_max_height: map.smf.header.max_height,
        elevation_min: grid.elevation.iter().copied().fold(f32::INFINITY, f32::min),
        elevation_max: grid.elevation.iter().copied().fold(f32::NEG_INFINITY, f32::max),
        elevation_mean: (grid.elevation.iter().map(|&e| e as f64).sum::<f64>()
            / grid.len() as f64) as f32,
        land_fraction: land_cells.len() as f32 / grid.len() as f32,
        mean_land_slope,
        mobility: MoveClass::ALL.iter().map(|&c| grid.mobility(c, types)).collect(),
        largest_ground_component: largest as f32 / grid.len() as f32,
        ground_component_count: ground.sizes.len(),
        metal_distribution,
        metal_coverage: grid.metal.iter().filter(|&&m| m > 0).count() as f32 / grid.len() as f32,
        total_metal_income: spots.iter().map(|s| s.income).sum(),
        metal_spots: spots,
        starts,
        water_damage: map.info.water_damage,
        tidal_strength: map.info.tidal_strength,
        gravity: map.info.gravity,
        max_metal: map.info.max_metal,
        extractor_radius: map.info.extractor_radius,
        option_sensitive: map.option_sensitive,
        notes: map
            .info
            .notes
            .iter()
            .cloned()
            .chain(map.echoes.iter().map(|line| format!("script echo: {line}")))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_follows_water_coverage() {
        assert_eq!(suggest_role(0.8, false), Role::Sea);
        assert_eq!(suggest_role(0.35, false), Role::Tactical);
        assert_eq!(suggest_role(0.05, false), Role::Front);
    }

    #[test]
    fn an_isolated_dry_start_still_needs_water_crossing_units() {
        assert_eq!(suggest_role(0.05, true), Role::Tactical);
    }
}
