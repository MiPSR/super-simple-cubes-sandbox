//! Small, versioned save format for the world seed and block edits.
//!
//! Terrain is regenerated from the seed, so saves contain only explicit
//! block overrides rather than whole generated columns.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use ssc_world::{BlockId, ColumnKey, MAX_Y, MIN_Y};

pub type BlockEdits = HashMap<ColumnKey, HashMap<(i32, i32, i32), BlockId>>;

const MAGIC_V1: &[u8; 8] = b"SSCWLD01";
const MAGIC: &[u8; 8] = b"SSCWLD02";
const HEADER_V1_SIZE: u64 = 20;
const HEADER_SIZE: u64 = 24;
const RECORD_SIZE: u64 = 14;
const MAX_EDITS: u32 = 4_000_000;
const PLAYER_MAGIC: &[u8; 8] = b"SSCPLY01";
const PLAYER_MAGIC_V2: &[u8; 8] = b"SSCPLY02";
const PLAYER_MAGIC_V3: &[u8; 8] = b"SSCPLY03";
const PLAYER_MAGIC_V4: &[u8; 8] = b"SSCPLY04";
const PLAYER_MAGIC_V5: &[u8; 8] = b"SSCPLY05";
const ITEMS_MAGIC: &[u8; 8] = b"SSCITM01";
const ITEMS_MAGIC_V2: &[u8; 8] = b"SSCITM02";
const MOBS_MAGIC: &[u8; 8] = b"SSCMOB01";
const MOBS_MAGIC_V2: &[u8; 8] = b"SSCMOB02";
const FURNACE_MAGIC: &[u8; 8] = b"SSCFUR01";
const MAX_SAVED_ITEMS: u32 = 1_000_000;
const MAX_SAVED_MOBS: u32 = 4_096;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SavedItem {
    pub item: ssc_world::ItemId,
    pub count: u8,
    pub durability: u16,
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub age: u32,
    pub pickup_delay: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SavedMob {
    pub kind: u8,
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub yaw: f32,
    pub health: f32,
    pub wander_timer: u32,
    pub wander_dir: Option<[f32; 2]>,
    pub attack_cooldown: u32,
    pub burn_timer: u8,
    pub panic_timer: u8,
    pub on_ground: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSave {
    pub inventory: Vec<crate::survival::Slot>,
    /// Remaining uses per slot (zero for non-tools or old full-wear tools).
    pub durability: Vec<u16>,
    pub position: Option<[f32; 3]>,
    pub health: f32,
    pub hunger: u8,
    pub saturation: f32,
    pub air: u16,
    pub selected_slot: u8,
    pub hunger_effect_ticks: u16,
    pub exhaustion: f32,
}

impl Default for PlayerSave {
    fn default() -> Self {
        Self {
            inventory: vec![None; crate::survival::INVENTORY_SIZE],
            durability: vec![0; crate::survival::INVENTORY_SIZE],
            position: None,
            health: crate::survival::MAX_HEALTH,
            hunger: crate::survival::MAX_HUNGER,
            saturation: crate::survival::INITIAL_SATURATION,
            air: crate::survival::MAX_AIR,
            selected_slot: 0,
            hunger_effect_ticks: 0,
            exhaustion: 0.0,
        }
    }
}

/// Load a save. `Ok(None)` means no save exists.
pub fn load(path: &Path) -> io::Result<Option<(u64, f32, BlockEdits)>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let file_len = file.metadata()?.len();
    if file_len < HEADER_V1_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "save header is truncated",
        ));
    }

    let mut prefix = [0u8; HEADER_V1_SIZE as usize];
    file.read_exact(&mut prefix)?;
    let (seed, time_of_day, count, header_size) = if &prefix[..8] == MAGIC {
        let seed = u64::from_le_bytes(prefix[8..16].try_into().expect("fixed header slice"));
        let time_of_day =
            f32::from_le_bytes(prefix[16..20].try_into().expect("fixed header slice"));
        let mut count_bytes = [0u8; 4];
        file.read_exact(&mut count_bytes)?;
        let count = u32::from_le_bytes(count_bytes);
        if !time_of_day.is_finite() || !(0.0..1.0).contains(&time_of_day) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid world time in save",
            ));
        }
        (seed, time_of_day, count, HEADER_SIZE)
    } else if &prefix[..8] == MAGIC_V1 {
        let seed = u64::from_le_bytes(prefix[8..16].try_into().expect("fixed header slice"));
        let count = u32::from_le_bytes(prefix[16..20].try_into().expect("fixed header slice"));
        // Version 1 had no stored clock; retain its original start time.
        (seed, 0.3, count, HEADER_V1_SIZE)
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad world save magic",
        ));
    };
    if count > MAX_EDITS || file_len != header_size + count as u64 * RECORD_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid world save length",
        ));
    }
    let mut edits = BlockEdits::new();
    for _ in 0..count {
        let mut record = [0u8; RECORD_SIZE as usize];
        file.read_exact(&mut record)?;
        let x = i32::from_le_bytes(record[0..4].try_into().expect("fixed record slice"));
        let y = i32::from_le_bytes(record[4..8].try_into().expect("fixed record slice"));
        let z = i32::from_le_bytes(record[8..12].try_into().expect("fixed record slice"));
        let block = u16::from_le_bytes(record[12..14].try_into().expect("fixed record slice"));
        if !(MIN_Y..MAX_Y).contains(&y) || !ssc_world::valid_block_id(block) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid block edit in world save",
            ));
        }
        let key = (x.div_euclid(16), z.div_euclid(16));
        let local = (x.rem_euclid(16), y, z.rem_euclid(16));
        edits.entry(key).or_default().insert(local, block);
    }
    Ok(Some((seed, time_of_day, edits)))
}

/// Atomically save the seed and explicit block edits.
pub fn save(path: &Path, seed: u64, time_of_day: f32, edits: &BlockEdits) -> io::Result<()> {
    if !time_of_day.is_finite() || !(0.0..1.0).contains(&time_of_day) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "world time must be finite and in 0..1",
        ));
    }
    let mut records = Vec::new();
    for (&(cx, cz), column) in edits {
        for (&(lx, y, lz), &block) in column {
            records.push((cx * 16 + lx, y, cz * 16 + lz, block));
        }
    }
    if records.len() > MAX_EDITS as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many world edits",
        ));
    }
    records.sort_unstable_by_key(|r| (r.0, r.1, r.2));

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("tmp");
    let mut file = File::create(&temp_path)?;
    file.write_all(MAGIC)?;
    file.write_all(&seed.to_le_bytes())?;
    file.write_all(&time_of_day.to_le_bytes())?;
    file.write_all(&(records.len() as u32).to_le_bytes())?;
    for (x, y, z, block) in records {
        file.write_all(&x.to_le_bytes())?;
        file.write_all(&y.to_le_bytes())?;
        file.write_all(&z.to_le_bytes())?;
        file.write_all(&block.to_le_bytes())?;
    }
    file.sync_all()?;
    fs::rename(temp_path, path)?;
    Ok(())
}

/// Path for one anonymous player's save. The UUID is encoded as lowercase
/// hex, so client input cannot escape the player-data directory.
pub fn player_path(world_path: &Path, player_id: &[u8; 16]) -> std::path::PathBuf {
    let mut hex = String::with_capacity(32);
    for byte in player_id {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    world_path
        .with_extension("players")
        .join(format!("{hex}.dat"))
}

/// Load a persisted 36-slot inventory. Missing player data is not an error.
pub fn load_player(path: &Path, player_id: &[u8; 16]) -> io::Result<Option<PlayerSave>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let file_len = file.metadata()?.len();
    let v1_len = 8 + 16 + crate::survival::INVENTORY_SIZE * 3;
    let v2_len = 8 + 16 + 20 + crate::survival::INVENTORY_SIZE * 3;
    let v3_len = v2_len + 6;
    let v4_len = v3_len + crate::survival::INVENTORY_SIZE * 2;
    let v5_len = v4_len + 4;
    if file_len != v1_len as u64
        && file_len != v2_len as u64
        && file_len != v3_len as u64
        && file_len != v4_len as u64
        && file_len != v5_len as u64
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid player save length",
        ));
    }
    let expected_len = file_len as usize;
    let mut bytes = Vec::with_capacity(expected_len);
    file.read_to_end(&mut bytes)?;
    if bytes.len() != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "player save changed while reading",
        ));
    }
    let v2 = &bytes[..8] == PLAYER_MAGIC_V2;
    let v3 = &bytes[..8] == PLAYER_MAGIC_V3;
    let v4 = &bytes[..8] == PLAYER_MAGIC_V4;
    let v5 = &bytes[..8] == PLAYER_MAGIC_V5;
    if (!v2 && !v3 && !v4 && !v5 && &bytes[..8] != PLAYER_MAGIC) || &bytes[8..24] != player_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "player save identity or format mismatch",
        ));
    }
    if (v2 && expected_len != v2_len)
        || (v3 && expected_len != v3_len)
        || (v4 && expected_len != v4_len)
        || (v5 && expected_len != v5_len)
        || (!v2 && !v3 && !v4 && !v5 && expected_len != v1_len)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "player save version and length mismatch",
        ));
    }
    let mut data = PlayerSave::default();
    data.inventory.clear();
    let inventory_offset = if v2 || v3 || v4 || v5 {
        let position = [
            f32::from_le_bytes(bytes[24..28].try_into().expect("fixed save slice")),
            f32::from_le_bytes(bytes[28..32].try_into().expect("fixed save slice")),
            f32::from_le_bytes(bytes[32..36].try_into().expect("fixed save slice")),
        ];
        let health = f32::from_le_bytes(bytes[36..40].try_into().expect("fixed save slice"));
        let hunger = bytes[40];
        let air = u16::from_le_bytes(bytes[41..43].try_into().expect("fixed save slice"));
        let selected_slot = bytes[43];
        if position.iter().any(|v| !v.is_finite())
            || position[1] < ssc_world::MIN_Y as f32
            || position[1] >= ssc_world::MAX_Y as f32 - ssc_math::PLAYER_HEIGHT
            || position[0].abs() > 30_000_000.0
            || position[2].abs() > 30_000_000.0
            || !health.is_finite()
            || !(0.0..=crate::survival::MAX_HEALTH).contains(&health)
            || hunger > crate::survival::MAX_HUNGER
            || air > crate::survival::MAX_AIR
            || selected_slot >= 9
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid player state in save",
            ));
        }
        data.position = Some(position);
        data.health = health;
        data.hunger = hunger;
        data.air = air;
        data.selected_slot = selected_slot;
        if v3 || v4 || v5 {
            data.hunger_effect_ticks =
                u16::from_le_bytes(bytes[44..46].try_into().expect("fixed save slice"));
            data.exhaustion =
                f32::from_le_bytes(bytes[46..50].try_into().expect("fixed save slice"));
            if data.hunger_effect_ticks > 600
                || !data.exhaustion.is_finite()
                || !(0.0..=10.0).contains(&data.exhaustion)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid player effects",
                ));
            }
            50
        } else {
            44
        }
    } else {
        24
    };
    let inventory_end = inventory_offset + crate::survival::INVENTORY_SIZE * 3;
    for slot in bytes[inventory_offset..inventory_end].chunks_exact(3) {
        let item = u16::from_le_bytes([slot[0], slot[1]]);
        let count = slot[2];
        if item == 0 && count == 0 {
            data.inventory.push(None);
            continue;
        }
        if item == 0 || count == 0 || !ssc_world::valid_item_id(item) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid player inventory slot",
            ));
        }
        if count > ssc_world::item_info(item).max_stack {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "player inventory stack exceeds max size",
            ));
        }
        data.inventory.push(Some((item, count)));
    }
    if v4 || v5 {
        let wear_end = inventory_end + crate::survival::INVENTORY_SIZE * 2;
        for (i, slot) in bytes[inventory_end..wear_end].chunks_exact(2).enumerate() {
            let wear = u16::from_le_bytes([slot[0], slot[1]]);
            let max =
                data.inventory[i].map_or(0, |(item, _)| crate::survival::max_durability(item));
            if (max == 0 && wear != 0) || (max > 0 && (wear == 0 || wear > max)) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid tool durability",
                ));
            }
            data.durability[i] = wear;
        }
    } else {
        for (slot, wear) in data.inventory.iter().zip(&mut data.durability) {
            *wear = slot.map_or(0, |(item, _)| crate::survival::max_durability(item));
        }
    }
    if v5 {
        data.saturation =
            f32::from_le_bytes(bytes[v4_len..v5_len].try_into().expect("fixed save slice"));
        if !data.saturation.is_finite() || !(0.0..=data.hunger as f32).contains(&data.saturation) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid player saturation",
            ));
        }
    } else {
        // Older saves had no saturation; preserve their visible hunger rather
        // than granting a new buffer on migration.
        data.saturation = 0.0;
    }
    Ok(Some(data))
}

/// Atomically write a player's inventory.
pub fn save_player(path: &Path, player_id: &[u8; 16], data: &PlayerSave) -> io::Result<()> {
    if data.inventory.len() != crate::survival::INVENTORY_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "player inventory has wrong slot count",
        ));
    }
    if data.durability.len() != crate::survival::INVENTORY_SIZE
        || data
            .inventory
            .iter()
            .zip(&data.durability)
            .any(|(slot, wear)| {
                let max = slot.map_or(0, |(item, _)| crate::survival::max_durability(item));
                (max == 0 && *wear != 0) || (max > 0 && *wear > max)
            })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid player tool durability",
        ));
    }
    if data.inventory.iter().flatten().any(|(item, count)| {
        !ssc_world::valid_item_id(*item)
            || *count == 0
            || *count > ssc_world::item_info(*item).max_stack
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid item stack in player inventory",
        ));
    }
    let Some(position) = data.position else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "player save needs a position",
        ));
    };
    if position.iter().any(|v| !v.is_finite())
        || position[1] < ssc_world::MIN_Y as f32
        || position[1] >= ssc_world::MAX_Y as f32 - ssc_math::PLAYER_HEIGHT
        || position[0].abs() > 30_000_000.0
        || position[2].abs() > 30_000_000.0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid player position",
        ));
    }
    if !data.health.is_finite()
        || !(0.0..=crate::survival::MAX_HEALTH).contains(&data.health)
        || data.hunger > crate::survival::MAX_HUNGER
        || !data.saturation.is_finite()
        || !(0.0..=data.hunger as f32).contains(&data.saturation)
        || data.air > crate::survival::MAX_AIR
        || data.selected_slot >= 9
        || data.hunger_effect_ticks > 600
        || !data.exhaustion.is_finite()
        || !(0.0..=10.0).contains(&data.exhaustion)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid player vitals",
        ));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("tmp");
    let mut file = File::create(&temp_path)?;
    file.write_all(PLAYER_MAGIC_V5)?;
    file.write_all(player_id)?;
    for coordinate in position {
        file.write_all(&coordinate.to_le_bytes())?;
    }
    file.write_all(&data.health.to_le_bytes())?;
    file.write_all(&[data.hunger])?;
    file.write_all(&data.air.to_le_bytes())?;
    file.write_all(&[data.selected_slot])?;
    file.write_all(&data.hunger_effect_ticks.to_le_bytes())?;
    file.write_all(&data.exhaustion.to_le_bytes())?;
    for slot in &data.inventory {
        if let Some((item, count)) = slot {
            file.write_all(&item.to_le_bytes())?;
            file.write_all(&[*count])?;
        } else {
            file.write_all(&[0, 0, 0])?;
        }
    }
    for (slot, &wear) in data.inventory.iter().zip(&data.durability) {
        let value = slot.map_or(0, |(item, _)| {
            crate::survival::effective_durability(item, wear)
        });
        file.write_all(&value.to_le_bytes())?;
    }
    file.write_all(&data.saturation.to_le_bytes())?;
    file.sync_all()?;
    fs::rename(temp_path, path)?;
    Ok(())
}

pub fn item_save_path(world_path: &Path) -> std::path::PathBuf {
    world_path.with_extension("items")
}

pub fn load_items(path: &Path) -> io::Result<Option<Vec<SavedItem>>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    const HEADER: u64 = 12;
    let file_len = file.metadata()?.len();
    if file_len < HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated item save",
        ));
    }
    let mut header = [0u8; HEADER as usize];
    file.read_exact(&mut header)?;
    let v2 = &header[..8] == ITEMS_MAGIC_V2;
    if !v2 && &header[..8] != ITEMS_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid item save magic",
        ));
    }
    let count = u32::from_le_bytes(header[8..12].try_into().expect("fixed item header"));
    let record_len = if v2 { 35 } else { 33 };
    if count > MAX_SAVED_ITEMS || file_len != HEADER + count as u64 * record_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid item save length",
        ));
    }
    let mut items = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut record = [0u8; 35];
        file.read_exact(&mut record[..record_len as usize])?;
        let item = u16::from_le_bytes([record[0], record[1]]);
        let amount = record[2];
        let mut floats = [0.0f32; 6];
        for (i, bytes) in record[3..27].chunks_exact(4).enumerate() {
            floats[i] = f32::from_le_bytes(bytes.try_into().expect("fixed item float"));
        }
        let age = u32::from_le_bytes(record[27..31].try_into().expect("fixed item age"));
        let pickup_delay = record[31];
        let max = crate::survival::max_durability(item);
        let durability = if v2 {
            u16::from_le_bytes([record[33], record[34]])
        } else {
            max
        };
        // Byte 32 is reserved for forward-compatible item flags.
        if record[32] != 0
            || !ssc_world::valid_item_id(item)
            || item == 0
            || amount == 0
            || amount > ssc_world::item_info(item).max_stack
            || floats.iter().any(|v| !v.is_finite())
            || floats[0].abs() > 30_000_000.0
            || floats[2].abs() > 30_000_000.0
            || !(ssc_world::MIN_Y as f32 - 1.0..ssc_world::MAX_Y as f32 + 1.0).contains(&floats[1])
            || age >= 6_000
            || (max == 0 && durability != 0)
            || (max > 0 && (durability == 0 || durability > max))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid dropped item in save",
            ));
        }
        items.push(SavedItem {
            item,
            count: amount,
            durability,
            pos: [floats[0], floats[1], floats[2]],
            vel: [floats[3], floats[4], floats[5]],
            age,
            pickup_delay,
        });
    }
    Ok(Some(items))
}

pub fn save_items(path: &Path, items: &[SavedItem]) -> io::Result<()> {
    if items.len() > MAX_SAVED_ITEMS as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many dropped items",
        ));
    }
    if items.iter().any(|entry| {
        !ssc_world::valid_item_id(entry.item)
            || entry.item == 0
            || entry.count == 0
            || entry.count > ssc_world::item_info(entry.item).max_stack
            || entry
                .pos
                .iter()
                .chain(entry.vel.iter())
                .any(|v| !v.is_finite())
            || entry.pos[0].abs() > 30_000_000.0
            || entry.pos[2].abs() > 30_000_000.0
            || !(ssc_world::MIN_Y as f32 - 1.0..ssc_world::MAX_Y as f32 + 1.0)
                .contains(&entry.pos[1])
            || entry.age >= 6_000
            || (crate::survival::max_durability(entry.item) == 0 && entry.durability != 0)
            || (crate::survival::max_durability(entry.item) > 0
                && entry.durability > crate::survival::max_durability(entry.item))
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid dropped item state",
        ));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("tmp");
    let mut file = File::create(&temp_path)?;
    file.write_all(ITEMS_MAGIC_V2)?;
    file.write_all(&(items.len() as u32).to_le_bytes())?;
    for entry in items {
        file.write_all(&entry.item.to_le_bytes())?;
        file.write_all(&[entry.count])?;
        for value in entry.pos.into_iter().chain(entry.vel) {
            file.write_all(&value.to_le_bytes())?;
        }
        file.write_all(&entry.age.to_le_bytes())?;
        file.write_all(&[entry.pickup_delay, 0])?;
        file.write_all(
            &crate::survival::effective_durability(entry.item, entry.durability).to_le_bytes(),
        )?;
    }
    file.sync_all()?;
    fs::rename(temp_path, path)?;
    Ok(())
}

pub fn mob_save_path(world_path: &Path) -> std::path::PathBuf {
    world_path.with_extension("mobs")
}

pub fn load_mobs(path: &Path) -> io::Result<Option<Vec<SavedMob>>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    const HEADER: u64 = 12;
    let file_len = file.metadata()?.len();
    if file_len < HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated mob save",
        ));
    }
    let mut header = [0u8; HEADER as usize];
    file.read_exact(&mut header)?;
    let v2 = &header[..8] == MOBS_MAGIC_V2;
    if !v2 && &header[..8] != MOBS_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid mob save magic",
        ));
    }
    let count = u32::from_le_bytes(header[8..12].try_into().expect("fixed mob header"));
    let record_len = if v2 { 52 } else { 51 };
    if count > MAX_SAVED_MOBS || file_len != HEADER + count as u64 * record_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid mob save length",
        ));
    }
    let mut mobs = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut record = [0u8; 52];
        file.read_exact(&mut record[..record_len as usize])?;
        let kind = record[0];
        let flags = record[1];
        let mut floats = [0.0f32; 8];
        for (i, bytes) in record[2..34].chunks_exact(4).enumerate() {
            floats[i] = f32::from_le_bytes(bytes.try_into().expect("fixed mob float"));
        }
        let wander_timer = u32::from_le_bytes(record[34..38].try_into().expect("fixed mob timer"));
        let attack_cooldown =
            u32::from_le_bytes(record[46..50].try_into().expect("fixed mob cooldown"));
        let burn_timer = record[50];
        let panic_timer = if v2 { record[51] } else { 0 };
        let max_health = match kind {
            1 => 10.0,
            2 => 20.0,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown mob kind",
                ));
            }
        };
        let pos = [floats[0], floats[1], floats[2]];
        let vel = [floats[3], floats[4], floats[5]];
        let yaw = floats[6];
        let health = floats[7];
        let wander_dir = if flags & 1 != 0 {
            Some([
                f32::from_le_bytes(record[38..42].try_into().expect("fixed mob direction")),
                f32::from_le_bytes(record[42..46].try_into().expect("fixed mob direction")),
            ])
        } else {
            None
        };
        let on_ground = flags & 2 != 0;
        if flags & !3 != 0
            || pos.iter().chain(vel.iter()).any(|v| !v.is_finite())
            || !yaw.is_finite()
            || !health.is_finite()
            || !(0.0 < health && health <= max_health)
            || pos[0].abs() > 30_000_000.0
            || pos[2].abs() > 30_000_000.0
            || pos[1] < ssc_world::MIN_Y as f32 - 1.0
            || pos[1] >= ssc_world::MAX_Y as f32
            || wander_timer > 120
            || attack_cooldown > 20
            || burn_timer >= 20
            || panic_timer > 60
            || (panic_timer > 0 && (kind != 1 || wander_dir.is_none()))
            || wander_dir.is_some_and(|d| d.iter().any(|v| !v.is_finite()))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid mob state",
            ));
        }
        mobs.push(SavedMob {
            kind,
            pos,
            vel,
            yaw,
            health,
            wander_timer,
            wander_dir,
            attack_cooldown,
            burn_timer,
            panic_timer,
            on_ground,
        });
    }
    Ok(Some(mobs))
}

pub fn save_mobs(path: &Path, mobs: &[SavedMob]) -> io::Result<()> {
    if mobs.len() > MAX_SAVED_MOBS as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many mobs"));
    }
    if mobs.iter().any(|mob| {
        let max_health = match mob.kind {
            1 => 10.0,
            2 => 20.0,
            _ => return true,
        };
        mob.pos.iter().chain(mob.vel.iter()).any(|v| !v.is_finite())
            || !mob.yaw.is_finite()
            || !mob.health.is_finite()
            || !(0.0 < mob.health && mob.health <= max_health)
            || mob.pos[0].abs() > 30_000_000.0
            || mob.pos[2].abs() > 30_000_000.0
            || mob.pos[1] < ssc_world::MIN_Y as f32 - 1.0
            || mob.pos[1] >= ssc_world::MAX_Y as f32
            || mob.wander_timer > 120
            || mob.attack_cooldown > 20
            || mob.burn_timer >= 20
            || mob.panic_timer > 60
            || (mob.panic_timer > 0 && (mob.kind != 1 || mob.wander_dir.is_none()))
            || mob
                .wander_dir
                .is_some_and(|d| d.iter().any(|v| !v.is_finite()))
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid mob state",
        ));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("tmp");
    let mut file = File::create(&temp_path)?;
    file.write_all(MOBS_MAGIC_V2)?;
    file.write_all(&(mobs.len() as u32).to_le_bytes())?;
    for mob in mobs {
        let flags = u8::from(mob.wander_dir.is_some()) | (u8::from(mob.on_ground) << 1);
        file.write_all(&[mob.kind, flags])?;
        for value in mob
            .pos
            .into_iter()
            .chain(mob.vel)
            .chain([mob.yaw, mob.health])
        {
            file.write_all(&value.to_le_bytes())?;
        }
        file.write_all(&mob.wander_timer.to_le_bytes())?;
        let direction = mob.wander_dir.unwrap_or([0.0, 0.0]);
        file.write_all(&direction[0].to_le_bytes())?;
        file.write_all(&direction[1].to_le_bytes())?;
        file.write_all(&mob.attack_cooldown.to_le_bytes())?;
        file.write_all(&[mob.burn_timer])?;
        file.write_all(&[mob.panic_timer])?;
    }
    file.sync_all()?;
    fs::rename(temp_path, path)?;
    Ok(())
}

pub type FurnaceSaves = HashMap<(i32, i32, i32), crate::furnace::Furnace>;

pub fn furnace_save_path(world_path: &Path) -> std::path::PathBuf {
    world_path.with_extension("furnaces")
}

fn valid_furnace_position((x, y, z): (i32, i32, i32)) -> bool {
    x.unsigned_abs() <= 30_000_000 && z.unsigned_abs() <= 30_000_000 && (MIN_Y..MAX_Y).contains(&y)
}

pub fn load_furnaces(path: &Path) -> io::Result<Option<FurnaceSaves>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let length = file.metadata()?.len();
    if length < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated furnace save",
        ));
    }
    let mut header = [0u8; 12];
    file.read_exact(&mut header)?;
    let count = u32::from_le_bytes(header[8..12].try_into().expect("fixed header"));
    if &header[..8] != FURNACE_MAGIC || count > 100_000 || length != 12 + count as u64 * 27 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid furnace save",
        ));
    }
    let mut furnaces = FurnaceSaves::new();
    for _ in 0..count {
        let mut bytes = [0u8; 27];
        file.read_exact(&mut bytes)?;
        let i32_at = |at| i32::from_le_bytes(bytes[at..at + 4].try_into().expect("fixed position"));
        let pos = (i32_at(0), i32_at(4), i32_at(8));
        let mut slots = [None; 3];
        for (index, slot) in slots.iter_mut().enumerate() {
            let at = 12 + index * 3;
            let item = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
            let count = bytes[at + 2];
            if item != 0 || count != 0 {
                *slot = Some((item, count));
            }
        }
        let furnace = crate::furnace::Furnace {
            slots,
            burn_remaining: u16::from_le_bytes([bytes[21], bytes[22]]),
            burn_total: u16::from_le_bytes([bytes[23], bytes[24]]),
            cook_ticks: u16::from_le_bytes([bytes[25], bytes[26]]),
        };
        if !valid_furnace_position(pos)
            || !furnace.valid()
            || furnaces.insert(pos, furnace).is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid furnace state",
            ));
        }
    }
    Ok(Some(furnaces))
}

pub fn save_furnaces(path: &Path, furnaces: &FurnaceSaves) -> io::Result<()> {
    if furnaces.len() > 100_000
        || furnaces
            .iter()
            .any(|(pos, furnace)| !valid_furnace_position(*pos) || !furnace.valid())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid furnace state",
        ));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    let mut file = File::create(&temp)?;
    file.write_all(FURNACE_MAGIC)?;
    file.write_all(&(furnaces.len() as u32).to_le_bytes())?;
    let mut positions: Vec<_> = furnaces.keys().copied().collect();
    positions.sort_unstable();
    for (x, y, z) in positions {
        let furnace = &furnaces[&(x, y, z)];
        for value in [x, y, z] {
            file.write_all(&value.to_le_bytes())?;
        }
        for slot in furnace.slots {
            let (item, count) = slot.unwrap_or((0, 0));
            file.write_all(&item.to_le_bytes())?;
            file.write_all(&[count])?;
        }
        for value in [
            furnace.burn_remaining,
            furnace.burn_total,
            furnace.cook_ticks,
        ] {
            file.write_all(&value.to_le_bytes())?;
        }
    }
    file.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn swords_round_trip_with_remaining_durability() {
        use super::*;
        let path = std::env::temp_dir().join(format!("ssc-sword-save-{}.dat", std::process::id()));
        let id = [0xc3; 16];
        let mut data = PlayerSave::default();
        data.position = Some([8.5, 70.0, 8.5]);
        for (slot, item) in [
            ssc_world::WOODEN_SWORD,
            ssc_world::STONE_SWORD,
            ssc_world::IRON_SWORD,
        ]
        .into_iter()
        .enumerate()
        {
            data.inventory[slot] = Some((item, 1));
            data.durability[slot] = 17;
        }
        save_player(&path, &id, &data).unwrap();
        assert_eq!(load_player(&path, &id).unwrap(), Some(data));
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn iron_tools_preserve_wear_and_validate_durability() {
        use super::*;
        let path =
            std::env::temp_dir().join(format!("ssc-iron-tools-save-{}.dat", std::process::id()));
        let id = [0xb4; 16];
        let mut data = PlayerSave::default();
        data.position = Some([8.5, 70.0, 8.5]);
        for (slot, item) in [
            ssc_world::IRON_PICKAXE,
            ssc_world::IRON_AXE,
            ssc_world::IRON_SHOVEL,
        ]
        .into_iter()
        .enumerate()
        {
            data.inventory[slot] = Some((item, 1));
            data.durability[slot] = 200 + slot as u16;
        }
        save_player(&path, &id, &data).unwrap();
        assert_eq!(load_player(&path, &id).unwrap(), Some(data.clone()));
        data.durability[0] = 252;
        assert!(save_player(&path, &id, &data).is_err());
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn furnace_state_round_trips_and_corruption_is_rejected() {
        use super::*;
        let path =
            std::env::temp_dir().join(format!("ssc-furnace-roundtrip-{}.bin", std::process::id()));
        let mut saves = FurnaceSaves::new();
        saves.insert(
            (-16, 64, 8),
            crate::furnace::Furnace {
                slots: [
                    Some((ssc_world::RAW_IRON, 3)),
                    Some((ssc_world::COAL, 2)),
                    Some((ssc_world::IRON_INGOT, 1)),
                ],
                burn_remaining: 123,
                burn_total: 1600,
                cook_ticks: 87,
            },
        );
        save_furnaces(&path, &saves).unwrap();
        assert_eq!(load_furnaces(&path).unwrap(), Some(saves));
        let mut bytes = fs::read(&path).unwrap();
        bytes[37..39].copy_from_slice(&200u16.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        assert!(load_furnaces(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_file(path).unwrap();
    }
    use super::*;

    #[test]
    fn seed_and_negative_coordinate_edits_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "ssc-world-save-{}-{}.bin",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let mut edits = BlockEdits::new();
        edits
            .entry((-1, -2))
            .or_default()
            .insert((15, 64, 0), ssc_world::BRICKS);

        save(&path, 987654321, 0.625, &edits).unwrap();
        let loaded = load(&path).unwrap().unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(loaded.0, 987654321);
        assert_eq!(loaded.1, 0.625);
        assert_eq!(loaded.2, edits);
    }

    #[test]
    fn truncated_or_unknown_saves_are_rejected() {
        let path =
            std::env::temp_dir().join(format!("ssc-world-save-invalid-{}.bin", std::process::id()));
        for bytes in [&b"short"[..], &b"NOTWORLD"[..]] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(load(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn version_one_save_loads_with_default_time() {
        let path =
            std::env::temp_dir().join(format!("ssc-world-save-v1-{}.bin", std::process::id()));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC_V1);
        bytes.extend_from_slice(&123u64.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        fs::write(&path, bytes).unwrap();

        let loaded = load(&path).unwrap().unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(loaded.0, 123);
        assert_eq!(loaded.1, 0.3);
        assert!(loaded.2.is_empty());
    }

    #[test]
    fn anonymous_player_inventory_round_trips_and_validates_identity() {
        let dir = std::env::temp_dir().join(format!("ssc-player-save-{}", std::process::id()));
        let player_id = [0xabu8; 16];
        let path = player_path(&dir.join("world.bin"), &player_id);
        let mut inventory = vec![None; crate::survival::INVENTORY_SIZE];
        inventory[0] = Some((ssc_world::APPLE, 12));
        inventory[35] = Some((ssc_world::BRICKS, 64));
        let data = PlayerSave {
            inventory,
            durability: vec![0; crate::survival::INVENTORY_SIZE],
            position: Some([12.5, 70.0, -9.25]),
            health: 13.0,
            hunger: 7,
            saturation: 2.5,
            air: 250,
            selected_slot: 4,
            hunger_effect_ticks: 321,
            exhaustion: 2.25,
        };

        save_player(&path, &player_id, &data).unwrap();
        assert_eq!(load_player(&path, &player_id).unwrap().unwrap(), data);
        assert_eq!(
            load_player(&path, &[0x55; 16]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn crafted_pickaxes_round_trip_without_becoming_stacks() {
        let path =
            std::env::temp_dir().join(format!("ssc-crafted-player-{}.dat", std::process::id()));
        let id = [0x98; 16];
        let mut data = PlayerSave::default();
        data.position = Some([8.5, 70.0, 8.5]);
        data.inventory[1] = Some((ssc_world::WOODEN_PICKAXE, 1));
        data.inventory[2] = Some((ssc_world::STONE_PICKAXE, 1));
        data.inventory[3] = Some((ssc_world::STICK, 12));
        data.durability[1] = 4;
        data.durability[2] = 99;
        save_player(&path, &id, &data).unwrap();
        let loaded = load_player(&path, &id).unwrap().unwrap();
        assert_eq!(loaded.inventory, data.inventory);
        assert_eq!(loaded.durability, data.durability);
        data.inventory[1] = Some((ssc_world::WOODEN_PICKAXE, 2));
        assert_eq!(
            save_player(&path, &id, &data).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        data.inventory[1] = Some((ssc_world::WOODEN_PICKAXE, 1));
        let mut invalid = fs::read(&path).unwrap();
        let wear_offset = 50 + crate::survival::INVENTORY_SIZE * 3 + 2;
        invalid[wear_offset..wear_offset + 2].copy_from_slice(&61u16.to_le_bytes());
        fs::write(&path, invalid).unwrap();
        assert_eq!(
            load_player(&path, &id).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn axes_shovels_and_snowball_stacks_round_trip() {
        let path = std::env::temp_dir().join(format!("ssc-tools-save-{}.dat", std::process::id()));
        let id = [0x79; 16];
        let mut data = PlayerSave::default();
        data.position = Some([8.5, 70.0, 8.5]);
        for (slot, item) in [
            ssc_world::WOODEN_AXE,
            ssc_world::STONE_AXE,
            ssc_world::WOODEN_SHOVEL,
            ssc_world::STONE_SHOVEL,
        ]
        .into_iter()
        .enumerate()
        {
            data.inventory[slot] = Some((item, 1));
            data.durability[slot] = 17;
        }
        data.inventory[4] = Some((ssc_world::SNOWBALL, 16));
        save_player(&path, &id, &data).unwrap();
        assert_eq!(load_player(&path, &id).unwrap().unwrap(), data);
        data.inventory[4] = Some((ssc_world::SNOWBALL, 17));
        assert_eq!(
            save_player(&path, &id, &data).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn v3_pickaxe_loads_with_full_durability() {
        let path = std::env::temp_dir().join(format!(
            "ssc-player-wear-migration-{}.dat",
            std::process::id()
        ));
        let id = [0xa9; 16];
        let mut data = PlayerSave::default();
        data.position = Some([8.5, 70.0, 8.5]);
        data.inventory[5] = Some((ssc_world::STONE_PICKAXE, 1));
        data.durability[5] = 1;
        save_player(&path, &id, &data).unwrap();
        let mut v3 = fs::read(&path).unwrap();
        v3.truncate(50 + crate::survival::INVENTORY_SIZE * 3);
        v3[..8].copy_from_slice(PLAYER_MAGIC_V3);
        fs::write(&path, v3).unwrap();
        let loaded = load_player(&path, &id).unwrap().unwrap();
        assert_eq!(loaded.durability[5], 132);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn v4_player_preserves_tool_wear_and_migrates_without_saturation() {
        let path = std::env::temp_dir().join(format!(
            "ssc-player-saturation-migration-{}.dat",
            std::process::id()
        ));
        let id = [0xab; 16];
        let mut data = PlayerSave::default();
        data.position = Some([8.5, 70.0, 8.5]);
        data.inventory[2] = Some((ssc_world::STONE_PICKAXE, 1));
        data.durability[2] = 17;
        data.hunger = 6;
        data.saturation = 2.5;
        save_player(&path, &id, &data).unwrap();
        let mut v4 = fs::read(&path).unwrap();
        v4.truncate(v4.len() - 4);
        v4[..8].copy_from_slice(PLAYER_MAGIC_V4);
        fs::write(&path, &v4).unwrap();
        let loaded = load_player(&path, &id).unwrap().unwrap();
        assert_eq!(loaded.durability[2], 17);
        assert_eq!(loaded.hunger, 6);
        assert_eq!(loaded.saturation, 0.0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn old_player_saves_migrate_without_effects() {
        let path =
            std::env::temp_dir().join(format!("ssc-player-migration-{}.dat", std::process::id()));
        let player_id = [0x42; 16];
        let mut data = PlayerSave::default();
        data.position = Some([8.5, 70.0, -4.5]);
        data.inventory[0] = Some((ssc_world::APPLE, 3));
        data.hunger_effect_ticks = 300;
        data.exhaustion = 2.5;
        save_player(&path, &player_id, &data).unwrap();

        let mut v2 = fs::read(&path).unwrap();
        v2.truncate(50 + crate::survival::INVENTORY_SIZE * 3);
        v2[..8].copy_from_slice(PLAYER_MAGIC_V2);
        v2.drain(44..50);
        fs::write(&path, &v2).unwrap();
        let loaded = load_player(&path, &player_id).unwrap().unwrap();
        assert_eq!(loaded.position, data.position);
        assert_eq!(loaded.inventory, data.inventory);
        assert_eq!(loaded.hunger_effect_ticks, 0);
        assert_eq!(loaded.exhaustion, 0.0);
        assert_eq!(loaded.saturation, 0.0);

        let mut v1 = v2;
        v1[..8].copy_from_slice(PLAYER_MAGIC);
        v1.drain(24..44);
        fs::write(&path, &v1).unwrap();
        let loaded = load_player(&path, &player_id).unwrap().unwrap();
        assert_eq!(loaded.position, None);
        assert_eq!(loaded.inventory, data.inventory);
        assert_eq!(loaded.hunger_effect_ticks, 0);
        assert_eq!(loaded.saturation, 0.0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_player_effects_are_rejected() {
        let path =
            std::env::temp_dir().join(format!("ssc-player-effects-{}.dat", std::process::id()));
        let player_id = [0x43; 16];
        let mut data = PlayerSave::default();
        data.position = Some([8.5, 70.0, 8.5]);
        save_player(&path, &player_id, &data).unwrap();
        let valid = fs::read(&path).unwrap();
        let mut invalid = valid.clone();
        invalid[44..46].copy_from_slice(&601u16.to_le_bytes());
        fs::write(&path, &invalid).unwrap();
        assert_eq!(
            load_player(&path, &player_id).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        invalid = valid;
        invalid[46..50].copy_from_slice(&f32::NAN.to_le_bytes());
        fs::write(&path, &invalid).unwrap();
        assert_eq!(
            load_player(&path, &player_id).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_saturation_is_rejected_without_overwriting_save() {
        let path = std::env::temp_dir().join(format!(
            "ssc-player-invalid-saturation-{}.dat",
            std::process::id()
        ));
        let id = [0xbe; 16];
        let mut data = PlayerSave::default();
        data.position = Some([8.5, 70.0, 8.5]);
        data.hunger = 4;
        data.saturation = 2.0;
        save_player(&path, &id, &data).unwrap();
        let valid = fs::read(&path).unwrap();
        for saturation in [f32::NAN, f32::INFINITY, -1.0, 4.1] {
            let mut invalid = valid.clone();
            let end = invalid.len();
            invalid[end - 4..].copy_from_slice(&saturation.to_le_bytes());
            fs::write(&path, &invalid).unwrap();
            assert_eq!(
                load_player(&path, &id).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(fs::read(&path).unwrap(), invalid);
            data.saturation = saturation;
            assert_eq!(
                save_player(&path, &id, &data).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(fs::read(&path).unwrap(), invalid);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dropped_item_state_round_trips() {
        let path = std::env::temp_dir().join(format!("ssc-item-save-{}.dat", std::process::id()));
        let items = vec![SavedItem {
            item: ssc_world::APPLE,
            count: 3,
            durability: 0,
            pos: [-12.5, 64.25, 3.0],
            vel: [0.25, 1.5, -0.5],
            age: 321,
            pickup_delay: 7,
        }];
        save_items(&path, &items).unwrap();
        assert_eq!(load_items(&path).unwrap().unwrap(), items);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dropped_pickaxe_wear_round_trips_and_v1_loads_full() {
        let path = std::env::temp_dir().join(format!("ssc-item-wear-{}.dat", std::process::id()));
        let item = SavedItem {
            item: ssc_world::WOODEN_PICKAXE,
            count: 1,
            durability: 3,
            pos: [8.5, 70.0, 8.5],
            vel: [0.0; 3],
            age: 20,
            pickup_delay: 0,
        };
        save_items(&path, &[item]).unwrap();
        assert_eq!(load_items(&path).unwrap().unwrap(), vec![item]);
        let mut v1 = fs::read(&path).unwrap();
        v1[..8].copy_from_slice(ITEMS_MAGIC);
        v1.truncate(v1.len() - 2);
        fs::write(&path, v1).unwrap();
        assert_eq!(load_items(&path).unwrap().unwrap()[0].durability, 60);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn mob_state_round_trips() {
        let path = std::env::temp_dir().join(format!("ssc-mob-save-{}.dat", std::process::id()));
        let mobs = vec![SavedMob {
            kind: 2,
            pos: [-32.5, 64.0, 16.25],
            vel: [2.0, 0.0, -1.0],
            yaw: 1.25,
            health: 17.0,
            wander_timer: 0,
            wander_dir: Some([0.5, -0.5]),
            attack_cooldown: 12,
            burn_timer: 8,
            panic_timer: 0,
            on_ground: true,
        }];
        save_mobs(&path, &mobs).unwrap();
        assert_eq!(load_mobs(&path).unwrap().unwrap(), mobs);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fleeing_pig_round_trips_and_legacy_mobs_default_to_no_panic() {
        let path =
            std::env::temp_dir().join(format!("ssc-panic-mob-save-{}.dat", std::process::id()));
        let pig = SavedMob {
            kind: 1,
            pos: [8.5, 65.0, 8.5],
            vel: [1.2, 0.0, 0.0],
            yaw: 0.0,
            health: 9.0,
            wander_timer: 0,
            wander_dir: Some([1.0, 0.0]),
            attack_cooldown: 0,
            burn_timer: 0,
            panic_timer: 37,
            on_ground: true,
        };
        save_mobs(&path, &[pig]).unwrap();
        assert_eq!(load_mobs(&path).unwrap().unwrap(), vec![pig]);
        let valid = fs::read(&path).unwrap();
        let mut legacy = valid.clone();
        legacy[..8].copy_from_slice(MOBS_MAGIC);
        legacy.pop();
        fs::write(&path, &legacy).unwrap();
        let migrated = load_mobs(&path).unwrap().unwrap();
        assert_eq!(migrated[0].panic_timer, 0);
        assert_eq!(migrated[0].wander_dir, pig.wander_dir);
        let mut invalid = valid;
        *invalid.last_mut().unwrap() = 61;
        fs::write(&path, &invalid).unwrap();
        assert_eq!(
            load_mobs(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(&path).unwrap(), invalid);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dead_mob_save_is_rejected_without_overwriting_existing_data() {
        let path =
            std::env::temp_dir().join(format!("ssc-dead-mob-save-{}.dat", std::process::id()));
        let existing = b"existing mob save";
        fs::write(&path, existing).unwrap();
        let mob = SavedMob {
            kind: 1,
            pos: [8.5, 65.0, 8.5],
            vel: [0.0; 3],
            yaw: 0.0,
            health: 0.0,
            wander_timer: 0,
            wander_dir: None,
            attack_cooldown: 0,
            burn_timer: 0,
            panic_timer: 0,
            on_ground: true,
        };
        assert!(save_mobs(&path, &[mob]).is_err());
        assert_eq!(fs::read(&path).unwrap(), existing);
        fs::remove_file(&path).unwrap();
    }
}
