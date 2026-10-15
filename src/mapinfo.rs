//! Typed view over a map's evaluated `mapinfo.lua`.
//!
//! Key names and defaults mirror `CMapInfo::ReadGlobal`, `ReadWater` and
//! `ReadTerrainTypes` in the Recoil engine. Lookups are case-insensitive because
//! the engine lowercases both the parsed table (`LuaUtils::LowerKeys`) and every
//! C++-side key (`LuaParser::lowerCppKeys`); [`crate::luajson`] applies the same
//! normalisation, so every key read here is lowercase.

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value as Json;

/// Number of terrain type slots addressable by the `.smf` typemap.
pub const NUM_TERRAIN_TYPES: usize = 256;

/// Per-terrain-type movement multipliers, indexed by typemap byte.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TerrainType {
    pub name: String,
    pub hardness: f32,
    pub tank: f32,
    pub kbot: f32,
    pub hover: f32,
    pub ship: f32,
}

impl Default for TerrainType {
    /// Engine defaults for an undefined slot: fully passable, normal hardness.
    fn default() -> Self {
        Self { name: "Default".into(), hardness: 1.0, tank: 1.0, kbot: 1.0, hover: 1.0, ship: 1.0 }
    }
}

/// A start position declared by the map, in world units.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct StartPos {
    /// Team index the map assigned this position to.
    pub team: u32,
    pub x: f32,
    pub z: f32,
}

/// The subset of `mapinfo.lua` that affects terrain analysis or map identity.
#[derive(Debug, Clone, Serialize)]
pub struct MapInfo {
    /// `name` as authored, before the version suffix is applied.
    pub name: String,
    pub version: String,
    /// Name the engine reports to a Skirmish AI. See [`versioned_name`].
    pub versioned_name: String,
    pub description: String,
    pub author: String,

    pub map_hardness: f32,
    pub not_deformable: bool,
    /// Raw `gravity`, before the engine's per-frame conversion.
    pub gravity: f32,
    pub tidal_strength: f32,
    pub max_metal: f32,
    pub extractor_radius: f32,
    pub void_water: bool,
    pub void_ground: bool,

    /// Height range override from the `smf` table. Takes precedence over the
    /// `.smf` header and is what the water-level map options manipulate.
    pub min_height_override: Option<f32>,
    pub max_height_override: Option<f32>,

    /// Damage dealt to units in water. Non-zero rules out amphibious play.
    pub water_damage: f32,

    pub terrain_types: Vec<TerrainType>,
    /// Start positions declared under `teams`, ordered by team index.
    pub start_positions: Vec<StartPos>,

    /// Non-fatal observations, e.g. a missing `terrainTypes` table.
    pub notes: Vec<String>,
}

/// Reproduce `CArchiveScanner`'s versioned name exactly: `name + " " + version`,
/// unless the version already appears in the name (ArchiveScanner.cpp:191).
pub fn versioned_name(name: &str, version: &str) -> String {
    if version.is_empty() || name.contains(version) {
        name.to_string()
    } else {
        format!("{name} {version}")
    }
}

/// Field accessors over a JSON object with lowercase keys.
trait JsonExt {
    fn field(&self, key: &str) -> Option<&Json>;
    fn str_or(&self, key: &str, default: &str) -> String;
    fn f32_opt(&self, key: &str) -> Option<f32>;
    fn f32_or(&self, key: &str, default: f32) -> f32;
    fn bool_or(&self, key: &str, default: bool) -> bool;
}

impl JsonExt for Json {
    fn field(&self, key: &str) -> Option<&Json> {
        self.as_object()?.get(key)
    }

    fn str_or(&self, key: &str, default: &str) -> String {
        self.field(key).and_then(Json::as_str).unwrap_or(default).to_string()
    }

    fn f32_opt(&self, key: &str) -> Option<f32> {
        match self.field(key)? {
            Json::Number(n) => n.as_f64().map(|v| v as f32),
            // Map scripts sometimes store numbers as strings.
            Json::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    fn f32_or(&self, key: &str, default: f32) -> f32 {
        self.f32_opt(key).unwrap_or(default)
    }

    fn bool_or(&self, key: &str, default: bool) -> bool {
        match self.field(key) {
            Some(Json::Bool(b)) => *b,
            Some(Json::Number(n)) => n.as_f64().is_some_and(|v| v != 0.0),
            _ => default,
        }
    }
}

fn read_terrain_types(root: &Json, notes: &mut Vec<String>) -> Vec<TerrainType> {
    let mut types = vec![TerrainType::default(); NUM_TERRAIN_TYPES];

    let Some(table) = root.field("terraintypes") else {
        notes.push("no terrainTypes table; assuming uniform passability".into());
        return types;
    };

    let mut defined = 0usize;
    for (index, slot) in types.iter_mut().enumerate() {
        let Some(entry) = table.field(&index.to_string()) else {
            continue;
        };
        let speeds = entry.field("movespeeds");
        let speed = |key: &str| speeds.map_or(1.0, |s| s.f32_or(key, 1.0)).max(0.0);

        *slot = TerrainType {
            name: entry.str_or("name", "Default"),
            hardness: entry.f32_or("hardness", 1.0).max(0.001),
            tank: speed("tank"),
            kbot: speed("kbot"),
            hover: speed("hover"),
            ship: speed("ship"),
        };
        defined += 1;
    }

    if defined == 0 {
        notes.push("terrainTypes table present but defined no slots".into());
    }
    types
}

fn read_start_positions(root: &Json, notes: &mut Vec<String>) -> Vec<StartPos> {
    let Some(teams) = root.field("teams").and_then(Json::as_object) else {
        notes.push("no teams table; map declares no start positions".into());
        return Vec::new();
    };

    let mut positions: Vec<StartPos> = teams
        .iter()
        .filter_map(|(key, entry)| {
            let start = entry.field("startpos")?;
            Some(StartPos {
                team: key.parse().ok()?,
                x: start.f32_opt("x")?,
                z: start.f32_opt("z")?,
            })
        })
        .collect();

    positions.sort_by_key(|p| p.team);
    if positions.is_empty() {
        notes.push("teams table present but declared no usable startpos".into());
    }
    positions
}

/// Interpret an evaluated `mapinfo.lua` result, already normalised to JSON.
pub fn from_json(root: &Json) -> Result<MapInfo> {
    if !root.is_object() {
        bail!("mapinfo.lua returned a non-table value");
    }

    let mut notes = Vec::new();
    let name = root.str_or("name", "");
    let version = root.str_or("version", "");
    if name.is_empty() {
        notes.push("mapinfo has no name field".into());
    }

    let smf = root.field("smf");
    let min_height_override = smf.and_then(|t| t.f32_opt("minheight"));
    let max_height_override = smf.and_then(|t| t.f32_opt("maxheight"));

    let water_damage = root.field("water").map_or(0.0, |w| w.f32_or("damage", 0.0));
    let terrain_types = read_terrain_types(root, &mut notes);
    let start_positions = read_start_positions(root, &mut notes);

    Ok(MapInfo {
        versioned_name: versioned_name(&name, &version),
        name,
        version,
        description: root.str_or("description", ""),
        author: root.str_or("author", ""),
        map_hardness: root.f32_or("maphardness", 100.0),
        not_deformable: root.bool_or("notdeformable", false),
        gravity: root.f32_or("gravity", 130.0),
        tidal_strength: root.f32_or("tidalstrength", 0.0).max(0.0),
        max_metal: root.f32_or("maxmetal", 0.02).max(0.0),
        extractor_radius: root.f32_or("extractorradius", 500.0).max(0.0),
        void_water: root.bool_or("voidwater", false),
        void_ground: root.bool_or("voidground", false),
        min_height_override,
        max_height_override,
        water_damage,
        terrain_types,
        start_positions,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_name_appends_when_absent() {
        assert_eq!(versioned_name("AcidicQuarry", "5.17"), "AcidicQuarry 5.17");
        assert_eq!(versioned_name("All That Glitters", "v2.2.3"), "All That Glitters v2.2.3");
    }

    #[test]
    fn versioned_name_keeps_name_when_version_embedded() {
        assert_eq!(versioned_name("Shore_to_Shore_V3", "V3"), "Shore_to_Shore_V3");
        assert_eq!(versioned_name("Mediterraneum_V1", "V1"), "Mediterraneum_V1");
    }

    #[test]
    fn versioned_name_tolerates_missing_version() {
        assert_eq!(versioned_name("Tempest", ""), "Tempest");
    }
}
