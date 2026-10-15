//! Parser for the Spring Map Format (`.smf`) binary container.
//!
//! Layout and field semantics follow `rts/Map/SMF/SMFFormat.h` in the Recoil engine.
//! Only the three grids needed for terrain analysis are decoded; tiles, minimap and
//! features are skipped.

use anyhow::{Context, Result, bail, ensure};

/// Distance in world units between adjacent heightmap samples (`SMFHeader::squareSize`).
pub const SQUARE_SIZE: i32 = 8;

const MAGIC: &[u8; 16] = b"spring map file\0";
const HEADER_LEN: usize = 80;

/// Raw header fields, in on-disk declaration order.
///
/// Every documented field is kept even where this tool does not consume it, so
/// the struct stays a faithful description of the format and offsets can be
/// checked against `SMFFormat.h` by eye.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub version: i32,
    pub map_id: i32,
    /// Heightmap squares along x. World width is `map_x * SQUARE_SIZE`.
    pub map_x: i32,
    pub map_y: i32,
    pub square_size: i32,
    pub texel_per_square: i32,
    pub tile_size: i32,
    /// World height that heightmap value `0` encodes.
    pub min_height: f32,
    /// World height that heightmap value `0xffff` encodes.
    pub max_height: f32,
    pub heightmap_ptr: i32,
    pub type_map_ptr: i32,
    pub tiles_ptr: i32,
    pub minimap_ptr: i32,
    pub metalmap_ptr: i32,
    pub feature_ptr: i32,
    pub num_extra_headers: i32,
}

/// One map feature placement: rocks, trees, geothermal vents and the like.
#[derive(Debug, Clone)]
pub struct Feature {
    /// Feature definition name, e.g. `geovent` or a tree/rock model name.
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// A decoded `.smf`: the header plus the grids used by terrain analysis.
#[derive(Debug, Clone)]
pub struct Smf {
    pub header: Header,
    /// Corner heights, `(map_x + 1) * (map_y + 1)` samples in world units, row-major.
    pub heights: Vec<f32>,
    /// Terrain type index per half-square, `(map_x / 2) * (map_y / 2)`, row-major.
    pub type_map: Vec<u8>,
    /// Raw metal content per half-square, same extent as `type_map`.
    pub metal_map: Vec<u8>,
    /// Features placed by the map author, in file order.
    pub features: Vec<Feature>,
}

impl Smf {
    /// Heightmap sample count along x (one more than the number of squares).
    pub fn height_stride(&self) -> usize {
        self.header.map_x as usize + 1
    }

    /// Width of the half-resolution grids used by `type_map` and `metal_map`.
    pub fn half_width(&self) -> usize {
        self.header.map_x as usize / 2
    }

    pub fn half_height(&self) -> usize {
        self.header.map_y as usize / 2
    }

    /// Map extent in world units.
    pub fn world_size(&self) -> (f32, f32) {
        (
            (self.header.map_x * self.header.square_size) as f32,
            (self.header.map_y * self.header.square_size) as f32,
        )
    }

    /// Height at a heightmap corner, clamped to the grid.
    pub fn height_at_corner(&self, x: usize, z: usize) -> f32 {
        let stride = self.height_stride();
        let x = x.min(stride - 1);
        let z = z.min(self.header.map_y as usize);
        self.heights[z * stride + x]
    }
}

fn i32_at(buf: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]])
}

fn f32_at(buf: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes([buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]])
}

fn parse_header(buf: &[u8]) -> Result<Header> {
    ensure!(buf.len() >= HEADER_LEN, "file is shorter than an SMF header");
    ensure!(&buf[..16] == MAGIC, "missing 'spring map file' magic");

    let header = Header {
        version: i32_at(buf, 16),
        map_id: i32_at(buf, 20),
        map_x: i32_at(buf, 24),
        map_y: i32_at(buf, 28),
        square_size: i32_at(buf, 32),
        texel_per_square: i32_at(buf, 36),
        tile_size: i32_at(buf, 40),
        min_height: f32_at(buf, 44),
        max_height: f32_at(buf, 48),
        heightmap_ptr: i32_at(buf, 52),
        type_map_ptr: i32_at(buf, 56),
        tiles_ptr: i32_at(buf, 60),
        minimap_ptr: i32_at(buf, 64),
        metalmap_ptr: i32_at(buf, 68),
        feature_ptr: i32_at(buf, 72),
        num_extra_headers: i32_at(buf, 76),
    };

    ensure!(header.version == 1, "unsupported SMF version {}", header.version);
    ensure!(
        header.map_x > 0 && header.map_y > 0,
        "degenerate map extent {}x{}",
        header.map_x,
        header.map_y
    );
    ensure!(
        header.square_size == SQUARE_SIZE,
        "unexpected square size {}",
        header.square_size
    );
    Ok(header)
}

fn slice_at<'a>(buf: &'a [u8], ptr: i32, len: usize, what: &str) -> Result<&'a [u8]> {
    let start = usize::try_from(ptr).with_context(|| format!("negative {what} offset"))?;
    let end = start
        .checked_add(len)
        .with_context(|| format!("{what} extent overflows"))?;
    if end > buf.len() {
        bail!("{what} extends past end of file ({end} > {})", buf.len());
    }
    Ok(&buf[start..end])
}

/// Decode the header and the height, type and metal grids.
pub fn parse(buf: &[u8]) -> Result<Smf> {
    let header = parse_header(buf)?;

    let corners = (header.map_x as usize + 1) * (header.map_y as usize + 1);
    let half = (header.map_x as usize / 2) * (header.map_y as usize / 2);

    // Heights are stored as u16 spanning [min_height, max_height]; see SMFReadMap.cpp.
    let scale = (header.max_height - header.min_height) / 65536.0;
    let raw = slice_at(buf, header.heightmap_ptr, corners * 2, "heightmap")?;
    let heights = raw
        .chunks_exact(2)
        .map(|c| header.min_height + u16::from_le_bytes([c[0], c[1]]) as f32 * scale)
        .collect();

    let type_map = slice_at(buf, header.type_map_ptr, half, "typemap")?.to_vec();
    let metal_map = slice_at(buf, header.metalmap_ptr, half, "metalmap")?.to_vec();
    let features = parse_features(buf, header.feature_ptr).unwrap_or_default();

    Ok(Smf { header, heights, type_map, metal_map, features })
}

/// Decode the feature block: a count header, then that many NUL-terminated type
/// names, then one fixed-size record per placement.
///
/// A malformed or absent block is not fatal - the grids are what analysis needs -
/// so callers receive an empty list rather than an error.
fn parse_features(buf: &[u8], ptr: i32) -> Result<Vec<Feature>> {
    const RECORD_LEN: usize = 24;

    let head = slice_at(buf, ptr, 8, "feature header")?;
    let type_count = usize::try_from(i32_at(head, 0)).unwrap_or(0);
    let feature_count = usize::try_from(i32_at(head, 4)).unwrap_or(0);
    if type_count == 0 || feature_count == 0 {
        return Ok(Vec::new());
    }

    let mut cursor = usize::try_from(ptr).unwrap_or(0) + 8;
    let mut names = Vec::with_capacity(type_count);
    for _ in 0..type_count {
        let end = buf[cursor..]
            .iter()
            .position(|&b| b == 0)
            .map(|offset| cursor + offset)
            .context("unterminated feature type name")?;
        names.push(String::from_utf8_lossy(&buf[cursor..end]).into_owned());
        cursor = end + 1;
    }

    let records = slice_at(buf, cursor as i32, feature_count * RECORD_LEN, "feature records")?;
    Ok(records
        .chunks_exact(RECORD_LEN)
        .filter_map(|record| {
            let kind = usize::try_from(i32_at(record, 0)).ok()?;
            Some(Feature {
                name: names.get(kind)?.clone(),
                x: f32_at(record, 4),
                y: f32_at(record, 8),
                z: f32_at(record, 12),
            })
        })
        .collect())
}
