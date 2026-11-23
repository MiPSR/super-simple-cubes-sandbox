//! Item registry.
//!
//! Items are what inventories hold. Every block has a 1:1 block-item
//! (item id mirrors the block id); non-block items (food, later tools)
//! start at 100. No behavior here — the server owns all logic.

use super::blocks::{
    BlockId, AIR, BEDROCK, BRICKS, COAL_ORE, COBBLED_DEEPSLATE, COBBLESTONE, COPPER_ORE, DEEPSLATE,
    DEEPSLATE_COAL_ORE, DEEPSLATE_COPPER_ORE, DEEPSLATE_IRON_ORE, DIRT, GRASS_BLOCK, GRAVEL,
    IRON_ORE, OAK_LEAVES, OAK_LOG, OAK_PLANKS, SAND, SANDSTONE, SNOW_BLOCK, STONE,
    STRIPPED_OAK_LOG, WATER,
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
/// Raw porkchop dropped by pigs; edible raw or cooked in a furnace.
pub const PORKCHOP: ItemId = 101;
/// Rotten flesh dropped by zombies.
pub const ROTTEN_FLESH: ItemId = 102;
pub const COAL: ItemId = 103;
pub const RAW_IRON: ItemId = 104;
pub const STICK: ItemId = 105;
pub const WOODEN_PICKAXE: ItemId = 106;
pub const STONE_PICKAXE: ItemId = 107;
pub const FLINT: ItemId = 108;
pub const RAW_COPPER: ItemId = 109;
pub const WOODEN_AXE: ItemId = 110;
pub const STONE_AXE: ItemId = 111;
pub const WOODEN_SHOVEL: ItemId = 112;
pub const STONE_SHOVEL: ItemId = 113;
pub const SNOWBALL: ItemId = 114;
pub const IRON_INGOT: ItemId = 115;
pub const COPPER_INGOT: ItemId = 116;
pub const COOKED_PORKCHOP: ItemId = 117;
pub const CHARCOAL: ItemId = 118;
pub const IRON_PICKAXE: ItemId = 119;
pub const IRON_AXE: ItemId = 120;
pub const IRON_SHOVEL: ItemId = 121;
pub const WOODEN_SWORD: ItemId = 122;
pub const STONE_SWORD: ItemId = 123;
pub const IRON_SWORD: ItemId = 124;
pub const BUCKET: ItemId = 125;
pub const WATER_BUCKET: ItemId = 126;
pub const WOODEN_HOE: ItemId = 127;
pub const STONE_HOE: ItemId = 128;
pub const IRON_HOE: ItemId = 129;
pub const WHEAT_SEEDS: ItemId = 130;
pub const WHEAT: ItemId = 131;
pub const BREAD: ItemId = 132;
pub const MUTTON: ItemId = 133;
pub const COOKED_MUTTON: ItemId = 134;

pub fn is_hoe(item: ItemId) -> bool {
    matches!(item, WOODEN_HOE | STONE_HOE | IRON_HOE)
}

pub fn is_sword(item: ItemId) -> bool {
    matches!(item, WOODEN_SWORD | STONE_SWORD | IRON_SWORD)
}

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
        BUCKET | WATER_BUCKET => ItemInfo {
            name: if id == BUCKET {
                "bucket"
            } else {
                "water_bucket"
            },
            max_stack: if id == BUCKET { 16 } else { 1 },
            places: None,
            food: None,
        },
        WOODEN_SWORD | STONE_SWORD | IRON_SWORD => ItemInfo {
            name: match id {
                WOODEN_SWORD => "wooden_sword",
                STONE_SWORD => "stone_sword",
                _ => "iron_sword",
            },
            max_stack: 1,
            places: None,
            food: None,
        },
        IRON_PICKAXE | IRON_AXE | IRON_SHOVEL => ItemInfo {
            name: match id {
                IRON_PICKAXE => "iron_pickaxe",
                IRON_AXE => "iron_axe",
                _ => "iron_shovel",
            },
            max_stack: 1,
            places: None,
            food: None,
        },
        WOODEN_HOE | STONE_HOE | IRON_HOE => ItemInfo {
            name: match id {
                WOODEN_HOE => "wooden_hoe",
                STONE_HOE => "stone_hoe",
                _ => "iron_hoe",
            },
            max_stack: 1,
            places: None,
            food: None,
        },
        WHEAT_SEEDS => ItemInfo {
            name: "wheat_seeds",
            max_stack: 64,
            places: Some(super::WHEAT_0),
            food: None,
        },
        WHEAT => ItemInfo {
            name: "wheat",
            max_stack: 64,
            places: None,
            food: None,
        },
        BREAD => ItemInfo {
            name: "bread",
            max_stack: 64,
            places: None,
            food: Some(5),
        },
        MUTTON => ItemInfo {
            name: "mutton",
            max_stack: 64,
            places: None,
            food: Some(2),
        },
        COOKED_MUTTON => ItemInfo {
            name: "cooked_mutton",
            max_stack: 64,
            places: None,
            food: Some(6),
        },
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
        20 => block_item(DEEPSLATE_COAL_ORE, "deepslate_coal_ore"),
        21 => block_item(COPPER_ORE, "copper_ore"),
        22 => block_item(DEEPSLATE_COPPER_ORE, "deepslate_copper_ore"),
        23 => block_item(STRIPPED_OAK_LOG, "stripped_oak_log"),
        24 | 25 => block_item(super::OAK_SAPLING, "oak_sapling"),
        26 => block_item(OAK_LEAVES, "oak_leaves"),
        27 | 28 => block_item(super::FURNACE, "furnace"),
        29 => block_item(super::GLASS, "glass"),
        30 => block_item(super::SMOOTH_STONE, "smooth_stone"),
        31 | 32 => block_item(super::FARMLAND, "farmland"),
        33..=40 => block_item(super::WHEAT_0, "wheat"),
        41 => block_item(super::WOOL, "white_wool"),
        42 => block_item(super::BED, "bed"),
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
        FLINT => ItemInfo {
            name: "flint",
            max_stack: 64,
            places: None,
            food: None,
        },
        RAW_COPPER => ItemInfo {
            name: "raw_copper",
            max_stack: 64,
            places: None,
            food: None,
        },
        WOODEN_AXE => ItemInfo {
            name: "wooden_axe",
            max_stack: 1,
            places: None,
            food: None,
        },
        STONE_AXE => ItemInfo {
            name: "stone_axe",
            max_stack: 1,
            places: None,
            food: None,
        },
        WOODEN_SHOVEL => ItemInfo {
            name: "wooden_shovel",
            max_stack: 1,
            places: None,
            food: None,
        },
        STONE_SHOVEL => ItemInfo {
            name: "stone_shovel",
            max_stack: 1,
            places: None,
            food: None,
        },
        SNOWBALL => ItemInfo {
            name: "snowball",
            max_stack: 16,
            places: None,
            food: None,
        },
        IRON_INGOT => ItemInfo {
            name: "iron_ingot",
            max_stack: 64,
            places: None,
            food: None,
        },
        COPPER_INGOT => ItemInfo {
            name: "copper_ingot",
            max_stack: 64,
            places: None,
            food: None,
        },
        COOKED_PORKCHOP => ItemInfo {
            name: "cooked_porkchop",
            max_stack: 64,
            places: None,
            food: Some(8),
        },
        CHARCOAL => ItemInfo {
            name: "charcoal",
            max_stack: 64,
            places: None,
            food: None,
        },
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
        APPLE
            | PORKCHOP
            | ROTTEN_FLESH
            | COAL
            | RAW_IRON
            | STICK
            | WOODEN_PICKAXE
            | STONE_PICKAXE
            | FLINT
            | RAW_COPPER
            | WOODEN_AXE
            | STONE_AXE
            | WOODEN_SHOVEL
            | STONE_SHOVEL
            | SNOWBALL
            | IRON_INGOT
            | COPPER_INGOT
            | COOKED_PORKCHOP
            | CHARCOAL
            | IRON_PICKAXE
            | IRON_AXE
            | IRON_SHOVEL
            | WOODEN_SWORD
            | STONE_SWORD
            | IRON_SWORD
            | BUCKET
            | WATER_BUCKET
            | WOODEN_HOE
            | STONE_HOE
            | IRON_HOE
            | WHEAT_SEEDS
            | WHEAT
            | BREAD
            | MUTTON
            | COOKED_MUTTON
    );
    id == EMPTY_ITEM || block_items || free_items
}

/// Saturation restored by the foods currently in the registry (Bedrock).
pub fn food_saturation(id: ItemId) -> f32 {
    match id {
        APPLE => 2.4,
        PORKCHOP => 1.8,
        ROTTEN_FLESH => 0.8,
        COOKED_PORKCHOP => 12.8,
        BREAD => 6.0,
        MUTTON => 1.2,
        COOKED_MUTTON => 9.6,
        _ => 0.0,
    }
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
        COAL_ORE | DEEPSLATE_COAL_ORE => Some(COAL),
        IRON_ORE | DEEPSLATE_IRON_ORE => Some(RAW_IRON),
        COPPER_ORE | DEEPSLATE_COPPER_ORE => Some(RAW_COPPER),
        GRASS_BLOCK => Some(for_block(DIRT)),
        OAK_LEAVES | super::GROWN_OAK_LEAVES => None,
        super::OAK_SAPLING | super::OAK_SAPLING_READY => Some(super::OAK_SAPLING),
        super::FURNACE | super::LIT_FURNACE => Some(super::FURNACE),
        super::GLASS => None,
        super::FARMLAND | super::FARMLAND_WET => Some(for_block(DIRT)),
        super::WHEAT_0..=super::WHEAT_7 => None,
        SNOW_BLOCK => Some(SNOWBALL),
        other => Some(for_block(other)),
    }
}

/// Chance an oak leaf breaks into an apple.
pub const LEAF_APPLE_CHANCE: f32 = 0.05;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_foods_restore_wiki_saturation() {
        assert_eq!(food_saturation(APPLE), 2.4);
        assert_eq!(food_saturation(PORKCHOP), 1.8);
        assert_eq!(food_saturation(ROTTEN_FLESH), 0.8);
        assert_eq!(food_saturation(STONE_PICKAXE), 0.0);
    }

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
            (DEEPSLATE_COAL_ORE, COAL),
            (IRON_ORE, RAW_IRON),
            (DEEPSLATE_IRON_ORE, RAW_IRON),
            (COPPER_ORE, RAW_COPPER),
            (DEEPSLATE_COPPER_ORE, RAW_COPPER),
        ] {
            assert!(valid_item_id(ore));
            assert!(valid_item_id(resource));
            assert_eq!(block_drop(ore), Some(resource));
            assert_eq!(item_info(resource).places, None);
        }
        assert_eq!(block_drop(DEEPSLATE), Some(COBBLED_DEEPSLATE));
    }
}
