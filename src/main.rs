//! `smrt-bar-map` — deterministic terrain analysis and config generation for
//! Beyond All Reason maps.

mod analysis;
mod archive;
mod emit;
mod luaenv;
mod luajson;
mod map;
mod mapinfo;
mod smf;
mod terrain;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use luaenv::Options;
use std::path::PathBuf;

/// Default BAR map directory on a standard Windows install.
fn default_maps_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("Programs/Beyond-All-Reason/data/maps"))
}

#[derive(Parser)]
#[command(name = "smrt-bar-map", about = "Analyse Beyond All Reason maps and generate AI map configs")]
struct Cli {
    /// Directory holding `.sd7` map archives.
    #[arg(long, global = true, value_name = "DIR")]
    maps_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a config file per map into an output directory.
    Generate {
        /// Directory to write into. Created if absent.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// Only process archives whose file name contains this substring.
        #[arg(long)]
        filter: Option<String>,
        /// Also write the full analysis JSON beside each config.
        #[arg(long)]
        with_json: bool,
    },
    /// Analyse one map and print the derived facts as JSON.
    Analyze {
        /// Archive file stem, e.g. `acidicquarry_5.17`.
        archive: String,
    },
    /// Print one file from inside a map archive, or list the archive's contents.
    Cat {
        /// Archive file stem, e.g. `acidicquarry_5.17`.
        archive: String,
        /// VFS path inside the archive. Omit to list every entry.
        path: Option<String>,
    },
    /// Summarise the features placed on a map, grouped by type.
    Features {
        /// Archive file stem, or omit to summarise the whole library.
        archive: Option<String>,
    },
    /// Render a map's elevation bands as ASCII, for eyeballing the decode.
    Render {
        /// Archive file stem, e.g. `acidicquarry_5.17`.
        archive: String,
        /// Output width in characters.
        #[arg(long, default_value_t = 96)]
        cols: usize,
    },
    /// Print one map's evaluated `mapinfo.lua` as JSON.
    Dump {
        /// Archive file stem, e.g. `acidicquarry_5.17`.
        archive: String,
    },
    /// Inventory every archive: engine map name, extent, and load status.
    Scan {
        /// Only report maps whose name or archive contains this substring.
        #[arg(long)]
        filter: Option<String>,
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

/// One row of the inventory.
#[derive(serde::Serialize)]
struct ScanRow {
    archive: String,
    /// Name the engine reports to a Skirmish AI, or `null` when loading failed.
    engine_name: Option<String>,
    map_x: Option<i32>,
    map_y: Option<i32>,
    /// Playable extent in world units.
    world_w: Option<i32>,
    world_h: Option<i32>,
    min_height: Option<f32>,
    max_height: Option<f32>,
    option_sensitive: Option<bool>,
    error: Option<String>,
}

fn scan(dir: &std::path::Path, filter: Option<&str>, json: bool) -> Result<()> {
    let needle = filter.map(str::to_ascii_lowercase);
    let archives: Vec<_> = archive::discover(dir)?
        .into_iter()
        .filter(|path| {
            // Filter on the archive name before loading; engine names are only
            // known after evaluating the script, and loading all 250-plus
            // archives to answer a narrow query is wasteful.
            needle
                .as_ref()
                .is_none_or(|n| archive::stem_of(path).to_ascii_lowercase().contains(n))
        })
        .collect();

    let options = Options::default();
    let mut rows = Vec::with_capacity(archives.len());

    for path in &archives {
        let stem = archive::stem_of(path);
        let row = match map::load(path, &options) {
            Ok(m) => {
                let (w, h) = m.smf.world_size();
                let (min_h, max_h) = m.height_range();
                ScanRow {
                    archive: stem,
                    engine_name: Some(m.info.versioned_name),
                    map_x: Some(m.smf.header.map_x),
                    map_y: Some(m.smf.header.map_y),
                    world_w: Some(w as i32),
                    world_h: Some(h as i32),
                    min_height: Some(min_h),
                    max_height: Some(max_h),
                    option_sensitive: Some(m.option_sensitive),
                    error: None,
                }
            }
            Err(e) => ScanRow {
                archive: stem,
                engine_name: None,
                map_x: None,
                map_y: None,
                world_w: None,
                world_h: None,
                min_height: None,
                max_height: None,
                option_sensitive: None,
                error: Some(format!("{e:#}")),
            },
        };

        rows.push(row);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!("{:<44} {:<40} {:>11} {:>8}", "ARCHIVE", "ENGINE MAP NAME", "WORLD", "OPTS");
    for row in &rows {
        match &row.engine_name {
            Some(name) => println!(
                "{:<44} {:<40} {:>11} {:>8}",
                row.archive,
                name,
                format!("{}x{}", row.world_w.unwrap_or(0), row.world_h.unwrap_or(0)),
                if row.option_sensitive == Some(true) { "yes" } else { "" },
            ),
            None => println!("{:<44} !! {}", row.archive, row.error.as_deref().unwrap_or("failed")),
        }
    }

    let failed = rows.iter().filter(|r| r.engine_name.is_none()).count();
    eprintln!("\n{} archives, {} loaded, {} failed", rows.len(), rows.len() - failed, failed);
    Ok(())
}

fn dump(dir: &std::path::Path, archive: &str) -> Result<()> {
    let path = dir.join(format!("{archive}.sd7"));
    let loaded = map::load(&path, &Options::default())?;
    println!("{}", serde_json::to_string_pretty(&loaded.raw_info)?);
    Ok(())
}

fn analyze(dir: &std::path::Path, archive: &str) -> Result<()> {
    let path = dir.join(format!("{archive}.sd7"));
    let loaded = map::load(&path, &Options::default())?;
    let grid = terrain::build(&loaded, terrain::Thresholds::default());
    let report = analysis::analyse(&loaded, &grid);
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Print an archive entry, or list the archive when no path is given.
fn cat(dir: &std::path::Path, stem: &str, path: Option<&str>) -> Result<()> {
    let mut opened = archive::MapArchive::open(dir.join(format!("{stem}.sd7")))?;

    let Some(path) = path else {
        for entry in opened.entries() {
            println!("{entry}");
        }
        return Ok(());
    };

    let data = opened
        .read(path)?
        .with_context(|| format!("{path} is not in {stem}"))?;
    std::io::Write::write_all(&mut std::io::stdout(), &data)?;
    Ok(())
}

/// Report feature placements, which carry reclaim value and block line of sight.
fn features(dir: &std::path::Path, archive: Option<&str>) -> Result<()> {
    use std::collections::BTreeMap;

    let archives = select_archives(dir, archive)?;
    let options = Options::default();
    let mut totals: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut scanned = 0usize;

    for path in &archives {
        let Ok(loaded) = map::load(path, &options) else { continue };
        scanned += 1;

        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for feature in &loaded.smf.features {
            *seen.entry(feature.name.as_str()).or_default() += 1;
        }
        if archives.len() == 1 {
            println!("{}: {} features", loaded.info.versioned_name, loaded.smf.features.len());
        }
        for (name, count) in seen {
            let entry = totals.entry(name.to_string()).or_default();
            entry.0 += count;
            entry.1 += 1;
        }
    }

    let mut rows: Vec<_> = totals.into_iter().collect();
    rows.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(b.1.0.cmp(&a.1.0)));

    println!("
{:<38} {:>10} {:>8}", "FEATURE TYPE", "PLACEMENTS", "MAPS");
    for (name, (count, maps)) in rows.iter().take(40) {
        println!("{name:<38} {count:>10} {maps:>8}");
    }
    eprintln!("
{} distinct feature types across {scanned} maps", rows.len());
    Ok(())
}

/// Archives in `dir`, optionally narrowed by a substring of the file name.
fn select_archives(dir: &std::path::Path, filter: Option<&str>) -> Result<Vec<PathBuf>> {
    let needle = filter.map(str::to_ascii_lowercase);
    Ok(archive::discover(dir)?
        .into_iter()
        .filter(|path| {
            needle
                .as_ref()
                .is_none_or(|n| archive::stem_of(path).to_ascii_lowercase().contains(n))
        })
        .collect())
}

/// Analyse every selected map and write one config file each.
fn generate(
    dir: &std::path::Path,
    out: &std::path::Path,
    filter: Option<&str>,
    with_json: bool,
) -> Result<()> {
    std::fs::create_dir_all(out)
        .with_context(|| format!("creating output directory {}", out.display()))?;

    let archives = select_archives(dir, filter)?;
    let options = Options::default();
    let mut written = 0usize;
    let mut failures = Vec::new();
    // Several archives can describe the same map at different versions; the last
    // one wins, and collisions are reported rather than silently overwritten.
    let mut by_match_name: std::collections::BTreeMap<String, Vec<String>> = Default::default();

    for path in &archives {
        let stem = archive::stem_of(path);
        let loaded = match map::load(path, &options) {
            Ok(loaded) => loaded,
            Err(error) => {
                failures.push((stem, format!("{error:#}")));
                continue;
            }
        };

        let grid = terrain::build(&loaded, terrain::Thresholds::default());
        let report = analysis::analyse(&loaded, &grid);
        by_match_name.entry(report.match_name.clone()).or_default().push(stem.clone());

        let base = emit::namespace_for(&report.match_name);
        std::fs::write(out.join(format!("{base}.as")), emit::angelscript(&report))?;
        if with_json {
            std::fs::write(
                out.join(format!("{base}.json")),
                serde_json::to_string_pretty(&report)?,
            )?;
        }
        written += 1;
    }

    eprintln!("wrote {written} configs to {}", out.display());

    let shared: Vec<_> = by_match_name.iter().filter(|(_, v)| v.len() > 1).collect();
    if !shared.is_empty() {
        eprintln!("
{} map names span several archives:", shared.len());
        for (name, archives) in shared {
            eprintln!("  {name}: {}", archives.join(", "));
        }
    }

    if !failures.is_empty() {
        eprintln!("
{} archives failed:", failures.len());
        for (stem, error) in &failures {
            eprintln!("  {stem}: {error}");
        }
    }
    Ok(())
}

/// Print elevation bands as ASCII so a decode can be checked by eye.
fn render(dir: &std::path::Path, archive: &str, cols: usize) -> Result<()> {
    let path = dir.join(format!("{archive}.sd7"));
    let loaded = map::load(&path, &Options::default())?;
    let grid = terrain::build(&loaded, terrain::Thresholds::default());

    let step = (grid.width / cols.max(1)).max(1);
    // Character cells are about twice as tall as wide.
    let rows = grid.height / (step * 2);

    for row in 0..rows {
        let line: String = (0..grid.width / step)
            .map(|col| {
                let index = grid.index(col * step, (row * step * 2).min(grid.height - 1));
                match grid.band[index] {
                    terrain::Band::Deep => '~',
                    terrain::Band::Shoal => ':',
                    terrain::Band::Shallows => '.',
                    terrain::Band::Land if grid.slope[index] > 0.6 => '#',
                    terrain::Band::Land => '+',
                }
            })
            .collect();
        println!("{line}");
    }
    println!("legend: ~ deep  : shoal  . shallows  + land  # steep land");
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dir = cli
        .maps_dir
        .or_else(default_maps_dir)
        .context("no --maps-dir given and LOCALAPPDATA is unset")?;

    match cli.command {
        Command::Scan { filter, json } => scan(&dir, filter.as_deref(), json),
        Command::Dump { archive } => dump(&dir, &archive),
        Command::Analyze { archive } => analyze(&dir, &archive),
        Command::Render { archive, cols } => render(&dir, &archive, cols),
        Command::Features { archive } => features(&dir, archive.as_deref()),
        Command::Cat { archive, path } => cat(&dir, &archive, path.as_deref()),
        Command::Generate { out, filter, with_json } => {
            generate(&dir, &out, filter.as_deref(), with_json)
        }
    }
}
