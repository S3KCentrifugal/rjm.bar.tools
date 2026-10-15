# smrt-bar-map

Deterministic terrain analysis and AI map-config generation for Beyond All Reason.

`smrt-bar-map` reads BAR map archives (`.sd7`) directly, reproduces the engine's own
interpretation of them, and emits per-map configuration that CircuitAI can
consume. It replaces hand-typed map data — start positions, roles, land-locked
flags, opening factory weights — with values derived from the map itself.

Nothing is written to the map library, the game clone, or the engine clone.

## Why it can be exact

Every value the tool needs is in the archive, and the rules for interpreting it
are in the engine source. The tool follows those rules rather than guessing:

| Question | Engine rule reproduced |
| --- | --- |
| What name does the AI see? | `name + " " + version`, unless the version already appears in the name — `ArchiveScanner.cpp:191` |
| How are heights decoded? | `min + raw * (max - min) / 65536`, with `mapinfo.smf` overriding the `.smf` header — `SMFReadMap.cpp:157` |
| How steep is a cell? | `1 - normal.y`, blended from the steepest and mean face normal — `ReadMap.cpp:770` |
| What counts as too steep? | `maxslope` degrees converted by `1 - cos(theta)` — `MoveDefHandler.cpp` |
| How deep can units go? | `DEPTH.MAX_SHALLOW` (20) to wade, `DEPTH.MIN_SHALLOW` (8) to float — BAR `gamedata/movedefs.lua` |
| Are `mapinfo` keys case-sensitive? | No. The engine lowercases the table and every lookup — `LuaParser.cpp:872` |

`mapinfo.lua` is a real Lua program, not a data file: maps merge overlays through
`VFS.Include`, branch on map options, and rewrite their own height range. The
tool therefore executes it in a sandboxed Lua 5.1 interpreter with a VFS shim
backed by the archive. No filesystem or network access is available to map code.

## Determinism

Re-running over the same archives produces byte-identical output. Collections
are emitted in sorted or otherwise fixed order, ties break on the lowest cell
index, and no result depends on hash iteration or directory ordering.

Map options are the one genuine source of variation: 154 of 227 maps read them,
and some change water damage, movement speeds, or the height range itself.
Analysis runs with the engine's default (empty) option set, and every affected
map is flagged `option_sensitive` in its output and in a comment in its config.

## Usage

```sh
# Inventory every archive: engine map name, extent, option sensitivity
smrt-bar-map scan
smrt-bar-map scan --filter glacial --json

# Inspect one map
smrt-bar-map dump    acidicquarry_5.17     # evaluated mapinfo.lua, as JSON
smrt-bar-map analyze acidicquarry_5.17     # derived terrain facts, as JSON
smrt-bar-map render  acidicquarry_5.17     # ASCII elevation bands, to check a decode by eye

# Regenerate the tracked output set
smrt-bar-map generate --out generated/circuitai-maps --with-json
```

`--maps-dir` overrides the map directory; it defaults to the standard Windows
install path under `%LOCALAPPDATA%`.

## Output

`generated/circuitai-maps/` holds the committed result: one `<Namespace>.as`
config and one `<Namespace>.json` analysis per map, named after the AngelScript
namespace the config declares.

It is tracked rather than ignored on purpose. The generator is deterministic, so
regenerating after a map-library update produces a diff that shows exactly which
maps changed and how — which is only useful if the previous run is in history.
Regenerate with the command above and review the diff; do not edit the files by
hand, since the next run will discard the edit.

`out/` is gitignored and free for ad-hoc runs.

## What it derives

Per map: world extent, land fraction, mean land incline, per-movement-class
coverage (bot, vehicle, hover, ship), walkable region count and largest region,
metal distribution, water damage, tidal strength, gravity, extractor radius.

Per start position: surrounding water fraction, mean slope, nearby metal spots
and income, share of the map reachable on foot, whether it is land-locked, and a
terrain-justified role.

Metal is classified rather than blindly enumerated. BAR ships maps whose entire
surface is metal ("Full Metal Plate", "SpeedMetal", the metal-island maps);
listing tens of thousands of extractor sites there is meaningless, so those are
reported as `field` and maps with none as `barren`.

## What it deliberately does not derive

- **`AIR` and `TECH` roles.** These are decisions about how a team divides
  labour, not properties of the ground. Only `FRONT`, `SEA` and `TACTICAL` are
  inferred; assign the rest by hand.
- **Per-map unit limits.** A balance judgement about specific units, not a
  terrain fact.

Generated files carry a "do not edit by hand" header. Keep hand-authored
decisions in a separate overlay so regeneration cannot discard them.

## Layout

| Module | Responsibility |
| --- | --- |
| `archive` | `.sd7` access; reads only the entries needed, never the 100 MB tile file |
| `smf` | Spring Map Format binary parser (height, type and metal grids) |
| `luaenv` | Sandboxed Lua 5.1 with a VFS shim and Spring globals |
| `luajson` | Lua values to JSON, with the engine's key lowercasing |
| `mapinfo` | Typed view over the evaluated info table |
| `map` | Archive to loaded map |
| `terrain` | Per-cell elevation, slope, depth band and passability |
| `analysis` | Connectivity, metal spots, per-start classification |
| `emit` | AngelScript `MapConfig` and JSON rendering |

## Version pinning

A config matches on the map's `name` field, which is always a prefix of the name
the engine reports. That collapses versions only when the author left the
version out of `name` — true for 96 of 227 maps, where `AcidicQuarry 5.16` and
`5.17` share one config.

The other 131 maps bake the version into `name` (`All That Glitters v2.2.3`,
`Altair_Crossing_V4.1`). Their configs are pinned to that exact release and will
not match the next one; the map simply falls through to the default config, as
it does today. This is a property of how those maps are named, not a choice the
tool makes, but it means the generated set needs regenerating whenever the map
library updates.

## Known limitations

- One archive in the library (`mescaline_v2`) predates `mapinfo.lua` and is
  skipped. Old `.smd` map definitions are not parsed.
- Start positions come from the map's `teams` table. BAR games normally place
  players from lobby start boxes, so these are the map author's intended spots,
  not necessarily where a given match begins.
- Chokepoints, polygon extraction and travel-time analysis are not implemented.
