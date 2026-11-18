//! Chunk meshing for the client renderer.
//!
//! Emits one mesh per 16³ sub-chunk, containing only the **exposed** faces:
//! a face is emitted when the neighboring block is not opaque. Neighbor
//! lookups cross column borders (3×3 column neighborhood), and meshing is
//! only scheduled once a column and its four neighbors are fully streamed
//! (see `state.rs`), so borders are always correct — this is the "proper
//! culling" foundation of `specs.md` §3.

use ssc_engine::{atlas_tile_uv, Vertex};
use ssc_math::Vec3;
use ssc_world::{
    is_opaque, Column, World, CHUNK_W, MIN_Y, SUBCHUNKS_PER_COLUMN,
};

/// A built sub-chunk mesh with tight bounds (tight AABBs cull better).
#[derive(Debug, Default)]
pub struct SubchunkMesh {
    pub sub: usize,
    pub verts: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub min: Vec3,
    pub max: Vec3,
}

impl SubchunkMesh {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// Face definition: 4 corners (CCW from outside) + per-corner UV selection
/// in tile units (`0`/`1` within the tile rect) + the neighbor offset.
struct FaceDef {
    corners: [[i32; 3]; 4],
    /// (u_sel, v_sel) per corner: 0 = left/bottom of tile, 1 = right/top.
    uv_sel: [(u8, u8); 4],
    neighbor: [i32; 3],
    face: usize,
}

/// The six cube faces, verified CCW when viewed from outside (GL front).
const FACES: [FaceDef; 6] = [
    // +Y (top)
    FaceDef {
        corners: [[0, 1, 1], [1, 1, 1], [1, 1, 0], [0, 1, 0]],
        uv_sel: [(0, 0), (1, 0), (1, 1), (0, 1)],
        neighbor: [0, 1, 0],
        face: ssc_world::FACE_TOP,
    },
    // -Y (bottom)
    FaceDef {
        corners: [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]],
        uv_sel: [(0, 0), (1, 0), (1, 1), (0, 1)],
        neighbor: [0, -1, 0],
        face: ssc_world::FACE_BOTTOM,
    },
    // -X
    FaceDef {
        corners: [[0, 0, 1], [0, 1, 1], [0, 1, 0], [0, 0, 0]],
        uv_sel: [(0, 0), (0, 1), (1, 1), (1, 0)],
        neighbor: [-1, 0, 0],
        face: ssc_world::FACE_NX,
    },
    // +X
    FaceDef {
        corners: [[1, 0, 0], [1, 1, 0], [1, 1, 1], [1, 0, 1]],
        uv_sel: [(0, 0), (0, 1), (1, 1), (1, 0)],
        neighbor: [1, 0, 0],
        face: ssc_world::FACE_PX,
    },
    // -Z
    FaceDef {
        corners: [[1, 0, 0], [0, 0, 0], [0, 1, 0], [1, 1, 0]],
        uv_sel: [(0, 0), (1, 0), (1, 1), (0, 1)],
        neighbor: [0, 0, -1],
        face: ssc_world::FACE_NZ,
    },
    // +Z
    FaceDef {
        corners: [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]],
        uv_sel: [(0, 0), (1, 0), (1, 1), (0, 1)],
        neighbor: [0, 0, 1],
        face: ssc_world::FACE_PZ,
    },
];

/// Build all sub-chunk meshes of one column. Only emits meshes for
/// sub-chunks that produce geometry.
pub fn build_column_meshes(world: &World, cx: i32, cz: i32) -> Vec<SubchunkMesh> {
    // Cache the 3×3 column neighborhood for fast cross-border lookups.
    let mut cols: [Option<&Column>; 9] = [None; 9];
    for dx in -1i32..=1 {
        for dz in -1i32..=1 {
            cols[((dx + 1) * 3 + (dz + 1)) as usize] = world.column(cx + dx, cz + dz);
        }
    }

    // Block at world coords, limited to the cached neighborhood.
    let get = |wx: i32, wy: i32, wz: i32| -> u16 {
        if wy < MIN_Y || wy >= MIN_Y + 16 * SUBCHUNKS_PER_COLUMN as i32 {
            return ssc_world::AIR;
        }
        let dcx = wx.div_euclid(CHUNK_W) - cx;
        let dcz = wz.div_euclid(CHUNK_W) - cz;
        if dcx < -1 || dcx > 1 || dcz < -1 || dcz > 1 {
            return ssc_world::AIR;
        }
        let c = cols[((dcx + 1) * 3 + (dcz + 1)) as usize];
        match c {
            Some(c) => c.get(wx.rem_euclid(CHUNK_W), wy, wz.rem_euclid(CHUNK_W)),
            None => ssc_world::AIR,
        }
    };

    let Some(column) = world.column(cx, cz) else {
        return Vec::new();
    };

    let base_x = cx * CHUNK_W;
    let base_z = cz * CHUNK_W;
    let mut out = Vec::new();

    for sub in 0..SUBCHUNKS_PER_COLUMN {
        let Some(sc) = column.subchunks[sub].as_deref() else { continue };
        let base_y = MIN_Y + (sub as i32) * 16;

        let mut mesh = SubchunkMesh {
            sub,
            verts: Vec::new(),
            indices: Vec::new(),
            min: Vec3::splat(f32::MAX),
            max: Vec3::splat(f32::MIN),
        };

        for y in 0..16 {
            for z in 0..16 {
                for x in 0..16 {
                    let id = sc.get(x, y, z);
                    if id == ssc_world::AIR {
                        continue;
                    }
                    let info = ssc_world::info(id);
                    let (wx, wy, wz) = (base_x + x, base_y + y, base_z + z);

                    for f in &FACES {
                        let nb = get(
                            wx + f.neighbor[0],
                            wy + f.neighbor[1],
                            wz + f.neighbor[2],
                        );
                        if is_opaque(nb) {
                            continue; // hidden face — culled at mesh time
                        }
                        let [u0, v0, u1, v1] = atlas_tile_uv(info.tiles[f.face]);
                        let base = mesh.verts.len() as u32;
                        for (i, corner) in f.corners.iter().enumerate() {
                            let px = (wx + corner[0]) as f32;
                            let py = (wy + corner[1]) as f32;
                            let pz = (wz + corner[2]) as f32;
                            let (us, vs) = f.uv_sel[i];
                            let u = if us == 0 { u0 } else { u1 };
                            let v = if vs == 0 { v0 } else { v1 };
                            mesh.verts.push(Vertex { pos: [px, py, pz], uv: [u, v] });
                            mesh.min = mesh.min.min(Vec3::new(px, py, pz));
                            mesh.max = mesh.max.max(Vec3::new(px, py, pz));
                        }
                        mesh.indices
                            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
                    }
                }
            }
        }

        if !mesh.is_empty() {
            out.push(mesh);
        }
    }
    out
}
