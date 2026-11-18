//! Chunk storage.
//!
//! Minecraft-like chunking: 16×16 columns spanning the full world height as
//! sub-chunks of 16³. Matches the "Java 1.18+" world shape (−64..320) while
//! keeping Bedrock-style 16³ sub-chunk granularity on the wire.
//!
//! This is pure data — generation and modification logic live in the server.

use std::collections::HashMap;

use super::{is_solid, BlockId, AIR};

/// Horizontal chunk size in blocks.
pub const CHUNK_W: i32 = 16;
/// Sub-chunk height in blocks.
pub const SUBCHUNK_H: i32 = 16;
/// Lowest world Y (inclusive).
pub const MIN_Y: i32 = -64;
/// Highest world Y (exclusive).
pub const MAX_Y: i32 = 320;
/// World height in blocks.
pub const WORLD_HEIGHT: i32 = MAX_Y - MIN_Y;
/// Sub-chunks stacked per column.
pub const SUBCHUNKS_PER_COLUMN: usize = (WORLD_HEIGHT / SUBCHUNK_H) as usize; // 24

/// Blocks per sub-chunk.
pub const SUBCHUNK_VOLUME: usize = (SUBCHUNK_H * CHUNK_W * CHUNK_W) as usize; // 4096

/// Column (chunk) coordinates.
pub type ColumnKey = (i32, i32);

/// Index of the sub-chunk containing world Y.
#[inline]
pub fn subchunk_index(y: i32) -> Option<usize> {
    let idx = y - MIN_Y;
    if idx < 0 {
        None
    } else {
        let idx = (idx / SUBCHUNK_H) as usize;
        (idx < SUBCHUNKS_PER_COLUMN).then_some(idx)
    }
}

#[inline]
fn local_index(x: i32, y: i32, z: i32) -> usize {
    (y * CHUNK_W * CHUNK_W + z * CHUNK_W + x) as usize
}

/// One 16³ sub-chunk of block ids.
pub struct SubChunk {
    pub blocks: Box<[BlockId; SUBCHUNK_VOLUME]>,
}

impl SubChunk {
    pub fn filled(id: BlockId) -> Self {
        Self { blocks: Box::new([id; SUBCHUNK_VOLUME]) }
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32, z: i32) -> BlockId {
        self.blocks[local_index(x, y, z)]
    }

    #[inline]
    pub fn set(&mut self, x: i32, y: i32, z: i32, id: BlockId) {
        self.blocks[local_index(x, y, z)] = id;
    }

    /// Whether the sub-chunk contains any non-air block.
    pub fn has_content(&self) -> bool {
        self.blocks.iter().any(|&b| b != AIR)
    }

    /// Flatten to the wire layout — the single source of truth for the
    /// sub-chunk stream format (`y*256 + z*16 + x`). Both the server
    /// encoder and client decoder must use this; never re-implement it.
    pub fn to_wire(&self) -> Vec<BlockId> {
        let mut out = Vec::with_capacity(SUBCHUNK_VOLUME);
        for y in 0..SUBCHUNK_H {
            for z in 0..CHUNK_W {
                for x in 0..CHUNK_W {
                    out.push(self.get(x, y, z));
                }
            }
        }
        out
    }

    /// Parse the wire layout produced by [`SubChunk::to_wire`].
    /// Returns `None` if the payload has the wrong length.
    pub fn from_wire(blocks: &[BlockId]) -> Option<Self> {
        if blocks.len() != SUBCHUNK_VOLUME {
            return None;
        }
        let mut sc = Self::default();
        for y in 0..SUBCHUNK_H {
            for z in 0..CHUNK_W {
                for x in 0..CHUNK_W {
                    sc.set(x, y, z, blocks[(y * CHUNK_W * CHUNK_W + z * CHUNK_W + x) as usize]);
                }
            }
        }
        Some(sc)
    }
}

impl Default for SubChunk {
    fn default() -> Self {
        Self::filled(AIR)
    }
}

/// A full-height 16×16 column made of [`SUBCHUNKS_PER_COLUMN`] sub-chunks.
/// Sub-chunks are allocated lazily: `None` means "all air".
pub struct Column {
    pub subchunks: Vec<Option<Box<SubChunk>>>,
}

impl Default for Column {
    fn default() -> Self {
        Self {
            subchunks: (0..SUBCHUNKS_PER_COLUMN).map(|_| None).collect(),
        }
    }
}

impl Column {
    #[inline]
    fn sub_or_air(&self, sub: usize) -> Option<&SubChunk> {
        self.subchunks.get(sub).and_then(|s| s.as_deref())
    }

    /// Get a block at column-local coordinates. `x`/`z` must be in `0..16`,
    /// `y` in `MIN_Y..MAX_Y`.
    pub fn get(&self, x: i32, y: i32, z: i32) -> BlockId {
        match subchunk_index(y).and_then(|s| self.sub_or_air(s)) {
            Some(sc) => sc.get(x, y.rem_euclid(SUBCHUNK_H), z),
            None => AIR,
        }
    }

    /// Set a block, allocating the sub-chunk if needed.
    /// Returns false if the coordinates were out of range.
    pub fn set(&mut self, x: i32, y: i32, z: i32, id: BlockId) -> bool {
        let Some(sub) = subchunk_index(y) else { return false };
        let sc = self.subchunks[sub]
            .get_or_insert_with(|| Box::new(SubChunk::default()));
        sc.set(x, y.rem_euclid(SUBCHUNK_H), z, id);
        true
    }

    /// Highest non-air Y in the column at these local x/z, or `None`.
    pub fn top_solid_y(&self, x: i32, z: i32) -> Option<i32> {
        for sub in (0..SUBCHUNKS_PER_COLUMN).rev() {
            if let Some(sc) = self.sub_or_air(sub) {
                for y in (0..SUBCHUNK_H).rev() {
                    let wy = MIN_Y + sub as i32 * SUBCHUNK_H + y;
                    if is_solid(sc.get(x, y, z)) {
                        return Some(wy);
                    }
                }
            }
        }
        None
    }
}

/// The voxel world, a sparse map of columns. Shared data structure: the
/// server owns the authoritative one; the client mirrors what it is sent.
pub struct World {
    pub columns: HashMap<ColumnKey, Column>,
}

impl Default for World {
    fn default() -> Self {
        Self { columns: HashMap::new() }
    }
}

impl World {
    pub fn column(&self, cx: i32, cz: i32) -> Option<&Column> {
        self.columns.get(&(cx, cz))
    }

    pub fn column_mut(&mut self, cx: i32, cz: i32) -> &mut Column {
        self.columns.entry((cx, cz)).or_default()
    }

    /// Block at world coordinates, or `None` when the column or sub-chunk
    /// is not loaded (treated as air by callers that can tolerate it).
    pub fn get_block(&self, x: i32, y: i32, z: i32) -> Option<BlockId> {
        if y < MIN_Y || y >= MAX_Y {
            return None;
        }
        let (cx, lx) = (x.div_euclid(CHUNK_W), x.rem_euclid(CHUNK_W));
        let (cz, lz) = (z.div_euclid(CHUNK_W), z.rem_euclid(CHUNK_W));
        self.column(cx, cz).map(|c| c.get(lx, y, lz))
    }

    /// Convenience: block or air when unloaded.
    pub fn block_or_air(&self, x: i32, y: i32, z: i32) -> BlockId {
        self.get_block(x, y, z).unwrap_or(AIR)
    }

    /// Set a block. Returns false if Y was out of range (the column is
    /// created if missing).
    pub fn set_block(&mut self, x: i32, y: i32, z: i32, id: BlockId) -> bool {
        if y < MIN_Y || y >= MAX_Y {
            return false;
        }
        let (cx, lx) = (x.div_euclid(CHUNK_W), x.rem_euclid(CHUNK_W));
        let (cz, lz) = (z.div_euclid(CHUNK_W), z.rem_euclid(CHUNK_W));
        self.column_mut(cx, cz).set(lx, y, lz, id)
    }
}
