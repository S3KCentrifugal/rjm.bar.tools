//! Loading a map: archive in, evaluated `mapinfo.lua` plus decoded `.smf` out.

use crate::archive::MapArchive;
use crate::luaenv::{MapLua, Options};
use crate::mapinfo::{self, MapInfo};
use crate::smf::{self, Smf};
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;

/// Everything read from one map archive.
pub struct LoadedMap {
    /// Archive file stem, e.g. `acidicquarry_5.17`.
    pub archive: String,
    pub info: MapInfo,
    pub smf: Smf,
    /// `Spring.Echo` output produced while evaluating the info script.
    pub echoes: Vec<String>,
    /// The script consulted map or mod options, so its output is option-dependent.
    pub option_sensitive: bool,
    /// The evaluated info table, as JSON, for inspection and diffing.
    pub raw_info: serde_json::Value,
}

impl LoadedMap {
    /// Height range actually in force: the `mapinfo.smf` override when present,
    /// otherwise the `.smf` header. Map options commonly move these.
    pub fn height_range(&self) -> (f32, f32) {
        (
            self.info.min_height_override.unwrap_or(self.smf.header.min_height),
            self.info.max_height_override.unwrap_or(self.smf.header.max_height),
        )
    }
}

fn is_lua(path: &str) -> bool {
    path.ends_with(".lua")
}

fn is_smf(path: &str) -> bool {
    path.ends_with(".smf")
}

/// Open an archive and decode the parts needed for analysis.
///
/// Lua sources and the `.smf` are pulled in a single decompression pass; the tile
/// file (`.smt`), textures and models are never read.
pub fn load(path: impl AsRef<Path>, options: &Options) -> Result<LoadedMap> {
    let path = path.as_ref();
    let mut archive = MapArchive::open(path)?;
    let archive_name = archive.stem();

    let smf_path = archive
        .entries()
        .find(|entry| is_smf(entry))
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{} contains no .smf", path.display()))?;

    let lua_paths = archive.entries_matching(is_lua);
    let lua_files: BTreeMap<String, String> = archive
        .read_all(&lua_paths)
        .with_context(|| format!("reading Lua sources from {}", path.display()))?
        .into_iter()
        .map(|(entry, bytes)| (entry, String::from_utf8_lossy(&bytes).into_owned()))
        .collect();

    let smf_bytes = archive
        .read(&smf_path)?
        .ok_or_else(|| anyhow!("{smf_path} vanished from the archive index"))?;

    let has_root_info = lua_files.contains_key("mapinfo.lua");
    if !has_root_info && !lua_files.contains_key("maphelper/mapinfo.lua") {
        return Err(anyhow!("{} has no mapinfo.lua", path.display()));
    }

    let env = MapLua::new(Rc::new(lua_files), options)?;
    let info_path = if has_root_info { "mapinfo.lua" } else { "maphelper/mapinfo.lua" };
    let info_value = env.eval(info_path)?;
    let raw_info = crate::luajson::to_json(&info_value);
    let info = mapinfo::from_json(&raw_info)?;
    let transcript = env.transcript()?;
    let option_sensitive = env.read_options();

    let smf = smf::parse(&smf_bytes).with_context(|| format!("parsing {smf_path}"))?;

    Ok(LoadedMap {
        archive: archive_name,
        info,
        smf,
        echoes: transcript.echoes,
        option_sensitive,
        raw_info,
    })
}
