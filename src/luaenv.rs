//! A sandboxed Lua 5.1 environment that can execute a map's `mapinfo.lua`.
//!
//! Map scripts expect the host globals Spring provides: a `VFS` table backed by the
//! archive, `Spring.Echo`, the map/mod option getters, and `lowerkeys`. Nothing here
//! touches the real filesystem or the network; the VFS is served entirely from files
//! pre-extracted from the archive, which keeps evaluation deterministic.
//!
//! `mlua::Error` is neither `Send` nor `Sync`, so it cannot flow into `anyhow`
//! directly. Internals stay in `mlua::Result` and convert once at the public
//! boundary via [`lift`].

use anyhow::{Result, anyhow};
use mlua::{Lua, MultiValue, Table, Value, Variadic};
use std::collections::BTreeMap;
use std::fmt::Display;
use std::rc::Rc;

/// Lua sources addressed by normalised VFS path.
pub type LuaFiles = Rc<BTreeMap<String, String>>;

/// Convert a Lua error into an `anyhow` error, preserving its message.
fn lift<T>(result: mlua::Result<T>, what: impl Display) -> Result<T> {
    result.map_err(|e| anyhow!("{what}: {e}"))
}

/// Options surfaced to map scripts through `Spring.GetMapOptions`/`GetModOptions`.
///
/// Defaults are empty, which is what the engine passes when a game starts with no
/// options set. Some maps branch on these and produce materially different terrain,
/// so the choice is recorded in the analysis output rather than left implicit.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub map: BTreeMap<String, String>,
    pub mod_: BTreeMap<String, String>,
}

/// Messages a map script emitted via `Spring.Echo`, kept for diagnostics.
#[derive(Debug, Default)]
pub struct Transcript {
    pub echoes: Vec<String>,
}

pub struct MapLua {
    lua: Lua,
    files: LuaFiles,
}

impl MapLua {
    /// Build an environment serving `files`, with `options` visible to the script.
    pub fn new(files: LuaFiles, options: &Options) -> Result<Self> {
        let env = MapLua { lua: Lua::new(), files };
        lift(env.install_vfs(), "installing VFS shim")?;
        lift(env.install_spring(options), "installing Spring shim")?;
        lift(env.install_helpers(), "installing Lua helpers")?;
        Ok(env)
    }

    /// Execute a VFS-resident Lua file and return its result.
    pub fn eval(&self, path: &str) -> Result<Value> {
        let key = crate::archive::normalise(path);
        let source = self
            .files
            .get(&key)
            .ok_or_else(|| anyhow!("{path} is not present in the archive"))?;
        lift(
            self.lua.load(source.as_str()).set_name(path).eval::<Value>(),
            format!("evaluating {path}"),
        )
    }

    /// Collect `Spring.Echo` output recorded during evaluation.
    pub fn transcript(&self) -> Result<Transcript> {
        let echoes = lift(
            self.lua
                .globals()
                .get::<Table>("__echo")
                .and_then(|t| t.sequence_values().collect::<mlua::Result<Vec<String>>>()),
            "reading Spring.Echo transcript",
        )?;
        Ok(Transcript { echoes })
    }

    /// Whether the script consulted map or mod options. Such maps can analyse
    /// differently under non-default options, so callers should flag them.
    pub fn read_options(&self) -> bool {
        self.lua.globals().get::<bool>("__read_options").unwrap_or(false)
    }

    fn install_vfs(&self) -> mlua::Result<()> {
        let lua = &self.lua;
        let vfs = lua.create_table()?;

        // Mode constants. Scripts pass them through to VFS calls; the shim ignores
        // them because a map archive is the only source served.
        for mode in [
            "RAW", "MOD", "MAP", "BASE", "MENU", "ZIP", "RAW_FIRST", "ZIP_FIRST", "RAW_ONLY",
            "ZIP_ONLY",
        ] {
            vfs.set(mode, mode)?;
        }

        let files = Rc::clone(&self.files);
        vfs.set(
            "FileExists",
            lua.create_function(move |_, args: Variadic<Value>| {
                Ok(first_string(&args)
                    .is_some_and(|p| files.contains_key(&crate::archive::normalise(&p))))
            })?,
        )?;

        let files = Rc::clone(&self.files);
        vfs.set(
            "LoadFile",
            lua.create_function(move |_, args: Variadic<Value>| {
                Ok(first_string(&args)
                    .and_then(|p| files.get(&crate::archive::normalise(&p)).cloned()))
            })?,
        )?;

        let files = Rc::clone(&self.files);
        vfs.set(
            "Include",
            lua.create_function(move |lua, args: Variadic<Value>| {
                let Some(path) = first_string(&args) else {
                    return Ok(Value::Nil);
                };
                let Some(source) = files.get(&crate::archive::normalise(&path)) else {
                    return Ok(Value::Nil);
                };
                lua.load(source.as_str()).set_name(&path).eval::<Value>()
            })?,
        )?;

        let files = Rc::clone(&self.files);
        vfs.set(
            "DirList",
            lua.create_function(move |lua, args: Variadic<Value>| {
                let dir = first_string(&args).unwrap_or_default();
                let pattern = args.get(1).and_then(as_string);
                dir_list(lua, &files, &dir, pattern.as_deref())
            })?,
        )?;

        let files = Rc::clone(&self.files);
        vfs.set(
            "SubDirs",
            lua.create_function(move |lua, args: Variadic<Value>| {
                let prefix = as_dir_prefix(&first_string(&args).unwrap_or_default());
                let mut seen: Vec<String> = files
                    .keys()
                    .filter_map(|p| p.strip_prefix(&prefix))
                    .filter_map(|rest| rest.split_once('/'))
                    .map(|(head, _)| format!("{prefix}{head}/"))
                    .collect();
                seen.sort();
                seen.dedup();
                lua.create_sequence_from(seen)
            })?,
        )?;

        lua.globals().set("VFS", vfs)?;
        Ok(())
    }

    fn install_spring(&self, options: &Options) -> mlua::Result<()> {
        let lua = &self.lua;
        let spring = lua.create_table()?;

        lua.globals().set("__echo", lua.create_table()?)?;
        lua.globals().set("__read_options", false)?;

        spring.set(
            "Echo",
            lua.create_function(|lua, args: MultiValue| {
                let line = args
                    .iter()
                    .map(render_echo_arg)
                    .collect::<Vec<_>>()
                    .join("\t");
                lua.globals().get::<Table>("__echo")?.push(line)
            })?,
        )?;
        spring.set("GetMapOptions", option_getter(lua, &options.map)?)?;
        spring.set("GetModOptions", option_getter(lua, &options.mod_)?)?;

        lua.globals().set("Spring", spring)?;
        Ok(())
    }

    fn install_helpers(&self) -> mlua::Result<()> {
        // `lowerkeys` is a Spring global; map scripts use it to normalise merged
        // overlay tables. Recurses into nested tables, leaving non-string keys alone.
        self.lua
            .load(
                r#"
                function lowerkeys(t)
                    if type(t) ~= "table" then return t end
                    local out = {}
                    for k, v in pairs(t) do
                        if type(k) == "string" then k = string.lower(k) end
                        if type(v) == "table" then v = lowerkeys(v) end
                        out[k] = v
                    end
                    return out
                end
                "#,
            )
            .set_name("=[lowerkeys]")
            .exec()
    }
}

fn option_getter(lua: &Lua, values: &BTreeMap<String, String>) -> mlua::Result<mlua::Function> {
    let values = values.clone();
    lua.create_function(move |lua, ()| {
        lua.globals().set("__read_options", true)?;
        let table = lua.create_table()?;
        for (key, value) in &values {
            table.set(key.as_str(), value.as_str())?;
        }
        Ok(table)
    })
}

fn dir_list(
    lua: &Lua,
    files: &BTreeMap<String, String>,
    dir: &str,
    pattern: Option<&str>,
) -> mlua::Result<Table> {
    let prefix = as_dir_prefix(dir);
    let ext = pattern
        .and_then(|p| p.rsplit('.').next())
        .map(str::to_ascii_lowercase)
        .filter(|e| e != "*");

    let matched: Vec<String> = files
        .keys()
        .filter(|p| p.starts_with(&prefix) && !p[prefix.len()..].contains('/'))
        .filter(|p| ext.as_ref().is_none_or(|e| p.rsplit('.').next() == Some(e.as_str())))
        .cloned()
        .collect();

    lua.create_sequence_from(matched)
}

/// Normalise a directory argument into a `""` or `"dir/"` prefix.
fn as_dir_prefix(dir: &str) -> String {
    let dir = crate::archive::normalise(dir);
    if dir.is_empty() || dir.ends_with('/') { dir } else { format!("{dir}/") }
}

fn render_echo_arg(value: &Value) -> String {
    match value {
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => "nil".to_string(),
        other => other.type_name().to_string(),
    }
}

fn as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.to_string_lossy().to_string()),
        _ => None,
    }
}

fn first_string(args: &Variadic<Value>) -> Option<String> {
    args.first().and_then(as_string)
}
