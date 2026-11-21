//! Anonymous persistent client identity.
//!
//! This UUID identifies a client installation; it is not a credential and
//! must never be treated as proof of account ownership by a server.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;

pub type PlayerId = [u8; 16];

/// Load the installation UUID, creating it on first start.
pub fn load_or_create() -> io::Result<PlayerId> {
    let dir = data_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("anonymous-player-id");
    load_or_create_at(&path)
}

fn load_or_create_at(path: &std::path::Path) -> io::Result<PlayerId> {
    match read_id(&path) {
        Ok(id) => return Ok(id),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let mut id = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut id)?;
    // RFC 4122 version-4/variant bits; the bytes remain opaque on the wire.
    id[6] = (id[6] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;

    let temp_path = path.with_file_name(format!(
        ".anonymous-player-id-{}.tmp",
        id.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(&id)?;
        file.sync_all()?;
        match fs::hard_link(&temp_path, path) {
            Ok(()) => Ok(id),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => read_id(path),
            Err(err) => Err(err),
        }
    })();
    let _ = fs::remove_file(temp_path);
    result
}

fn read_id(path: &std::path::Path) -> io::Result<PlayerId> {
    let mut file = File::open(path)?;
    let mut id = [0u8; 16];
    file.read_exact(&mut id)?;
    let mut trailing = [0u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "anonymous player ID file must contain exactly 16 bytes",
        ));
    }
    Ok(id)
}

fn data_dir() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("SSC_DATA_DIR") {
        return Ok(path.into());
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("super-simple-cubes"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("super-simple-cubes"));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no data directory: set SSC_DATA_DIR, XDG_DATA_HOME, or HOME",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_has_uuid_v4_version_and_variant_bits() {
        let path = std::env::temp_dir().join(format!(
            "ssc-client-id-{}",
            std::process::id()
        ));
        let id = load_or_create_at(&path).expect("create client UUID");
        assert_eq!(load_or_create_at(&path).unwrap(), id);
        fs::remove_file(path).unwrap();
        assert_eq!(id[6] >> 4, 4);
        assert_eq!(id[8] >> 6, 2);
    }
}
