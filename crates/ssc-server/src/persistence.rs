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
}
