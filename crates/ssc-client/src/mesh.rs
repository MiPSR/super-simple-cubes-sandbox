//! Chunk meshing for the client renderer.
//!
//! Two-stage culling per sub-chunk:
//! 1. **Face culling**: a face exists only where the neighboring block is
//!    neither opaque nor the same block type (no internal water faces).
//!    Neighbor lookups cross column borders (3×3 column neighborhood);
//!    meshing waits for the 4 side neighbors to be streamed.
//! 2. **Greedy meshing**: identical faces on a slice are merged into
//!    maximal rectangles — vertex counts drop several-fold on flat
//!    terrain. Merged quads keep per-block texture repeat through the
//!    engine's atlas-aware shader (local repeating UVs + tile origin).
//!
//! Occlusion data ([`FaceMasks`], [`visible_flood`]) is computed here too.

use std::collections::{HashSet, VecDeque};

use ssc_engine::{atlas_tile_origin, Vertex};
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

// ---------------------------------------------------------------------------
// Occlusion flood
// ---------------------------------------------------------------------------

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
/// nodes are never traversed (they carry no meshes, and walking into
/// unknown space would be an unbounded flood). Returns `None` when
/// `start` itself is unknown (caller should skip filtering).
///
/// The result is the **draw set**: sub-chunks reachable by light *plus
/// one ring around them*. A fully solid sub-chunk is not traversable
/// (no light passes through it), but its faces are visible from any
/// adjacent lit sub-chunk — hence the dilation. The dilation is applied
/// only to flood nodes, never cascaded, so stone behind stone stays
/// culled.
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

// ---------------------------------------------------------------------------
// Greedy meshing
// ---------------------------------------------------------------------------

/// Axis selector for a face's u/v plane coordinates.
#[derive(Clone, Copy)]
struct PlaneAxis {
    /// 0 = x, 1 = y, 2 = z.
    axis: usize,
    /// Whether face-plane coordinate u maps inverted onto the block axis
    /// (block axis = 15 − u).
    invert: bool,
}

/// The six cube faces: CCW corner winding + local (u,v) per corner +
/// the plane-axis mapping. Corner positions are reconstructed as
/// `pos[normal] = slice + corner[normal]` and
/// `pos[u] = invert ? 16 − pu : pu` (same for v).
struct GreedyFace {
    /// Axis index of the face normal (x=0, y=1, z=2).
    normal: usize,
    /// Sign of the face normal (+1/−1).
    normal_sign: i32,
    u: PlaneAxis,
    v: PlaneAxis,
    /// Registry face index for tile lookup.
    face: usize,
    /// Corner local (u_sel, v_sel) in CCW order.
    uv_sel: [(u8, u8); 4],
    /// Corner normal-axis offset (0/1) in CCW order.
    normal_off: [i32; 4],
}

const GREEDY_FACES: [GreedyFace; 6] = [
    // +Y (top): u = x, v = −z
    GreedyFace { normal: 1, normal_sign: 1, u: PlaneAxis { axis: 0, invert: false }, v: PlaneAxis { axis: 2, invert: true }, face: ssc_world::FACE_TOP, uv_sel: [(0, 0), (1, 0), (1, 1), (0, 1)], normal_off: [1, 1, 1, 1] },
    // −Y (bottom): u = x, v = z
    GreedyFace { normal: 1, normal_sign: -1, u: PlaneAxis { axis: 0, invert: false }, v: PlaneAxis { axis: 2, invert: false }, face: ssc_world::FACE_BOTTOM, uv_sel: [(0, 0), (1, 0), (1, 1), (0, 1)], normal_off: [0, 0, 0, 0] },
    // −X: u = −z, v = y
    GreedyFace { normal: 0, normal_sign: -1, u: PlaneAxis { axis: 2, invert: true }, v: PlaneAxis { axis: 1, invert: false }, face: ssc_world::FACE_NX, uv_sel: [(0, 0), (0, 1), (1, 1), (1, 0)], normal_off: [0, 0, 0, 0] },
    // +X: u = z, v = y
    GreedyFace { normal: 0, normal_sign: 1, u: PlaneAxis { axis: 2, invert: false }, v: PlaneAxis { axis: 1, invert: false }, face: ssc_world::FACE_PX, uv_sel: [(0, 0), (0, 1), (1, 1), (1, 0)], normal_off: [1, 1, 1, 1] },
    // −Z: u = −x, v = y
    GreedyFace { normal: 2, normal_sign: -1, u: PlaneAxis { axis: 0, invert: true }, v: PlaneAxis { axis: 1, invert: false }, face: ssc_world::FACE_NZ, uv_sel: [(0, 0), (1, 0), (1, 1), (0, 1)], normal_off: [0, 0, 0, 0] },
    // +Z: u = x, v = y
    GreedyFace { normal: 2, normal_sign: 1, u: PlaneAxis { axis: 0, invert: false }, v: PlaneAxis { axis: 1, invert: false }, face: ssc_world::FACE_PZ, uv_sel: [(0, 0), (1, 0), (1, 1), (0, 1)], normal_off: [1, 1, 1, 1] },
];

/// Emit one greedy quad. `(u0, v0, w, h)` are face-plane coordinates of
/// the merged rectangle.
#[allow(clippy::too_many_arguments)]
fn emit_greedy_quad(
    f: &GreedyFace,
    base: [i32; 3],
    slice: i32,
    u0: usize,
    v0: usize,
    w: usize,
    h: usize,
    tile: u16,
    mesh: &mut SubchunkMesh,
) {
    let origin = atlas_tile_origin(tile);
    let base_idx = mesh.verts.len() as u32;
    for k in 0..4 {
        let (us, vs) = f.uv_sel[k];
        // Continuous plane coordinates → repeating local UVs.
        let pu = (u0 + w * (us as usize)) as i32;
        let pv = (v0 + h * (vs as usize)) as i32;
        let mut pos = [0i32; 3];
        pos[f.normal] = base[f.normal] + slice + f.normal_off[k];
        pos[f.u.axis] = base[f.u.axis] + if f.u.invert { 16 - pu } else { pu };
        pos[f.v.axis] = base[f.v.axis] + if f.v.invert { 16 - pv } else { pv };
        let p = Vec3::new(pos[0] as f32, pos[1] as f32, pos[2] as f32);
        mesh.verts.push(Vertex {
            pos: [p.x, p.y, p.z],
            uv: [pu as f32, pv as f32],
            tile: origin,
        });
        mesh.min = mesh.min.min(p);
        mesh.max = mesh.max.max(p);
    }
    mesh.indices
        .extend_from_slice(&[base_idx, base_idx + 1, base_idx + 2, base_idx, base_idx + 2, base_idx + 3]);
}

/// Build all sub-chunk meshes of one column with greedy meshing. Only
/// emits meshes for sub-chunks that produce geometry. Also returns
/// visibility masks for **every** sub-chunk of the column (empty ones get
/// all-open masks — they are pass-through nodes for occlusion culling).
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
        let base = [base_x, base_y, base_z];

        // Two buffers per sub-chunk: opaque and translucent (water).
        let mut solid = SubchunkMesh {
            sub,
            translucent: false,
            ..Default::default()
        };
        let mut water = SubchunkMesh {
            sub,
            translucent: true,
            ..Default::default()
        };

        // Face-slice masks per direction, per pass. mask[v][u] = block id
        // whose face exists there (0 = no face).
        let mut mask_opaque = [[0u16; 16]; 16];
        let mut mask_water = [[0u16; 16]; 16];

        for f in &GREEDY_FACES {
            for pass_water in [false, true] {
                // Build the slice mask for every slice along the normal.
                for slice in 0..16i32 {
                    let mut any = false;
                    for v in 0..16usize {
                        for u in 0..16usize {
                            // Face-plane coords → block cell coords.
                            let cu = if f.u.invert { 15 - u as i32 } else { u as i32 };
                            let cv = if f.v.invert { 15 - v as i32 } else { v as i32 };
                            let mut cell = [0i32; 3];
                            cell[f.u.axis] = cu;
                            cell[f.v.axis] = cv;
                            cell[f.normal] = slice;
                            let id = sc.get(cell[0], cell[1], cell[2]);
                            if id == ssc_world::AIR {
                                continue;
                            }
                            let info = ssc_world::block_info(id);
                            if pass_water != !info.opaque {
                                continue; // wrong pass
                            }
                            // Neighbor across the face (world coords).
                            let nb = get(
                                base[0] + cell[0] + f.normal_sign * (f.normal == 0) as i32,
                                base[1] + cell[1] + f.normal_sign * (f.normal == 1) as i32,
                                base[2] + cell[2] + f.normal_sign * (f.normal == 2) as i32,
                            );
                            // Hidden face — culled at mesh time: either the
                            // neighbor occludes (opaque) or it is the same
                            // block type (no internal water faces).
                            if is_opaque(nb) || nb == id {
                                continue;
                            }
                            if pass_water {
                                mask_water[v][u] = id;
                            } else {
                                mask_opaque[v][u] = id;
                            }
                            any = true;
                        }
                    }
                    if !any {
                        continue;
                    }
                    // Greedy merge on this slice.
                    let mask = if pass_water { &mut mask_water } else { &mut mask_opaque };
                    for v in 0..16usize {
                        let mut u = 0usize;
                        while u < 16 {
                            let id = mask[v][u];
                            if id == 0 {
                                u += 1;
                                continue;
                            }
                            // Width: extend while same id.
                            let mut w = 1usize;
                            while u + w < 16 && mask[v][u + w] == id {
                                w += 1;
                            }
                            // Height: extend while the whole strip matches.
                            let mut h = 1usize;
                            'h: while v + h < 16 {
                                for k in 0..w {
                                    if mask[v + h][u + k] != id {
                                        break 'h;
                                    }
                                }
                                h += 1;
                            }
                            // Emit.
                            let tile = ssc_world::block_info(id).tiles[f.face];
                            let target = if pass_water { &mut water } else { &mut solid };
                            emit_greedy_quad(f, base, slice, u, v, w, h, tile, target);
                            // Clear the merged rectangle.
                            for dv in 0..h {
                                for du in 0..w {
                                    mask[v + dv][u + du] = 0;
                                }
                            }
                            u += w;
                        }
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

// ---------------------------------------------------------------------------
// Entity avatars: three boxes in local space, feet at y=0.
// ---------------------------------------------------------------------------

/// A local-space box for entity avatars: min/max corners + per-face tile.
struct BoxDef {
    min: [f32; 3],
    max: [f32; 3],
    tiles: [u16; 6],
}

/// Placeholder player avatar: three boxes (legs, body, head) in local
/// space. Minecraft proportions; placeholder atlas tiles until real
/// player skins exist (`implemented-features.md`).
pub fn build_avatar() -> (Vec<Vertex>, Vec<u32>) {
    let boxes = [
        // legs
        BoxDef {
            min: [-0.125, 0.0, -0.125],
            max: [0.125, 0.7, 0.125],
            tiles: [ssc_world::TILE_BRICKS; 6],
        },
        // body
        BoxDef {
            min: [-0.25, 0.7, -0.125],
            max: [0.25, 1.4, 0.125],
            tiles: [ssc_world::TILE_PLANKS; 6],
        },
        // head
        BoxDef {
            min: [-0.25, 1.4, -0.25],
            max: [0.25, 1.8, 0.25],
            tiles: [ssc_world::TILE_SNOW; 6],
        },
    ];
    let mut verts = Vec::new();
    let mut indices = Vec::new();
    for b in &boxes {
        push_box(&mut verts, &mut indices, b);
    }
    (verts, indices)
}

/// Emit all six faces of one local-space box (unit-UV per face, so the
/// tile maps once across the face — the right look for avatar parts).
fn push_box(verts: &mut Vec<Vertex>, indices: &mut Vec<u32>, b: &BoxDef) {
    for f in &GREEDY_FACES {
        let tile = b.tiles[f.face];
        let origin = atlas_tile_origin(tile);
        let base_idx = verts.len() as u32;
        for k in 0..4 {
            let (us, vs) = f.uv_sel[k];
            let pu = us as i32;
            let pv = vs as i32;
            let mut pos = [0f32; 3];
            // Unit box: normal offset selects min(0) / max(1).
            pos[f.u.axis] = if f.u.invert { 1.0 - pu as f32 } else { pu as f32 };
            pos[f.v.axis] = if f.v.invert { 1.0 - pv as f32 } else { pv as f32 };
            pos[f.normal] = f.normal_off[k] as f32;
            let p = [
                b.min[0] + pos[0] * (b.max[0] - b.min[0]),
                b.min[1] + pos[1] * (b.max[1] - b.min[1]),
                b.min[2] + pos[2] * (b.max[2] - b.min[2]),
            ];
            verts.push(Vertex {
                pos: p,
                uv: [pu as f32, pv as f32],
                tile: origin,
            });
        }
        indices.extend_from_slice(&[
            base_idx,
            base_idx + 1,
            base_idx + 2,
            base_idx,
            base_idx + 2,
            base_idx + 3,
        ]);
    }
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

    fn column_with(f: impl Fn(&mut ssc_world::SubChunk)) -> World {
        let mut world = World::default();
        for cx in -1..=1 {
            for cz in -1..=1 {
                let mut col = Column::default();
                let mut sc = Box::new(SubChunk::default());
                f(&mut sc);
                col.subchunks[7] = Some(sc);
                world.columns.insert((cx, cz), col);
            }
        }
        world
    }

    #[test]
    fn greedy_merges_flat_stone_floor() {
        let world = column_with(|sc| {
            for z in 0..16 {
                for x in 0..16 {
                    sc.set(x, 0, z, STONE);
                }
            }
        });
        let (meshes, _) = build_column_meshes(&world, 0, 0);
        let solid = meshes.iter().find(|m| !m.translucent).expect("solid mesh");
        // Top + bottom faces, each merged to a single 16×16 quad.
        assert_eq!(solid.verts.len(), 8, "greedy should merge the flat floor");
        // Local repeating UVs span the whole merged face.
        assert_eq!(solid.verts[2].uv, [16.0, 16.0]);
    }

    #[test]
    fn water_merges_with_itself_but_surfaces_against_air() {
        // Stone floor at the top of sub 6, water y 48..53 (sub 7 local
        // 0..5), air above: one greedy quad for the water surface.
        let world = column_with(|sc| {
            for y in 0..6 {
                for z in 0..16 {
                    for x in 0..16 {
                        sc.set(x, y, z, ssc_world::WATER);
                    }
                }
            }
        });
        // The floor under the water lives in sub 6 of every column.
        let world = {
            let mut w = world;
            for cx in -1..=1 {
                for cz in -1..=1 {
                    let col = w.columns.get_mut(&(cx, cz)).unwrap();
                    let mut floor = Box::new(SubChunk::default());
                    for z in 0..16 {
                        for x in 0..16 {
                            floor.set(x, 15, z, STONE);
                        }
                    }
                    col.subchunks[6] = Some(floor);
                }
            }
            w
        };

        let (meshes, _masks) = build_column_meshes(&world, 0, 0);
        let water = meshes.iter().find(|m| m.translucent).expect("water mesh");
        // One greedy quad for the whole surface: sides merge with
        // same-id water, the bottom is hidden by the stone floor.
        assert_eq!(water.verts.len(), 4);
    }

    #[test]
    fn solid_faces_below_water_surface_are_drawn() {
        // Stone at y=48 (sub 7 local 0), water above (local 1..5): the
        // stone top must exist (visible through the translucent water).
        let world = column_with(|sc| {
            for z in 0..16 {
                for x in 0..16 {
                    sc.set(x, 0, z, STONE);
                    for y in 1..6 {
                        sc.set(x, y, z, ssc_world::WATER);
                    }
                }
            }
        });

        let (meshes, _masks) = build_column_meshes(&world, 0, 0);
        let solid = meshes.iter().find(|m| !m.translucent).expect("solid mesh");
        // Stone top (drawn through water) + stone bottom, one greedy quad
        // each; sides merge with the neighboring stone.
        assert_eq!(solid.verts.len(), 8);
        // Water: top faces only, one merged quad.
        let water = meshes.iter().find(|m| m.translucent).expect("water mesh");
        assert_eq!(water.verts.len(), 4);
    }
}
