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

const MAGIC: &[u8; 8] = b"SSCWLD01";
const HEADER_SIZE: u64 = 20;
const RECORD_SIZE: u64 = 14;
const MAX_EDITS: u32 = 4_000_000;

/// Load a save. `Ok(None)` means no save exists.
pub fn load(path: &Path) -> io::Result<Option<(u64, BlockEdits)>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let file_len = file.metadata()?.len();
    if file_len < HEADER_SIZE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "save header is truncated"));
    }

    let mut header = [0u8; HEADER_SIZE as usize];
    file.read_exact(&mut header)?;
    if &header[..8] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad world save magic"));
    }
    let seed = u64::from_le_bytes(header[8..16].try_into().expect("fixed header slice"));
    let count = u32::from_le_bytes(header[16..20].try_into().expect("fixed header slice"));
    if count > MAX_EDITS || file_len != HEADER_SIZE + count as u64 * RECORD_SIZE {
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
    Ok(Some((seed, edits)))
}

/// Atomically save the seed and explicit block edits.
pub fn save(path: &Path, seed: u64, edits: &BlockEdits) -> io::Result<()> {
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

        save(&path, 987654321, &edits).unwrap();
        let loaded = load(&path).unwrap().unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(loaded.0, 987654321);
        assert_eq!(loaded.1, edits);
    }
}
