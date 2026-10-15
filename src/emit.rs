//! Rendering analysis results into the files CircuitAI consumes.
//!
//! Output is deterministic: identical input archives produce byte-identical
//! files. Floats are written at fixed precision, collections are emitted in a
//! sorted or otherwise fixed order, and nothing depends on hash iteration or
//! filesystem ordering.

use crate::analysis::{MapAnalysis, Role, StartAnalysis};
use crate::terrain::MoveClass;
use std::fmt::Write as _;

/// Factions whose factories can be weighted independently.
const SIDES: [&str; 3] = ["armada", "cortex", "legion"];

/// Faction-specific factory unit names, in the order [`FactoryKind`] declares.
const FACTORY_NAMES: [[&str; 4]; 3] = [
    ["armlab", "armvp", "armhp", "armsy"],
    ["corlab", "corvp", "corhp", "corsy"],
    ["leglab", "legvp", "leghp", "legsy"],
];

/// The opening factories a terrain analysis can argue for.
#[derive(Clone, Copy)]
enum FactoryKind {
    BotLab,
    VehiclePlant,
    HoverPlant,
    Shipyard,
}

impl FactoryKind {
    fn name_for(self, side_index: usize) -> &'static str {
        FACTORY_NAMES[side_index][self as usize]
    }
}

/// Convert a fraction in `0..=1` into a small positive weight, or `None` when the
/// terrain does not support that factory at all.
///
/// Weights are what `SelectWeightedFactory` samples, so only their ratios matter.
fn weight_from(fraction: f32) -> Option<u32> {
    // Below this the class can barely move; offering it as an opening is noise.
    const MINIMUM_USEFUL_COVERAGE: f32 = 0.08;
    (fraction >= MINIMUM_USEFUL_COVERAGE).then(|| (fraction * 10.0).round().max(1.0) as u32)
}

/// Factory weights implied by how much of the map each movement class can use,
/// for one role.
///
/// Hovercraft are only worth weighting where they reach ground the other classes
/// cannot, so their weight comes from the margin over the better land class
/// rather than from raw coverage. `SEA` and `TACTICAL` starts sit on water, so
/// they suppress the options that cannot leave the shoreline.
fn factory_weights(analysis: &MapAnalysis, role: Role) -> Vec<(FactoryKind, u32)> {
    let coverage = |class: MoveClass| {
        analysis.mobility.iter().find(|m| m.class == class).map_or(0.0, |m| m.coverage)
    };

    let bot = coverage(MoveClass::Bot);
    let vehicle = coverage(MoveClass::Vehicle);
    let ship = coverage(MoveClass::Ship);
    let hover_margin = (coverage(MoveClass::Hover) - bot.max(vehicle)).max(0.0);

    let candidates = match role {
        // A land start opens on whichever ground class gets around best.
        Role::Front => [
            (FactoryKind::BotLab, bot),
            (FactoryKind::VehiclePlant, vehicle),
            (FactoryKind::HoverPlant, hover_margin),
            (FactoryKind::Shipyard, 0.0),
        ],
        // A wet start leads with hulls and keeps hover as the shoreline answer.
        Role::Sea => [
            (FactoryKind::BotLab, bot * 0.25),
            (FactoryKind::VehiclePlant, 0.0),
            (FactoryKind::HoverPlant, hover_margin.max(ship * 0.3)),
            (FactoryKind::Shipyard, ship),
        ],
        // A mixed shoreline is exactly where hovercraft earn their place.
        Role::Tactical => [
            (FactoryKind::BotLab, bot * 0.5),
            (FactoryKind::VehiclePlant, 0.0),
            (FactoryKind::HoverPlant, hover_margin.max(0.35)),
            (FactoryKind::Shipyard, ship * 0.5),
        ],
    };

    candidates
        .into_iter()
        .filter_map(|(kind, fraction)| weight_from(fraction).map(|w| (kind, w)))
        .collect()
}

/// Roles that appear among a map's start positions, in a fixed order.
fn roles_present(analysis: &MapAnalysis) -> Vec<Role> {
    let mut roles: Vec<Role> = [Role::Front, Role::Sea, Role::Tactical]
        .into_iter()
        .filter(|role| analysis.starts.iter().any(|s| s.suggested_role == *role))
        .collect();
    if roles.is_empty() {
        roles.push(Role::Front);
    }
    roles
}

/// A CircuitAI namespace identifier derived from the map name.
///
/// Strips everything that is not alphanumeric and upper-cases each word, so
/// `All That Glitters` becomes `AllThatGlitters`. A leading digit is prefixed
/// because AngelScript identifiers cannot start with one.
pub fn namespace_for(map_name: &str) -> String {
    let mut out = String::new();
    let mut capitalise = true;

    for ch in map_name.chars() {
        if ch.is_ascii_alphanumeric() {
            if capitalise {
                out.extend(ch.to_uppercase());
                capitalise = false;
            } else {
                out.push(ch);
            }
        } else {
            capitalise = true;
        }
    }

    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, 'M');
    }
    if out.is_empty() {
        out.push_str("UnnamedMap");
    }
    out
}

/// Format a world coordinate the way the hand-written configs do: whole units.
fn coord(value: f32) -> i64 {
    value.round() as i64
}

fn write_start_spots(out: &mut String, starts: &[StartAnalysis]) {
    if starts.is_empty() {
        out.push_str("\tStartSpot@[] spots = {};\n");
        return;
    }

    out.push_str("\tStartSpot@[] spots = {\n");
    for (index, start) in starts.iter().enumerate() {
        let separator = if index + 1 == starts.len() { "" } else { "," };
        let _ = writeln!(
            out,
            "\t\tStartSpot(AIFloat3({:>6}, 0, {:>6}), AiRole::{}, {}){}  // team {} water {:.2}",
            coord(start.x),
            coord(start.z),
            start.suggested_role.as_str(),
            start.land_locked,
            separator,
            start.team,
            start.water_fraction,
        );
    }
    out.push_str("\t};\n");
}

/// Emit `getFactoryWeights` in the schema `MapConfig` expects:
/// role key -> side name -> factory name -> weight.
fn write_factory_weights(out: &mut String, analysis: &MapAnalysis) {
    out.push_str("\tdictionary getFactoryWeights() {\n\t\tdictionary root;\n");

    for role in roles_present(analysis) {
        let weights = factory_weights(analysis, role);
        if weights.is_empty() {
            continue;
        }

        let role_var = role.as_str().to_ascii_lowercase();
        let _ = writeln!(out, "\n\t\tdictionary {role_var};");

        for (side_index, side) in SIDES.iter().enumerate() {
            let var = format!("{role_var}_{side}");
            let _ = writeln!(out, "\t\tdictionary {var};");
            for (kind, weight) in &weights {
                let _ = writeln!(
                    out,
                    "\t\t{var}.set(\"{}\", {weight});",
                    kind.name_for(side_index)
                );
            }
            let _ = writeln!(out, "\t\t{role_var}.set(\"{side}\", @{var});");
        }
        let _ = writeln!(out, "\t\troot.set(\"{}\", @{role_var});", role.as_str());
    }

    out.push_str("\n\t\treturn root;\n\t}\n");
}

/// Render an AngelScript `MapConfig` for one analysed map.
pub fn angelscript(analysis: &MapAnalysis) -> String {
    let namespace = namespace_for(&analysis.match_name);

    let mut out = String::new();
    out.push_str("// Generated by smrt-bar-map. Do not edit by hand.\n");
    let _ = writeln!(out, "// Source archive: {}", analysis.archive);
    let _ = writeln!(out, "// Engine map name: {}", analysis.engine_name);
    if analysis.option_sensitive {
        out.push_str(
            "// This map's info script reads map options; the analysis below\n\
             // describes the default option set only.\n",
        );
    }
    out.push('\n');
    out.push_str("#include \"../define.as\"\n");
    out.push_str("#include \"../types/start_spot.as\"\n");
    out.push_str("#include \"../types/map_config.as\"\n\n");

    let _ = writeln!(out, "namespace {namespace} {{");
    let _ = writeln!(
        out,
        "\t// {:.0}x{:.0} world units, {:.0}% land, mean land incline {:.1} deg,",
        analysis.world_width,
        analysis.world_height,
        analysis.land_fraction * 100.0,
        (1.0 - analysis.mean_land_slope).clamp(-1.0, 1.0).acos().to_degrees(),
    );
    let _ = writeln!(
        out,
        "\t// {} metal spots, largest walkable region {:.0}% of the map.",
        analysis.metal_spots.len(),
        analysis.largest_ground_component * 100.0,
    );

    write_start_spots(&mut out, &analysis.starts);
    out.push('\n');

    out.push_str("\t// Per-map unit limits are a balance judgement, not a terrain\n");
    out.push_str("\t// fact, so none are generated. Add them by hand if needed.\n");
    out.push_str("\tdictionary getMapUnitLimits() {\n\t\tdictionary limits;\n\t\treturn limits;\n\t}\n\n");

    write_factory_weights(&mut out, analysis);
    out.push('\n');

    // Match on the map's `name` field. The engine appends the version to it, so
    // this is always a prefix of the name an AI sees. Note that many authors put
    // the version inside `name` itself, in which case the match is pinned to that
    // release; see "Version pinning" in the README.
    let _ = writeln!(
        out,
        "\tMapConfig config = MapConfig(\"{}\", getMapUnitLimits(), spots, getFactoryWeights());",
        analysis.match_name,
    );
    out.push_str("}\n");
    out
}
