//! Chunk meshing for the client renderer.
//!
//! Emits one mesh per 16³ sub-chunk, containing only the **exposed** faces:
//! a face is emitted when the neighboring block is not opaque. Neighbor
//! lookups cross column borders (3×3 column neighborhood), and meshing is
//! only scheduled once a column and its four neighbors are fully streamed
//! (see `state.rs`), so borders are always correct — this is the "proper
//! culling" foundation of `specs.md` §3.

use std::collections::{HashSet, VecDeque};

use ssc_engine::{atlas_tile_uv, Vertex};
use ssc_math::Vec3;
use ssc_world::{
    is_opaque, Column, World, CHUNK_W, ColumnKey, MIN_Y, SUBCHUNKS_PER_COLUMN,
};

/// A built sub-chunk mesh with tight bounds (tight AABBs cull better).
/// `translucent` marks water (drawn in the blend pass).
#[derive(Debug, Default)]
pub struct SubchunkMesh {
    pub sub: usize,
    pub translucent: bool,
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

// ---------------------------------------------------------------------------
// Occlusion ("cave") culling data — Minecraft-style border visibility.
//
// For each of the six faces of a sub-chunk we record, per slice, a bitmask
// of border cells that are NON-OPAQUE ("open"). Two adjacent sub-chunks
// are connected (you can see from one into the other) when the pair of
// opposing masks share at least one open cell. A BFS from the camera's
// sub-chunk over these connections yields the set of potentially visible
// sub-chunks: everything else is occluded and must not be drawn.
//
// All-air sub-chunks have no mesh but still carry all-open masks — they
// are pass-through nodes (looking over a hill into the valley behind).
// ---------------------------------------------------------------------------

/// Slice indexing per face:
/// - `x_neg` / `x_pos`: 16 slices over **y**, bit **z**.
/// - `y_neg` / `y_pos`: 16 slices over **z**, bit **x**.
/// - `z_neg` / `z_pos`: 16 slices over **y**, bit **x**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaceMasks {
    pub x_neg: [u16; 16],
    pub x_pos: [u16; 16],
    pub y_neg: [u16; 16],
    pub y_pos: [u16; 16],
    pub z_neg: [u16; 16],
    pub z_pos: [u16; 16],
}

impl Default for FaceMasks {
    fn default() -> Self {
        Self::all_open()
    }
}

impl FaceMasks {
    /// Every border cell open (all-air sub-chunk).
    pub fn all_open() -> Self {
        Self {
            x_neg: [u16::MAX; 16],
            x_pos: [u16::MAX; 16],
            y_neg: [u16::MAX; 16],
            y_pos: [u16::MAX; 16],
            z_neg: [u16::MAX; 16],
            z_pos: [u16::MAX; 16],
        }
    }
}

/// Compute the masks of one sub-chunk from its own cells only
/// (openness is a per-cell property; the pair test handles the border).
pub fn compute_face_masks(sc: &ssc_world::SubChunk) -> FaceMasks {
    let mut m = FaceMasks {
        x_neg: [0; 16],
        x_pos: [0; 16],
        y_neg: [0; 16],
        y_pos: [0; 16],
        z_neg: [0; 16],
        z_pos: [0; 16],
    };
    for y in 0..16usize {
        for z in 0..16usize {
            if !is_opaque(sc.get(0, y as i32, z as i32)) {
                m.x_neg[y] |= 1 << z;
            }
            if !is_opaque(sc.get(15, y as i32, z as i32)) {
                m.x_pos[y] |= 1 << z;
            }
        }
        for x in 0..16usize {
            if !is_opaque(sc.get(x as i32, y as i32, 0)) {
                m.z_neg[y] |= 1 << x;
            }
            if !is_opaque(sc.get(x as i32, y as i32, 15)) {
                m.z_pos[y] |= 1 << x;
            }
        }
    }
    for z in 0..16usize {
        for x in 0..16usize {
            if !is_opaque(sc.get(x as i32, 0, z as i32)) {
                m.y_neg[z] |= 1 << x;
            }
            if !is_opaque(sc.get(x as i32, 15, z as i32)) {
                m.y_pos[z] |= 1 << x;
            }
        }
    }
    m
}

/// Key of a sub-chunk in visibility terms.
pub type SubKey = (ColumnKey, usize);

/// The six axis-aligned neighbor relations with their opposing mask
/// slices. X neighbors pair `x_pos` (slices over y) with `x_neg`.
fn connects_x(a: &FaceMasks, b: &FaceMasks) -> bool {
    (0..16).any(|y| a.x_pos[y] & b.x_neg[y] != 0)
}

fn connects_x_neg(a: &FaceMasks, b: &FaceMasks) -> bool {
    (0..16).any(|y| a.x_neg[y] & b.x_pos[y] != 0)
}

fn connects_y(a: &FaceMasks, b: &FaceMasks) -> bool {
    (0..16).any(|z| a.y_pos[z] & b.y_neg[z] != 0)
}

fn connects_y_neg(a: &FaceMasks, b: &FaceMasks) -> bool {
    (0..16).any(|z| a.y_neg[z] & b.y_pos[z] != 0)
}

fn connects_z(a: &FaceMasks, b: &FaceMasks) -> bool {
    (0..16).any(|y| a.z_pos[y] & b.z_neg[y] != 0)
}

fn connects_z_neg(a: &FaceMasks, b: &FaceMasks) -> bool {
    (0..16).any(|y| a.z_neg[y] & b.z_pos[y] != 0)
}

/// Breadth-first visibility flood from `start` over sub-chunk borders.
///
/// `get` returns the [`FaceMasks`] of a sub-chunk (world column coords,
/// sub index) or `None` when the sub-chunk isn't streamed/meshed — such
/// nodes are treated as open and pass-through (never false-cull during
/// streaming). Returns `None` when `start` itself is unknown (caller
/// should skip filtering).
///
/// The result is the **draw set**: sub-chunks reachable by light *plus one
/// ring around them*. A fully solid sub-chunk is not traversable (no light
/// passes through it), but its faces are visible from any adjacent
/// lit sub-chunk — hence the dilation. The dilation is applied only to
/// flood nodes, never cascaded, so stone behind stone stays culled.
pub fn visible_flood(
    start: SubKey,
    get: &dyn Fn(SubKey) -> Option<FaceMasks>,
) -> Option<HashSet<SubKey>> {
    // The start node must be known; otherwise fall back to no filtering.
    get(start)?;

    let mut flood: HashSet<SubKey> = HashSet::new();
    let mut queue: VecDeque<SubKey> = VecDeque::new();
    flood.insert(start);
    queue.push_back(start);

    while let Some(node) = queue.pop_front() {
        let (cx, cz, sub) = (node.0 .0, node.0 .1, node.1);
        // Six neighbors. Y clamps at the world bounds.
        let neighbors: [(i32, i32, i32); 6] = [
            (cx + 1, cz, sub as i32),
            (cx - 1, cz, sub as i32),
            (cx, cz + 1, sub as i32),
            (cx, cz - 1, sub as i32),
            (cx, cz, sub as i32 + 1),
            (cx, cz, sub as i32 - 1),
        ];
        for (i, (nx, nz, nsub)) in neighbors.iter().enumerate() {
            let nsub_u = *nsub as usize;
            if *nsub < 0 || nsub_u >= SUBCHUNKS_PER_COLUMN {
                continue;
            }
            let nkey: SubKey = ((*nx, *nz), nsub_u);
            if flood.contains(&nkey) {
                continue;
            }
            let a = get(node);
            // Traversal requires BOTH sides to be known: an unknown
            // sub-chunk has no meshes anyway, and walking into unknown
            // space would flood the entire plane (unbounded BFS).
            let Some(b) = get(nkey) else { continue };
            let Some(a) = a else { continue };
            let connected = match i {
                // +X, -X, +Z, -Z, +Y, -Y in neighbor order above.
                0 => connects_x(&a, &b),
                1 => connects_x_neg(&a, &b),
                2 => connects_z(&a, &b),
                3 => connects_z_neg(&a, &b),
                4 => connects_y(&a, &b),
                _ => connects_y_neg(&a, &b),
            };
            if connected {
                flood.insert(nkey);
                queue.push_back(nkey);
            }
        }
    }

    // Dilate by one ring around the lit sub-chunks (draw walls/floors of
    // geometry adjacent to reachable air). Non-cascading by construction:
    // we iterate over a snapshot of the flood set.
    let mut draw = flood.clone();
    for node in flood.iter() {
        let (cx, cz, sub) = (node.0 .0, node.0 .1, node.1 as i32);
        for (dx, dz, dy) in [
            (1, 0, 0),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ] {
            let nsub = sub + dy;
            if nsub < 0 || nsub as usize >= SUBCHUNKS_PER_COLUMN {
                continue;
            }
            draw.insert(((cx + dx, cz + dz), nsub as usize));
        }
    }
    Some(draw)
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
/// sub-chunks that produce geometry. Also returns visibility masks for
/// **every** sub-chunk of the column (empty ones get all-open masks —
/// they are pass-through nodes for occlusion culling).
pub fn build_column_meshes(world: &World, cx: i32, cz: i32) -> (Vec<SubchunkMesh>, Vec<(usize, FaceMasks)>) {
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
        return (Vec::new(), Vec::new());
    };

    let base_x = cx * CHUNK_W;
    let base_z = cz * CHUNK_W;
    let mut out = Vec::new();
    let mut masks_out = Vec::with_capacity(SUBCHUNKS_PER_COLUMN);

    for sub in 0..SUBCHUNKS_PER_COLUMN {
        let Some(sc) = column.subchunks[sub].as_deref() else {
            // All-air sub-chunk: pass-through for visibility.
            masks_out.push((sub, FaceMasks::all_open()));
            continue;
        };
        masks_out.push((sub, compute_face_masks(sc)));
        let base_y = MIN_Y + (sub as i32) * 16;

        // Two buffers per sub-chunk: opaque and translucent (water).
        let mut solid = SubchunkMesh {
            sub,
            translucent: false,
            verts: Vec::new(),
            indices: Vec::new(),
            min: Vec3::splat(f32::MAX),
            max: Vec3::splat(f32::MIN),
        };
        let mut water = SubchunkMesh {
            sub,
            translucent: true,
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
                    let mesh = if info.opaque { &mut solid } else { &mut water };

                    for f in &FACES {
                        let nb = get(
                            wx + f.neighbor[0],
                            wy + f.neighbor[1],
                            wz + f.neighbor[2],
                        );
                        // Hidden face — culled at mesh time: either the
                        // neighbor occludes (opaque) or it is the same
                        // block type (no internal water/leaves faces).
                        if is_opaque(nb) || nb == id {
                            continue;
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

        if !solid.is_empty() {
            out.push(solid);
        }
        if !water.is_empty() {
            out.push(water);
        }
    }
    (out, masks_out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_world::{Column, SubChunk, STONE};

    fn world_with_solid_wall() -> (World, Vec<(usize, FaceMasks)>) {
        // Column (0,0) with a solid stone sub-chunk wall at sub 10,
        // air everywhere else (missing sub-chunks read as air).
        let mut world = World::default();
        let mut col = Column::default();
        col.subchunks[10] = Some(Box::new(SubChunk::filled(STONE)));
        world.columns.insert((0, 0), col);
        let (_, masks) = build_column_meshes(&world, 0, 0);
        (world, masks)
    }

    fn mask_lookup(
        masks: &[(usize, FaceMasks)],
    ) -> impl Fn(SubKey) -> Option<FaceMasks> + '_ {
        move |k: SubKey| {
            if k.0 != (0, 0) {
                return None; // outside the test column: unknown → open
            }
            masks.iter().find(|(s, _)| *s == k.1).map(|(_, m)| *m)
        }
    }

    #[test]
    fn solid_wall_occludes_air_behind_it() {
        let (_world, masks) = world_with_solid_wall();
        // Camera in the air sub-chunk ABOVE the wall (sub 11).
        let visible = visible_flood(((0, 0), 11), &mask_lookup(&masks)).unwrap();
        // The wall itself is drawn (its top faces are lit).
        assert!(visible.contains(&((0, 0), 10)));
        // Air below the wall cannot be reached — nothing below is drawn.
        assert!(!visible.contains(&((0, 0), 9)));
        assert!(!visible.contains(&((0, 0), 8)));
    }

    #[test]
    fn flood_is_bounded_to_known_subchunks() {
        let (_world, masks) = world_with_solid_wall();
        // Camera in the air sub-chunk above the wall; surrounding columns
        // are UNKNOWN (not streamed) — the flood must not walk into them
        // (it would never terminate otherwise).
        let visible = visible_flood(((0, 0), 11), &mask_lookup(&masks)).unwrap();
        // The camera node and its dilated ring are visible.
        assert!(visible.contains(&((0, 0), 11)));
        assert!(visible.contains(&((1, 0), 11)));
        // Two columns away: unknown, never traversed, never dilated.
        assert!(!visible.contains(&((2, 0), 11)));
    }

    #[test]
    fn unknown_start_disables_filtering() {
        let (_world, masks) = world_with_solid_wall();
        // A camera in an unknown sub-chunk (sub 2 has no mask entry —
        // wait, it does: absent sub-chunks get all-open masks) — use an
        // unknown COLUMN instead.
        assert!(visible_flood(((9, 9), 5), &mask_lookup(&masks)).is_none());
    }

    #[test]
    fn masks_of_solid_subchunk_are_closed() {
        let sc = SubChunk::filled(STONE);
        let m = compute_face_masks(&sc);
        assert_eq!(m.x_neg, [0u16; 16]);
        assert_eq!(m.y_pos, [0u16; 16]);
        let air = SubChunk::default();
        let m = compute_face_masks(&air);
        assert_eq!(m.x_pos, [u16::MAX; 16]);
        assert_eq!(m.z_neg, [u16::MAX; 16]);
    }

    #[test]
    fn water_merges_with_itself_but_surfaces_against_air() {
        // 3×3 world; every column: stone floor (sub 6), water y 48..53
        // (sub 7 local 0..5), air above. Only the water top faces remain.
        let mut world = World::default();
        for cx in -1..=1 {
            for cz in -1..=1 {
                let mut col = Column::default();
                let mut floor = Box::new(SubChunk::default());
                for z in 0..16 {
                    for x in 0..16 {
                        // Top layer of sub 6 = world y 47, right below the
                        // water (sub 7 starts at world y 48).
                        floor.set(x, 15, z, STONE);
                    }
                }
                col.subchunks[6] = Some(floor);
                let mut sc = Box::new(SubChunk::default());
                for y in 0..6 {
                    for z in 0..16 {
                        for x in 0..16 {
                            sc.set(x, y, z, ssc_world::WATER);
                        }
                    }
                }
                col.subchunks[7] = Some(sc);
                world.columns.insert((cx, cz), col);
            }
        }

        let (meshes, _masks) = build_column_meshes(&world, 0, 0);
        let water = meshes.iter().find(|m| m.translucent).expect("water mesh");
        // 16×16 top quads only: sides merge with same-id water, the bottom
        // is hidden by the stone floor.
        assert_eq!(water.verts.len(), 4 * 16 * 16);
    }

    #[test]
    fn solid_faces_below_water_surface_are_drawn() {
        // 3×3 world; every column: stone at y=48 (sub 7 local 0), water
        // above it (local 1..5). The stone top face must exist (visible
        // through the translucent water); its sides merge with the
        // neighboring stone, its bottom is hidden by nothing below.
        let mut world = World::default();
        for cx in -1..=1 {
            for cz in -1..=1 {
                let mut col = Column::default();
                let mut sc = Box::new(SubChunk::default());
                for z in 0..16 {
                    for x in 0..16 {
                        sc.set(x, 0, z, STONE);
                        for y in 1..6 {
                            sc.set(x, y, z, ssc_world::WATER);
                        }
                    }
                }
                col.subchunks[7] = Some(sc);
                world.columns.insert((cx, cz), col);
            }
        }

        let (meshes, _masks) = build_column_meshes(&world, 0, 0);
        let solid = meshes.iter().find(|m| !m.translucent).expect("solid mesh");
        // Stone top faces (water is not opaque → drawn) plus the stone
        // bottom faces (below is nothing). Sides merge with same-id stone.
        assert_eq!(solid.verts.len(), 4 * 16 * 16 * 2);
        // Water: top faces only (sides are against same-id water).
        let water = meshes.iter().find(|m| m.translucent).expect("water mesh");
        assert_eq!(water.verts.len(), 4 * 16 * 16);
    }
}
