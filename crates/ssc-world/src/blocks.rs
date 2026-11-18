//! Block registry.
//!
//! Block ids are stable `u16` values used on the wire and in storage.
//! Texture tiles reference the client's texture atlas layout; the atlas
//! MUST provide the tiles listed here (16×16 tiles of 16×16 px, PNG-compatible,
//! so real Minecraft texture packs can be loaded later, `specs.md` §3).

/// Numeric block id used everywhere (storage + wire).
pub type BlockId = u16;

pub const AIR: BlockId = 0;
pub const BEDROCK: BlockId = 1;
pub const STONE: BlockId = 2;
pub const COBBLESTONE: BlockId = 3;
pub const DIRT: BlockId = 4;
pub const GRASS_BLOCK: BlockId = 5;
pub const SAND: BlockId = 6;
pub const SANDSTONE: BlockId = 7;
pub const GRAVEL: BlockId = 8;
pub const OAK_LOG: BlockId = 9;
pub const OAK_PLANKS: BlockId = 10;
pub const OAK_LEAVES: BlockId = 11;
pub const SNOW_BLOCK: BlockId = 12;
pub const BRICKS: BlockId = 13;

// Atlas tile ids (16×16 tiles per row/column in the 256×256 atlas).
pub const TILE_MISSING: u16 = 0;
pub const TILE_STONE: u16 = 1;
pub const TILE_COBBLESTONE: u16 = 2;
pub const TILE_DIRT: u16 = 3;
pub const TILE_GRASS_TOP: u16 = 4;
pub const TILE_GRASS_SIDE: u16 = 5;
pub const TILE_SAND: u16 = 6;
pub const TILE_SANDSTONE_TOP: u16 = 7;
pub const TILE_SANDSTONE_SIDE: u16 = 8;
pub const TILE_GRAVEL: u16 = 9;
pub const TILE_LOG_TOP: u16 = 10;
pub const TILE_LOG_SIDE: u16 = 11;
pub const TILE_PLANKS: u16 = 12;
pub const TILE_LEAVES: u16 = 13;
pub const TILE_SNOW: u16 = 14;
pub const TILE_BRICKS: u16 = 15;

/// Face indices into [`BlockInfo::tiles`].
pub const FACE_TOP: usize = 0;
pub const FACE_BOTTOM: usize = 1;
pub const FACE_NX: usize = 2;
pub const FACE_PX: usize = 3;
pub const FACE_NZ: usize = 4;
pub const FACE_PZ: usize = 5;

/// All six faces in a fixed order.
pub const FACES: [usize; 6] = [FACE_TOP, FACE_BOTTOM, FACE_NX, FACE_PX, FACE_NZ, FACE_PZ];

/// Static block description. No behavior — behavior lives in the server.
pub struct BlockInfo {
    pub name: &'static str,
    /// Blocks movement.
    pub solid: bool,
    /// Fully hides neighboring faces (mesh culling).
    pub opaque: bool,
    /// Atlas tile per face: top, bottom, -X, +X, -Z, +Z.
    pub tiles: [u16; 6],
}

const fn t(top: u16, bottom: u16, side: u16) -> [u16; 6] {
    [top, bottom, side, side, side, side]
}

/// The block registry, indexed by `BlockId`. Ids must stay contiguous.
pub const BLOCKS: &[BlockInfo] = &[
    // 0 — air
    BlockInfo { name: "air", solid: false, opaque: false, tiles: [TILE_MISSING; 6] },
    // 1 — bedrock
    BlockInfo { name: "bedrock", solid: true, opaque: true, tiles: t(TILE_STONE, TILE_STONE, TILE_STONE) },
    // 2 — stone
    BlockInfo { name: "stone", solid: true, opaque: true, tiles: t(TILE_STONE, TILE_STONE, TILE_STONE) },
    // 3 — cobblestone
    BlockInfo { name: "cobblestone", solid: true, opaque: true, tiles: t(TILE_COBBLESTONE, TILE_COBBLESTONE, TILE_COBBLESTONE) },
    // 4 — dirt
    BlockInfo { name: "dirt", solid: true, opaque: true, tiles: t(TILE_DIRT, TILE_DIRT, TILE_DIRT) },
    // 5 — grass block
    BlockInfo { name: "grass_block", solid: true, opaque: true, tiles: t(TILE_GRASS_TOP, TILE_DIRT, TILE_GRASS_SIDE) },
    // 6 — sand
    BlockInfo { name: "sand", solid: true, opaque: true, tiles: t(TILE_SAND, TILE_SAND, TILE_SAND) },
    // 7 — sandstone
    BlockInfo { name: "sandstone", solid: true, opaque: true, tiles: t(TILE_SANDSTONE_TOP, TILE_SANDSTONE_TOP, TILE_SANDSTONE_SIDE) },
    // 8 — gravel
    BlockInfo { name: "gravel", solid: true, opaque: true, tiles: t(TILE_GRAVEL, TILE_GRAVEL, TILE_GRAVEL) },
    // 9 — oak log
    BlockInfo { name: "oak_log", solid: true, opaque: true, tiles: t(TILE_LOG_TOP, TILE_LOG_TOP, TILE_LOG_SIDE) },
    // 10 — oak planks
    BlockInfo { name: "oak_planks", solid: true, opaque: true, tiles: t(TILE_PLANKS, TILE_PLANKS, TILE_PLANKS) },
    // 11 — oak leaves
    BlockInfo { name: "oak_leaves", solid: true, opaque: true, tiles: t(TILE_LEAVES, TILE_LEAVES, TILE_LEAVES) },
    // 12 — snow block
    BlockInfo { name: "snow_block", solid: true, opaque: true, tiles: t(TILE_SNOW, TILE_SNOW, TILE_SNOW) },
    // 13 — bricks
    BlockInfo { name: "bricks", solid: true, opaque: true, tiles: t(TILE_BRICKS, TILE_BRICKS, TILE_BRICKS) },
];

/// Number of registered blocks (valid ids are `0..BLOCK_COUNT`).
pub const BLOCK_COUNT: u16 = BLOCKS.len() as u16;

/// Validate a decoded block id (protocol safety).
pub fn valid_id(id: BlockId) -> bool {
    (id as usize) < BLOCKS.len()
}

/// Static info for a valid block id. Panics on invalid ids — always validate
/// wire input with [`valid_id`] first.
pub fn info(id: BlockId) -> &'static BlockInfo {
    &BLOCKS[id as usize]
}

/// Whether the block blocks movement.
pub fn is_solid(id: BlockId) -> bool {
    id != AIR && BLOCKS[id as usize].solid
}

/// Whether the block fully occludes the neighbor faces behind it.
pub fn is_opaque(id: BlockId) -> bool {
    id != AIR && BLOCKS[id as usize].opaque
}
