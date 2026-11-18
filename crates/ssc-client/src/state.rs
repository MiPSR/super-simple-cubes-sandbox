//! Client-side world mirror + dirty tracking.
//!
//! The client owns **no game logic** (`specs.md` §2): this is a pure
//! reflection of what the server streamed, plus presentation-side state
//! (which meshes are stale). Meshing decisions are gated on stream
//! completeness so chunk-border faces are always correct.

use std::collections::{HashMap, HashSet};

use ssc_lod::{region_of, LodColumn, LodMesh};
use ssc_world::{BlockId, ColumnKey, World, CHUNK_W, MAX_Y, MIN_Y};

#[derive(Default)]
pub struct ClientWorld {
    /// Full-detail streamed columns.
    pub world: World,
    /// Columns whose stream is complete (ColumnEnd received).
    pub loaded: HashSet<ColumnKey>,
    /// LOD summaries streamed by the server.
    pub lod: HashMap<ColumnKey, LodColumn>,
    /// Columns whose meshes need a rebuild.
    pub dirty_columns: HashSet<ColumnKey>,
    /// Regions whose LOD meshes need a rebuild.
    pub dirty_regions: HashSet<(i32, i32)>,
}

impl ClientWorld {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_subchunk(&mut self, cx: i32, cz: i32, sub: u8, blocks: &[BlockId]) {
        let col = self.world.column_mut(cx, cz);
        let Some(sub_idx) = subchunk_slot(sub) else { return };
        // Replace the sub-chunk wholesale. Index order must match the
        // server's serializer: y * 256 + z * 16 + x.
        let mut sc = ssc_world::SubChunk::default();
        for y in 0..16 {
            for z in 0..16 {
                for x in 0..16 {
                    let i = (y * 16 * 16 + z * 16 + x) as usize;
                    if i < blocks.len() {
                        sc.set(x, y, z, blocks[i]);
                    }
                }
            }
        }
        col.subchunks[sub_idx] = Some(Box::new(sc));
    }

    pub fn apply_column_end(&mut self, cx: i32, cz: i32) {
        self.loaded.insert((cx, cz));
        self.dirty_columns.insert((cx, cz));
        // Neighbors must re-mesh: their border faces may now be culled
        // correctly against this column's data.
        self.dirty_columns.insert((cx - 1, cz));
        self.dirty_columns.insert((cx + 1, cz));
        self.dirty_columns.insert((cx, cz - 1));
        self.dirty_columns.insert((cx, cz + 1));
        let (rx, rz) = region_of(cx, cz);
        self.dirty_regions.insert((rx, rz));
    }

    pub fn apply_unload(&mut self, cx: i32, cz: i32) {
        self.world.columns.remove(&(cx, cz));
        self.loaded.remove(&(cx, cz));
        self.dirty_columns.remove(&(cx, cz));
        // Neighbors' border faces are exposed again.
        self.dirty_columns.insert((cx - 1, cz));
        self.dirty_columns.insert((cx + 1, cz));
        self.dirty_columns.insert((cx, cz - 1));
        self.dirty_columns.insert((cx, cz + 1));
    }

    pub fn apply_lod(&mut self, cx: i32, cz: i32, cells: &[u16; 32]) {
        self.lod.insert((cx, cz), LodColumn::from_payload(cells));
        let (rx, rz) = region_of(cx, cz);
        self.dirty_regions.insert((rx, rz));
    }

    pub fn apply_lod_unload(&mut self, cx: i32, cz: i32) {
        self.lod.remove(&(cx, cz));
        let (rx, rz) = region_of(cx, cz);
        self.dirty_regions.insert((rx, rz));
    }

    pub fn apply_block_update(&mut self, x: i32, y: i32, z: i32, block: BlockId) {
        if y < MIN_Y || y >= MAX_Y {
            return;
        }
        let (cx, _lx) = (x.div_euclid(CHUNK_W), x.rem_euclid(CHUNK_W));
        let (cz, _lz) = (z.div_euclid(CHUNK_W), z.rem_euclid(CHUNK_W));
        self.world.set_block(x, y, z, block);
        self.dirty_columns.insert((cx, cz));
        // If the edit touched a border, the neighbor's faces changed too.
        let lx = x.rem_euclid(CHUNK_W);
        let lz = z.rem_euclid(CHUNK_W);
        if lx == 0 {
            self.dirty_columns.insert((cx - 1, cz));
        }
        if lx == CHUNK_W - 1 {
            self.dirty_columns.insert((cx + 1, cz));
        }
        if lz == 0 {
            self.dirty_columns.insert((cx, cz - 1));
        }
        if lz == CHUNK_W - 1 {
            self.dirty_columns.insert((cx, cz + 1));
        }
    }

    /// Whether a column and its four neighbors are fully streamed — the
    /// condition for correct border meshing.
    pub fn meshable(&self, cx: i32, cz: i32) -> bool {
        self.loaded.contains(&(cx, cz))
            && self.loaded.contains(&(cx - 1, cz))
            && self.loaded.contains(&(cx + 1, cz))
            && self.loaded.contains(&(cx, cz - 1))
            && self.loaded.contains(&(cx, cz + 1))
    }

    /// Pop the columns ready to mesh.
    pub fn take_dirty_columns(&mut self) -> Vec<ColumnKey> {
        let ready: Vec<ColumnKey> = self
            .dirty_columns
            .iter()
            .copied()
            .filter(|(cx, cz)| self.meshable(*cx, *cz))
            .collect();
        for k in &ready {
            self.dirty_columns.remove(k);
        }
        ready
    }

    /// Pop the regions ready to re-mesh (all of them; the LOD builder
    /// tolerates missing columns by leaving holes).
    pub fn take_dirty_regions(&mut self) -> Vec<(i32, i32)> {
        let ready: Vec<(i32, i32)> = self.dirty_regions.iter().copied().collect();
        self.dirty_regions.clear();
        ready
    }

    /// Build a region LOD mesh snapshot (for the mesh worker).
    pub fn build_region(&self, rx: i32, rz: i32, pcx: i32, pcz: i32, near_margin: i32) -> Option<LodMesh> {
        let lod = self.lod.clone(); // small (32 bytes per column)
        let built = ssc_lod::build_region_mesh(rx, rz, pcx, pcz, near_margin, &|cx, cz| {
            lod.get(&(cx, cz)).copied()
        });
        match built {
            ssc_lod::LodBuild::Mesh(m) => Some(m),
            ssc_lod::LodBuild::Empty => None,
        }
    }
}

fn subchunk_slot(sub: u8) -> Option<usize> {
    ((sub as usize) < SUBCHUNKS).then_some(sub as usize)
}

const SUBCHUNKS: usize = (MAX_Y - MIN_Y) as usize / 16;
