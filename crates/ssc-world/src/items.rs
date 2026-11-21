//! Item registry.
//!
//! Items are what inventories hold. Every block has a 1:1 block-item
//! (item id mirrors the block id); non-block items (food, later tools)
//! start at 100. No behavior here — the server owns all logic.

use super::blocks::{
    BlockId, AIR, BEDROCK, BRICKS, COAL_ORE, COBBLED_DEEPSLATE, COBBLESTONE, DEEPSLATE,
    DEEPSLATE_IRON_ORE, DIRT, GRASS_BLOCK, GRAVEL, IRON_ORE, OAK_LEAVES, OAK_LOG, OAK_PLANKS, SAND,
    SANDSTONE, SNOW_BLOCK, STONE, WATER,
};

pub type ItemId = u16;

/// Item id 0 = empty/none.
pub const EMPTY_ITEM: ItemId = 0;
/// Block-items mirror block ids 1..BLOCK_COUNT 1:1.
pub const FIRST_BLOCK_ITEM: ItemId = 1;
/// First non-block item id.
pub const FIRST_FREE_ITEM: ItemId = 100;

/// Apple: the survival food source (drops from oak leaves).
pub const APPLE: ItemId = 100;
/// Raw porkchop dropped by pigs. Cooking requires a furnace, not yet
/// implemented. It is edible raw (3 hunger points).
pub const PORKCHOP: ItemId = 101;
/// Rotten flesh dropped by zombies.
pub const ROTTEN_FLESH: ItemId = 102;
pub const COAL: ItemId = 103;
pub const RAW_IRON: ItemId = 104;
pub const STICK: ItemId = 105;
pub const WOODEN_PICKAXE: ItemId = 106;
pub const STONE_PICKAXE: ItemId = 107;
pub const FLINT: ItemId = 108;

/// Static item description. No behavior — behavior lives in the server.
pub struct ItemInfo {
    pub name: &'static str,
    /// Max stack size (Minecraft: 64 for most, 1 for tools).
    pub max_stack: u8,
    /// Placing this item places that block.
    pub places: Option<BlockId>,
    /// Hunger points restored when eaten (0..20 scale).
    pub food: Option<u8>,
}

fn block_item(id: BlockId, name: &'static str) -> ItemInfo {
    ItemInfo {
        name,
        max_stack: 64,
        places: Some(id),
        food: None,
    }
}

/// The item registry. Block-items are indexed 1..BLOCK_COUNT; the rest
/// by their explicit ids.
pub fn item_info(id: ItemId) -> ItemInfo {
    match id {
        0 => ItemInfo {
            name: "empty",
            max_stack: 0,
            places: None,
            food: None,
        },
        1 => block_item(BEDROCK, "bedrock"),
        2 => block_item(STONE, "stone"),
        3 => block_item(COBBLESTONE, "cobblestone"),
        4 => block_item(DIRT, "dirt"),
        5 => block_item(GRASS_BLOCK, "grass_block"),
        6 => block_item(SAND, "sand"),
        7 => block_item(SANDSTONE, "sandstone"),
        8 => block_item(GRAVEL, "gravel"),
        9 => block_item(OAK_LOG, "oak_log"),
        10 => block_item(OAK_PLANKS, "oak_planks"),
        11 => block_item(OAK_LEAVES, "oak_leaves"),
        12 => block_item(SNOW_BLOCK, "snow_block"),
        13 => block_item(BRICKS, "bricks"),
        14 => block_item(WATER, "water"),
        15 => block_item(COAL_ORE, "coal_ore"),
        16 => block_item(IRON_ORE, "iron_ore"),
        17 => block_item(DEEPSLATE, "deepslate"),
        18 => block_item(COBBLED_DEEPSLATE, "cobbled_deepslate"),
        19 => block_item(DEEPSLATE_IRON_ORE, "deepslate_iron_ore"),
        APPLE => ItemInfo {
            name: "apple",
            max_stack: 64,
            places: None,
            food: Some(4), // wiki: apple restores 4 hunger points
        },
        PORKCHOP => ItemInfo {
            name: "porkchop",
            max_stack: 64,
            places: None,
            food: Some(3), // raw porkchop restores 3 hunger points
        },
        ROTTEN_FLESH => ItemInfo {
            name: "rotten_flesh",
            max_stack: 64,
            places: None,
            food: Some(4), // Hunger effect and its chance are handled server-side.
        },
        COAL => ItemInfo {
            name: "coal",
            max_stack: 64,
            places: None,
            food: None,
        },
        RAW_IRON => ItemInfo {
            name: "raw_iron",
            max_stack: 64,
            places: None,
            food: None,
        },
        STICK => ItemInfo {
            name: "stick",
            max_stack: 64,
            places: None,
            food: None,
        },
        WOODEN_PICKAXE => ItemInfo {
            name: "wooden_pickaxe",
            max_stack: 1,
            places: None,
            food: None,
        },
        STONE_PICKAXE => ItemInfo {
            name: "stone_pickaxe",
            max_stack: 1,
            places: None,
            food: None,
        },
        FLINT => ItemInfo { name: "flint", max_stack: 64, places: None, food: None },
        _ => ItemInfo {
            name: "unknown",
            max_stack: 64,
            places: None,
            food: None,
        },
    }
}

/// Validate a decoded item id (protocol safety).
pub fn valid_item_id(id: ItemId) -> bool {
    // Block-items: contiguous. Free items: only the ones we defined.
    let block_items = id >= FIRST_BLOCK_ITEM && id < super::BLOCK_COUNT;
    let free_items = matches!(
        id,
        APPLE | PORKCHOP | ROTTEN_FLESH | COAL | RAW_IRON | STICK | WOODEN_PICKAXE | STONE_PICKAXE | FLINT
    );
    id == EMPTY_ITEM || block_items || free_items
}

/// The item form of a block.
pub fn for_block(b: BlockId) -> ItemId {
    if b == AIR {
        EMPTY_ITEM
    } else {
        b
    }
}

/// What breaking a block drops (item form), before randomness.
/// Simplified mapping: stone→cobblestone, grass→dirt, bedrock/water →
/// nothing. Leaves drop nothing directly (the apple chance is
/// server-side randomness, ~5% — higher than vanilla so food is
/// obtainable without saplings/sticks; noted in
/// implemented-features.md).
pub fn block_drop(b: BlockId) -> Option<ItemId> {
    match b {
        AIR | BEDROCK | WATER => None,
        STONE => Some(for_block(COBBLESTONE)),
        DEEPSLATE => Some(for_block(COBBLED_DEEPSLATE)),
        COAL_ORE => Some(COAL),
        IRON_ORE | DEEPSLATE_IRON_ORE => Some(RAW_IRON),
        GRASS_BLOCK => Some(for_block(DIRT)),
        OAK_LEAVES => None,
        other => Some(for_block(other)),
    }
}

/// Chance an oak leaf breaks into an apple.
pub const LEAF_APPLE_CHANCE: f32 = 0.05;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn food_items_have_expected_values_and_are_wire_valid() {
        for (item, hunger) in [(APPLE, 4), (PORKCHOP, 3), (ROTTEN_FLESH, 4)] {
            assert_eq!(item_info(item).food, Some(hunger));
            assert!(valid_item_id(item));
        }
    }

    #[test]
    fn ore_drops_are_resources_not_placeable_ore_blocks() {
        for (ore, resource) in [
            (COAL_ORE, COAL),
            (IRON_ORE, RAW_IRON),
            (DEEPSLATE_IRON_ORE, RAW_IRON),
        ] {
            assert!(valid_item_id(ore));
            assert!(valid_item_id(resource));
            assert_eq!(block_drop(ore), Some(resource));
            assert_eq!(item_info(resource).places, None);
        }
        assert_eq!(block_drop(DEEPSLATE), Some(COBBLED_DEEPSLATE));
    }
}
