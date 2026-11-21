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
const ITEMS_MAGIC: &[u8; 8] = b"SSCITM01";
const MOBS_MAGIC: &[u8; 8] = b"SSCMOB01";
const MAX_SAVED_ITEMS: u32 = 1_000_000;
const MAX_SAVED_MOBS: u32 = 4_096;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SavedItem {
    pub item: ssc_world::ItemId,
    pub count: u8,
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
    pub on_ground: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSave {
    pub inventory: Vec<crate::survival::Slot>,
    pub position: Option<[f32; 3]>,
    pub health: f32,
    pub hunger: u8,
    pub air: u16,
    pub selected_slot: u8,
}

impl Default for PlayerSave {
    fn default() -> Self {
        Self {
            inventory: vec![None; crate::survival::INVENTORY_SIZE],
            position: None,
            health: crate::survival::MAX_HEALTH,
            hunger: crate::survival::MAX_HUNGER,
            air: crate::survival::MAX_AIR,
            selected_slot: 0,
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
        return Err(io::Error::new(io::ErrorKind::InvalidData, "save header is truncated"));
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
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad world save magic"));
    };
    if count > MAX_EDITS || file_len != header_size + count as u64 * RECORD_SIZE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid world save length"));
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
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too many world edits"));
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
pub fn load_player(
    path: &Path,
    player_id: &[u8; 16],
) -> io::Result<Option<PlayerSave>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let file_len = file.metadata()?.len();
    let v1_len = 8 + 16 + crate::survival::INVENTORY_SIZE * 3;
    let v2_len = 8 + 16 + 20 + crate::survival::INVENTORY_SIZE * 3;
    if file_len != v1_len as u64 && file_len != v2_len as u64 {
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
    if (!v2 && &bytes[..8] != PLAYER_MAGIC) || &bytes[8..24] != player_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "player save identity or format mismatch",
        ));
    }
    if v2 != (expected_len == v2_len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "player save version and length mismatch",
        ));
    }
    let mut data = PlayerSave::default();
    data.inventory.clear();
    let inventory_offset = if v2 {
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
        44
    } else {
        24
    };
    for slot in bytes[inventory_offset..].chunks_exact(3) {
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
    Ok(Some(data))
}

/// Atomically write a player's inventory.
pub fn save_player(
    path: &Path,
    player_id: &[u8; 16],
    data: &PlayerSave,
) -> io::Result<()> {
    if data.inventory.len() != crate::survival::INVENTORY_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "player inventory has wrong slot count",
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
        || data.air > crate::survival::MAX_AIR
        || data.selected_slot >= 9
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid player vitals"));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("tmp");
    let mut file = File::create(&temp_path)?;
    file.write_all(PLAYER_MAGIC_V2)?;
    file.write_all(player_id)?;
    for coordinate in position {
        file.write_all(&coordinate.to_le_bytes())?;
    }
    file.write_all(&data.health.to_le_bytes())?;
    file.write_all(&[data.hunger])?;
    file.write_all(&data.air.to_le_bytes())?;
    file.write_all(&[data.selected_slot])?;
    for slot in &data.inventory {
        if let Some((item, count)) = slot {
            file.write_all(&item.to_le_bytes())?;
            file.write_all(&[*count])?;
        } else {
            file.write_all(&[0, 0, 0])?;
        }
    }
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
    const RECORD: u64 = 33;
    let file_len = file.metadata()?.len();
    if file_len < HEADER {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated item save"));
    }
    let mut header = [0u8; HEADER as usize];
    file.read_exact(&mut header)?;
    if &header[..8] != ITEMS_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid item save magic"));
    }
    let count = u32::from_le_bytes(header[8..12].try_into().expect("fixed item header"));
    if count > MAX_SAVED_ITEMS || file_len != HEADER + count as u64 * RECORD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid item save length"));
    }
    let mut items = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut record = [0u8; RECORD as usize];
        file.read_exact(&mut record)?;
        let item = u16::from_le_bytes([record[0], record[1]]);
        let amount = record[2];
        let mut floats = [0.0f32; 6];
        for (i, bytes) in record[3..27].chunks_exact(4).enumerate() {
            floats[i] = f32::from_le_bytes(bytes.try_into().expect("fixed item float"));
        }
        let age = u32::from_le_bytes(record[27..31].try_into().expect("fixed item age"));
        let pickup_delay = record[31];
        // Byte 32 is reserved for forward-compatible item flags.
        if record[32] != 0
            || !ssc_world::valid_item_id(item)
            || item == 0
            || amount == 0
            || amount > ssc_world::item_info(item).max_stack
            || floats.iter().any(|v| !v.is_finite())
            || floats[0].abs() > 30_000_000.0
            || floats[2].abs() > 30_000_000.0
            || !(ssc_world::MIN_Y as f32 - 1.0..ssc_world::MAX_Y as f32 + 1.0)
                .contains(&floats[1])
            || age >= 6_000
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid dropped item in save",
            ));
        }
        items.push(SavedItem {
            item,
            count: amount,
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
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many dropped items"));
    }
    if items.iter().any(|entry| {
        !ssc_world::valid_item_id(entry.item)
            || entry.item == 0
            || entry.count == 0
            || entry.count > ssc_world::item_info(entry.item).max_stack
            || entry.pos.iter().chain(entry.vel.iter()).any(|v| !v.is_finite())
            || entry.pos[0].abs() > 30_000_000.0
            || entry.pos[2].abs() > 30_000_000.0
            || !(ssc_world::MIN_Y as f32 - 1.0..ssc_world::MAX_Y as f32 + 1.0)
                .contains(&entry.pos[1])
            || entry.age >= 6_000
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
    file.write_all(ITEMS_MAGIC)?;
    file.write_all(&(items.len() as u32).to_le_bytes())?;
    for entry in items {
        file.write_all(&entry.item.to_le_bytes())?;
        file.write_all(&[entry.count])?;
        for value in entry.pos.into_iter().chain(entry.vel) {
            file.write_all(&value.to_le_bytes())?;
        }
        file.write_all(&entry.age.to_le_bytes())?;
        file.write_all(&[entry.pickup_delay, 0])?;
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
    const RECORD: u64 = 51;
    let file_len = file.metadata()?.len();
    if file_len < HEADER {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated mob save"));
    }
    let mut header = [0u8; HEADER as usize];
    file.read_exact(&mut header)?;
    if &header[..8] != MOBS_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid mob save magic"));
    }
    let count = u32::from_le_bytes(header[8..12].try_into().expect("fixed mob header"));
    if count > MAX_SAVED_MOBS || file_len != HEADER + count as u64 * RECORD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid mob save length"));
    }
    let mut mobs = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut record = [0u8; RECORD as usize];
        file.read_exact(&mut record)?;
        let kind = record[0];
        let flags = record[1];
        let mut floats = [0.0f32; 8];
        for (i, bytes) in record[2..34].chunks_exact(4).enumerate() {
            floats[i] = f32::from_le_bytes(bytes.try_into().expect("fixed mob float"));
        }
        let wander_timer =
            u32::from_le_bytes(record[34..38].try_into().expect("fixed mob timer"));
        let attack_cooldown =
            u32::from_le_bytes(record[46..50].try_into().expect("fixed mob cooldown"));
        let burn_timer = record[50];
        let max_health = match kind {
            1 => 10.0,
            2 => 20.0,
            _ => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown mob kind"));
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
            || !(0.0..=max_health).contains(&health)
            || pos[0].abs() > 30_000_000.0
            || pos[2].abs() > 30_000_000.0
            || pos[1] < ssc_world::MIN_Y as f32 - 1.0
            || pos[1] >= ssc_world::MAX_Y as f32
            || wander_timer > 120
            || attack_cooldown > 20
            || burn_timer >= 20
            || wander_dir.is_some_and(|d| d.iter().any(|v| !v.is_finite()))
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid mob state"));
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
            || !(0.0..=max_health).contains(&mob.health)
            || mob.pos[0].abs() > 30_000_000.0
            || mob.pos[2].abs() > 30_000_000.0
            || mob.pos[1] < ssc_world::MIN_Y as f32 - 1.0
            || mob.pos[1] >= ssc_world::MAX_Y as f32
            || mob.wander_timer > 120
            || mob.attack_cooldown > 20
            || mob.burn_timer >= 20
            || mob.wander_dir.is_some_and(|d| d.iter().any(|v| !v.is_finite()))
    }) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid mob state"));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("tmp");
    let mut file = File::create(&temp_path)?;
    file.write_all(MOBS_MAGIC)?;
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
    }
    file.sync_all()?;
    fs::rename(temp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
        let path = std::env::temp_dir().join(format!(
            "ssc-world-save-invalid-{}.bin",
            std::process::id()
        ));
        for bytes in [&b"short"[..], &b"NOTWORLD"[..]] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(load(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn version_one_save_loads_with_default_time() {
        let path = std::env::temp_dir().join(format!(
            "ssc-world-save-v1-{}.bin",
            std::process::id()
        ));
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
            position: Some([12.5, 70.0, -9.25]),
            health: 13.0,
            hunger: 7,
            air: 250,
            selected_slot: 4,
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
    fn dropped_item_state_round_trips() {
        let path = std::env::temp_dir().join(format!(
            "ssc-item-save-{}.dat",
            std::process::id()
        ));
        let items = vec![SavedItem {
            item: ssc_world::APPLE,
            count: 3,
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
    fn mob_state_round_trips() {
        let path = std::env::temp_dir().join(format!(
            "ssc-mob-save-{}.dat",
            std::process::id()
        ));
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
            on_ground: true,
        }];
        save_mobs(&path, &mobs).unwrap();
        assert_eq!(load_mobs(&path).unwrap().unwrap(), mobs);
        fs::remove_file(path).unwrap();
    }
}
