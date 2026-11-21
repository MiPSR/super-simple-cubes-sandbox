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
pub const WATER: BlockId = 14;
pub const COAL_ORE: BlockId = 15;
pub const IRON_ORE: BlockId = 16;

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
pub const TILE_WATER: u16 = 16; // second atlas row
pub const TILE_COAL_ORE: u16 = 17;
pub const TILE_IRON_ORE: u16 = 18;

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
    /// Breaking hardness (wiki values). `f32::INFINITY` = unbreakable.
    pub hardness: f32,
    /// Whether a proper tool is required (hand digging is 5× slower
    /// instead of 1.5×; no tools exist yet, so it is always the slow
    /// path for these blocks).
    pub needs_tool: bool,
}

const fn t(top: u16, bottom: u16, side: u16) -> [u16; 6] {
    [top, bottom, side, side, side, side]
}

/// Full block constructor with wiki hardness values.
const fn b(name: &'static str, tiles: [u16; 6], hardness: f32, needs_tool: bool) -> BlockInfo {
    BlockInfo {
        name,
        solid: true,
        opaque: true,
        tiles,
        hardness,
        needs_tool,
    }
}

/// The block registry, indexed by `BlockId`. Ids must stay contiguous.
pub const BLOCKS: &[BlockInfo] = &[
    // 0 — air
    BlockInfo {
        name: "air",
        solid: false,
        opaque: false,
        tiles: [TILE_MISSING; 6],
        hardness: f32::INFINITY,
        needs_tool: false,
    },
    // 1 — bedrock (unbreakable)
    BlockInfo {
        name: "bedrock",
        solid: true,
        opaque: true,
        tiles: t(TILE_STONE, TILE_STONE, TILE_STONE),
        hardness: f32::INFINITY,
        needs_tool: true,
    },
    // 2 — stone (hardness 1.5)
    b("stone", t(TILE_STONE, TILE_STONE, TILE_STONE), 1.5, true),
    // 3 — cobblestone (hardness 2)
    b(
        "cobblestone",
        t(TILE_COBBLESTONE, TILE_COBBLESTONE, TILE_COBBLESTONE),
        2.0,
        true,
    ),
    // 4 — dirt (hardness 0.5)
    b("dirt", t(TILE_DIRT, TILE_DIRT, TILE_DIRT), 0.5, false),
    // 5 — grass block (hardness 0.6)
    b(
        "grass_block",
        t(TILE_GRASS_TOP, TILE_DIRT, TILE_GRASS_SIDE),
        0.6,
        false,
    ),
    // 6 — sand (hardness 0.5)
    b("sand", t(TILE_SAND, TILE_SAND, TILE_SAND), 0.5, false),
    // 7 — sandstone (hardness 0.8)
    b(
        "sandstone",
        t(TILE_SANDSTONE_TOP, TILE_SANDSTONE_TOP, TILE_SANDSTONE_SIDE),
        0.8,
        true,
    ),
    // 8 — gravel (hardness 0.6)
    b(
        "gravel",
        t(TILE_GRAVEL, TILE_GRAVEL, TILE_GRAVEL),
        0.6,
        false,
    ),
    // 9 — oak log (hardness 2)
    b(
        "oak_log",
        t(TILE_LOG_TOP, TILE_LOG_TOP, TILE_LOG_SIDE),
        2.0,
        false,
    ),
    // 10 — oak planks (hardness 2)
    b(
        "oak_planks",
        t(TILE_PLANKS, TILE_PLANKS, TILE_PLANKS),
        2.0,
        false,
    ),
    // 11 — oak leaves (hardness 0.2)
    b(
        "oak_leaves",
        t(TILE_LEAVES, TILE_LEAVES, TILE_LEAVES),
        0.2,
        false,
    ),
    // 12 — snow block (hardness 0.2)
    b("snow_block", t(TILE_SNOW, TILE_SNOW, TILE_SNOW), 0.2, false),
    // 13 — bricks (hardness 2)
    b(
        "bricks",
        t(TILE_BRICKS, TILE_BRICKS, TILE_BRICKS),
        2.0,
        true,
    ),
    // 14 — water: no collision, light passes, drawn translucent.
    BlockInfo {
        name: "water",
        solid: false,
        opaque: false,
        tiles: t(TILE_WATER, TILE_WATER, TILE_WATER),
        hardness: f32::INFINITY,
        needs_tool: false,
    },
    b(
        "coal_ore",
        t(TILE_COAL_ORE, TILE_COAL_ORE, TILE_COAL_ORE),
        3.0,
        true,
    ),
    b(
        "iron_ore",
        t(TILE_IRON_ORE, TILE_IRON_ORE, TILE_IRON_ORE),
        3.0,
        true,
    ),
];

/// Number of registered blocks (valid ids are `0..BLOCK_COUNT`).
pub const BLOCK_COUNT: u16 = BLOCKS.len() as u16;

/// Validate a decoded block id (protocol safety).
pub fn valid_block_id(id: BlockId) -> bool {
    (id as usize) < BLOCKS.len()
}

/// Static info for a valid block id. Panics on invalid ids — always validate
/// wire input with [`valid_id`] first.
pub fn block_info(id: BlockId) -> &'static BlockInfo {
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
