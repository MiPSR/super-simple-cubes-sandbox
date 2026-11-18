//! Packet definitions and their wire encoding.
//!
//! Every gameplay-relevant event (including sounds) flows through these
//! packets from the server (`specs.md` §2). Encoding: one `u8` packet tag
//! followed by the payload, little-endian, via [`crate::codec`].

use crate::codec::{ByteReader, ByteWriter, DecodeResult};

/// Input key bits for [`ClientPacket::Input`].
pub mod keys {
    pub const FORWARD: u16 = 1 << 0;
    pub const BACKWARD: u16 = 1 << 1;
    pub const LEFT: u16 = 1 << 2;
    pub const RIGHT: u16 = 1 << 3;
    pub const JUMP: u16 = 1 << 4;
    pub const SNEAK: u16 = 1 << 5;
    pub const SPRINT: u16 = 1 << 6;
}

/// Sound trigger ids — chosen by the *server*; the client only plays them
/// (or logs them until audio playback is approved and wired, `specs.md` §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundId {
    Dig,
    Place,
    Step,
    _Ext(u16),
}

impl SoundId {
    pub fn to_u16(self) -> u16 {
        match self {
            SoundId::Dig => 0,
            SoundId::Place => 1,
            SoundId::Step => 2,
            SoundId::_Ext(v) => v,
        }
    }

    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => SoundId::Dig,
            1 => SoundId::Place,
            2 => SoundId::Step,
            _ => SoundId::_Ext(v),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SoundId::Dig => "dig",
            SoundId::Place => "place",
            SoundId::Step => "step",
            SoundId::_Ext(_) => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Client → Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ClientPacket {
    /// Initial handshake.
    Hello { version: u32, name: String },
    /// Player input snapshot at ~20 Hz: movement keys + look angles.
    Input { keys: u16, yaw: f32, pitch: f32 },
    /// Hotbar slot selection.
    SelectSlot { slot: u8 },
    /// Request to break the block being looked at (server raycasts).
    Dig,
    /// Request to place the selected hotbar block (server raycasts).
    Place,
}

impl ClientPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = ByteWriter::new();
        match self {
            ClientPacket::Hello { version, name } => {
                w.put_u8(0);
                w.put_u32(*version);
                w.put_string(name);
            }
            ClientPacket::Input { keys, yaw, pitch } => {
                w.put_u8(1);
                w.put_u16(*keys);
                w.put_f32(*yaw);
                w.put_f32(*pitch);
            }
            ClientPacket::SelectSlot { slot } => {
                w.put_u8(2);
                w.put_u8(*slot);
            }
            ClientPacket::Dig => w.put_u8(3),
            ClientPacket::Place => w.put_u8(4),
        }
        w.into_bytes()
    }

    pub fn decode(data: &[u8]) -> DecodeResult<Self> {
        let mut r = ByteReader::new(data);
        let tag = r.get_u8()?;
        let pkt = match tag {
            0 => ClientPacket::Hello {
                version: r.get_u32()?,
                name: r.get_string()?,
            },
            1 => ClientPacket::Input {
                keys: r.get_u16()?,
                yaw: r.get_f32()?,
                pitch: r.get_f32()?,
            },
            2 => ClientPacket::SelectSlot { slot: r.get_u8()? },
            3 => ClientPacket::Dig,
            4 => ClientPacket::Place,
            _ => return Err(crate::codec::DecodeError(format!("bad client packet tag {tag}"))),
        };
        r.expect_empty()?;
        Ok(pkt)
    }
}

// ---------------------------------------------------------------------------
// Server → Client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ServerPacket {
    /// Join acceptance: your entity id, spawn position, render distance,
    /// hotbar contents.
    Welcome {
        entity_id: u16,
        spawn: [f32; 3],
        render_distance: u8,
        hotbar: [u16; 9],
    },
    /// One 16³ sub-chunk of a column.
    Subchunk {
        cx: i32,
        cz: i32,
        sub: u8,
        blocks: Vec<u16>,
    },
    /// Marks a column as fully streamed; the client may mesh it now.
    ColumnEnd { cx: i32, cz: i32 },
    /// The server no longer tracks this column; drop its meshes.
    UnloadColumn { cx: i32, cz: i32 },
    /// LOD summary of a column (Distant Horizons-style far data,
    /// `specs.md` §3): 4×4 cells of (height, top tile), 4×4 blocks each.
    LodColumn {
        cx: i32,
        cz: i32,
        /// Interleaved `[height, tile; 16]` (height 0 = void).
        cells: [u16; 32],
    },
    /// Drop the LOD summary of a column.
    UnloadLod { cx: i32, cz: i32 },
    /// Authoritative entity state (all players, 20 Hz, own id included;
    /// clients interpolate everything the same way).
    EntityState {
        id: u16,
        x: f32,
        y: f32,
        z: f32,
        yaw: f32,
        pitch: f32,
    },
    /// An entity left the world.
    EntityRemove { id: u16 },
    /// A single block changed.
    BlockUpdate {
        x: i32,
        y: i32,
        z: i32,
        block: u16,
    },
    /// Server-driven sound trigger at a world position.
    Sound { sound: u16, x: f32, y: f32, z: f32 },
    /// Server is shutting down the session.
    Goodbye { reason: String },
}

impl ServerPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = ByteWriter::new();
        match self {
            ServerPacket::Welcome { entity_id, spawn, render_distance, hotbar } => {
                w.put_u8(0);
                w.put_u16(*entity_id);
                for v in spawn {
                    w.put_f32(*v);
                }
                w.put_u8(*render_distance);
                for b in hotbar {
                    w.put_u16(*b);
                }
            }
            ServerPacket::Subchunk { cx, cz, sub, blocks } => {
                w.put_u8(1);
                w.put_i32(*cx);
                w.put_i32(*cz);
                w.put_u8(*sub);
                w.put_block_array(blocks);
            }
            ServerPacket::ColumnEnd { cx, cz } => {
                w.put_u8(2);
                w.put_i32(*cx);
                w.put_i32(*cz);
            }
            ServerPacket::UnloadColumn { cx, cz } => {
                w.put_u8(3);
                w.put_i32(*cx);
                w.put_i32(*cz);
            }
            ServerPacket::LodColumn { cx, cz, cells } => {
                w.put_u8(4);
                w.put_i32(*cx);
                w.put_i32(*cz);
                for c in cells {
                    w.put_u16(*c);
                }
            }
            ServerPacket::UnloadLod { cx, cz } => {
                w.put_u8(5);
                w.put_i32(*cx);
                w.put_i32(*cz);
            }
            ServerPacket::EntityState { id, x, y, z, yaw, pitch } => {
                w.put_u8(6);
                w.put_u16(*id);
                w.put_f32(*x);
                w.put_f32(*y);
                w.put_f32(*z);
                w.put_f32(*yaw);
                w.put_f32(*pitch);
            }
            ServerPacket::EntityRemove { id } => {
                w.put_u8(7);
                w.put_u16(*id);
            }
            ServerPacket::BlockUpdate { x, y, z, block } => {
                w.put_u8(8);
                w.put_i32(*x);
                w.put_i32(*y);
                w.put_i32(*z);
                w.put_u16(*block);
            }
            ServerPacket::Sound { sound, x, y, z } => {
                w.put_u8(9);
                w.put_u16(*sound);
                w.put_f32(*x);
                w.put_f32(*y);
                w.put_f32(*z);
            }
            ServerPacket::Goodbye { reason } => {
                w.put_u8(10);
                w.put_string(reason);
            }
        }
        w.into_bytes()
    }

    pub fn decode(data: &[u8]) -> DecodeResult<Self> {
        let mut r = ByteReader::new(data);
        let tag = r.get_u8()?;
        let pkt = match tag {
            0 => {
                let entity_id = r.get_u16()?;
                let spawn = [
                    r.get_f32()?,
                    r.get_f32()?,
                    r.get_f32()?,
                ];
                let render_distance = r.get_u8()?;
                let mut hotbar = [0u16; 9];
                for b in &mut hotbar {
                    *b = r.get_u16()?;
                }
                ServerPacket::Welcome { entity_id, spawn, render_distance, hotbar }
            }
            1 => ServerPacket::Subchunk {
                cx: r.get_i32()?,
                cz: r.get_i32()?,
                sub: r.get_u8()?,
                blocks: r.get_block_array()?,
            },
            2 => ServerPacket::ColumnEnd { cx: r.get_i32()?, cz: r.get_i32()? },
            3 => ServerPacket::UnloadColumn { cx: r.get_i32()?, cz: r.get_i32()? },
            4 => {
                let cx = r.get_i32()?;
                let cz = r.get_i32()?;
                let mut cells = [0u16; 32];
                for c in &mut cells {
                    *c = r.get_u16()?;
                }
                ServerPacket::LodColumn { cx, cz, cells }
            }
            5 => ServerPacket::UnloadLod { cx: r.get_i32()?, cz: r.get_i32()? },
            6 => ServerPacket::EntityState {
                id: r.get_u16()?,
                x: r.get_f32()?,
                y: r.get_f32()?,
                z: r.get_f32()?,
                yaw: r.get_f32()?,
                pitch: r.get_f32()?,
            },
            7 => ServerPacket::EntityRemove { id: r.get_u16()? },
            8 => ServerPacket::BlockUpdate {
                x: r.get_i32()?,
                y: r.get_i32()?,
                z: r.get_i32()?,
                block: r.get_u16()?,
            },
            9 => ServerPacket::Sound {
                sound: r.get_u16()?,
                x: r.get_f32()?,
                y: r.get_f32()?,
                z: r.get_f32()?,
            },
            10 => ServerPacket::Goodbye { reason: r.get_string()? },
            _ => {
                return Err(crate::codec::DecodeError(format!(
                    "bad server packet tag {tag}"
                )))
            }
        };
        r.expect_empty()?;
        Ok(pkt)
    }
}
