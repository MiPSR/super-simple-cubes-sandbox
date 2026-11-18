//! Super Simple Cubes — LOD system (Distant Horizons-style).
//!
//! Far terrain is rendered through this crate from day one (`specs.md` §3).
//! Design: the **server** streams coarse column summaries ([`LodColumn`]);
//! the client builds one heightfield mesh per 8×8-column region with
//! [`build_region_mesh`] and draws it beyond the near render distance.
//! This is presentation only — never gameplay logic.

/// Blocks per LOD cell edge (a cell summarizes a 4×4 block area).
pub const CELL_STRIDE: i32 = 4;
/// LOD cells per column edge (16/4).
pub const CELLS_PER_COL: i32 = 4;
/// Columns per region edge (a region is 8×8 columns = 128×128 blocks).
pub const REGION_COLS: i32 = 8;
/// LOD cells per region edge.
pub const CELLS_PER_REGION: i32 = REGION_COLS * CELLS_PER_COL; // 32
/// Interleaved cell payload size: [height, tile] per cell.
pub const CELLS_PAYLOAD: usize = (CELLS_PER_COL * CELLS_PER_COL * 2) as usize; // 32

/// Region coordinates from column coordinates.
pub fn region_of(cx: i32, cz: i32) -> (i32, i32) {
    (cx.div_euclid(REGION_COLS), cz.div_euclid(REGION_COLS))
}

/// The LOD summary of one column, as streamed by the server:
/// 4×4 cells of `(surface_height, top_tile)`.
///
/// Wire layout is the flat `[u16; 32]` from `ServerPacket::LodColumn`;
/// `cells` is row-major in cells: index = `cz_local * 4 + cx_local`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LodColumn {
    pub cells: [[u16; 2]; 16],
}

impl LodColumn {
    /// Parse the wire payload. `height == 0` means void (nothing drawn).
    pub fn from_payload(cells: &[u16; 32]) -> Self {
        let mut out = [[0u16; 2]; 16];
        for i in 0..16 {
            out[i] = [cells[i * 2], cells[i * 2 + 1]];
        }
        LodColumn { cells: out }
    }

    /// Height (surface Y) of a cell, column-local cell coords.
    #[inline]
    pub fn height(&self, cx: i32, cz: i32) -> u16 {
        self.cells[(cz * CELLS_PER_COL + cx) as usize][0]
    }

    /// Atlas tile of a cell's surface, column-local cell coords.
    #[inline]
    pub fn tile(&self, cx: i32, cz: i32) -> u16 {
        self.cells[(cz * CELLS_PER_COL + cx) as usize][1]
    }
}

/// A built LOD mesh: interleaved `[x, y, z, u, v, tile]` floats — `u/v`
/// are local repeating coordinates (0..1 per cell) and `tile` is the
/// atlas tile **id**; the presentation layer (engine) maps ids to atlas
/// UVs. This crate stays atlas-layout-agnostic.
#[derive(Debug, Default)]
pub struct LodMesh {
    pub verts: Vec<[f32; 6]>,
    pub indices: Vec<u32>,
    /// Vertical extent of the emitted geometry (world coords).
    pub min_y: f32,
    pub max_y: f32,
}

/// Result of a region mesh build.
#[derive(Debug)]
pub enum LodBuild {
    /// Mesh data plus whether anything was emitted.
    Mesh(LodMesh),
    /// Region has no visible cells right now.
    Empty,
}

/// Mesh a region. `near` is the set of column keys covered by the near
/// full-detail renderer: cells in those columns are skipped so the LOD
/// surface never z-fights with the detailed terrain (`pcx`/`pcz` is the
/// player's current column; cells within `near_margin` columns of it are
/// considered near).
///
/// `lookup` gives the [`LodColumn`] for any world column (returns `None`
/// when not streamed yet). Vertices carry atlas tile **ids**; the atlas
/// layout is owned by the presentation layer (engine/client), NOT this
/// crate. Skirts are only emitted between cells of the same region;
/// region borders may show hairline gaps for now.
pub fn build_region_mesh(
    rx: i32,
    rz: i32,
    pcx: i32,
    pcz: i32,
    near_margin: i32,
    lookup: &dyn Fn(i32, i32) -> Option<LodColumn>,
) -> LodBuild {
    let origin_cx = rx * REGION_COLS;
    let origin_cz = rz * REGION_COLS;
    let origin_x = origin_cx * 16;
    let origin_z = origin_cz * 16;

    let mut mesh = LodMesh {
        verts: Vec::new(),
        indices: Vec::new(),
        min_y: f32::MAX,
        max_y: f32::MIN,
    };
    let mut any = false;

    // Cell coordinates in region space.
    for lx in 0..CELLS_PER_REGION {
        for lz in 0..CELLS_PER_REGION {
            let col_x = origin_cx + lx / CELLS_PER_COL;
            let col_z = origin_cz + lz / CELLS_PER_COL;
            let is_near = (col_x - pcx).abs() <= near_margin
                && (col_z - pcz).abs() <= near_margin;
            if is_near {
                continue;
            }
            let Some(lcol) = lookup(col_x, col_z) else { continue };
            let h = lcol.height(lx % CELLS_PER_COL, lz % CELLS_PER_COL);
            if h == 0 {
                continue;
            }
            let tile = lcol.tile(lx % CELLS_PER_COL, lz % CELLS_PER_COL);
            any = true;
            let hf = h as f32;
            mesh.min_y = mesh.min_y.min(hf - 1.0);
            mesh.max_y = mesh.max_y.max(hf);

            let x0 = (origin_x + lx * CELL_STRIDE) as f32;
            let z0 = (origin_z + lz * CELL_STRIDE) as f32;
            let x1 = x0 + CELL_STRIDE as f32;
            let z1 = z0 + CELL_STRIDE as f32;

            let uv = tile;
            push_top_quad(&mut mesh, x0, z0, x1, z1, hf, uv);
            push_skirts(&mut mesh, rx, rz, lx, lz, x0, z0, hf, tile, lookup);
        }
    }

    if !any {
        return LodBuild::Empty;
    }
    LodBuild::Mesh(mesh)
}

fn push_top_quad(mesh: &mut LodMesh, x0: f32, z0: f32, x1: f32, z1: f32, y: f32, tile: u16) {
    let base = mesh.verts.len() as u32;
    // CCW viewed from above; one full texture per LOD cell.
    mesh.verts.push([x0, y, z1, 0.0, 0.0, tile as f32]);
    mesh.verts.push([x1, y, z1, 1.0, 0.0, tile as f32]);
    mesh.verts.push([x1, y, z0, 1.0, 1.0, tile as f32]);
    mesh.verts.push([x0, y, z0, 0.0, 1.0, tile as f32]);
    mesh.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// Side skirts where a neighbor cell is lower.
#[allow(clippy::too_many_arguments)]
fn push_skirts(
    mesh: &mut LodMesh,
    _rx: i32,
    _rz: i32,
    lx: i32,
    lz: i32,
    x0: f32,
    z0: f32,
    h: f32,
    tile: u16,
    lookup: &dyn Fn(i32, i32) -> Option<LodColumn>,
) {
    let x1 = x0 + CELL_STRIDE as f32;
    let z1 = z0 + CELL_STRIDE as f32;
    let uv = tile;
    let origin_cx = _rx * REGION_COLS;
    let origin_cz = _rz * REGION_COLS;

    let cell_world = |lx: i32, lz: i32| -> Option<u16> {
        let cx = origin_cx + lx / CELLS_PER_COL;
        let cz = origin_cz + lz / CELLS_PER_COL;
        lookup(cx, cz)
            .map(|c| c.height(lx.rem_euclid(CELLS_PER_COL), lz.rem_euclid(CELLS_PER_COL)))
    };

    // -X neighbor (may live in the previous region; cell math handles it)
    let nh = cell_world(lx - 1, lz).unwrap_or(0);
    if nh < h as u16 && nh != 0 {
        push_side_quad(mesh, [x0, h, z1], [x0, h, z0], [x0, nh as f32, z0], [x0, nh as f32, z1], uv);
    }
    // +X neighbor
    let nh = cell_world(lx + 1, lz).unwrap_or(0);
    if nh < h as u16 && nh != 0 {
        push_side_quad(mesh, [x1, h, z0], [x1, h, z1], [x1, nh as f32, z1], [x1, nh as f32, z0], uv);
    }
    // -Z neighbor (may live in the previous region)
    let nh = cell_world(lx, lz - 1).unwrap_or(0);
    if nh < h as u16 && nh != 0 {
        push_side_quad(mesh, [x0, h, z0], [x1, h, z0], [x1, nh as f32, z0], [x0, nh as f32, z0], uv);
    }
    // +Z neighbor
    let nh = cell_world(lx, lz + 1).unwrap_or(0);
    if nh < h as u16 && nh != 0 {
        push_side_quad(mesh, [x1, h, z1], [x0, h, z1], [x0, nh as f32, z1], [x1, nh as f32, z1], uv);
    }
}

/// Quad given in CCW order viewed from outside; `[..]` = [x, y, z].
fn push_side_quad(mesh: &mut LodMesh, a: [f32; 3], b: [f32; 3], c: [f32; 3], d: [f32; 3], tile: u16) {
    let base = mesh.verts.len() as u32;
    let t = tile as f32;
    mesh.verts.push([a[0], a[1], a[2], 0.0, 1.0, t]);
    mesh.verts.push([b[0], b[1], b[2], 1.0, 1.0, t]);
    mesh.verts.push([c[0], c[1], c[2], 1.0, 0.0, t]);
    mesh.verts.push([d[0], d[1], d[2], 0.0, 0.0, t]);
    mesh.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}
