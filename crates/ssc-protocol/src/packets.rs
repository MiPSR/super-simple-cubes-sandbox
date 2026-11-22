//! Packet definitions and their wire encoding.
//!
//! Every gameplay-relevant event (including sounds) flows through these
//! packets from the server (`specs.md` §2). Encoding: one `u8` packet tag
//! followed by the payload, little-endian, via [`crate::codec`].

use crate::codec::{ByteReader, ByteWriter, DecodeResult};

/// Game modes (wire values).
pub mod game_mode {
    pub const SURVIVAL: u8 = 0;
    pub const CREATIVE: u8 = 1;
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoundId {
    Dig,
    Place,
    Step,
    Hurt,
    Eat,
    _Ext(u16),
}

impl SoundId {
    pub fn to_u16(self) -> u16 {
        match self {
            SoundId::Dig => 0,
            SoundId::Place => 1,
            SoundId::Step => 2,
            SoundId::Hurt => 3,
            SoundId::Eat => 4,
            SoundId::_Ext(v) => v,
        }
    }

    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => SoundId::Dig,
            1 => SoundId::Place,
            2 => SoundId::Step,
            3 => SoundId::Hurt,
            4 => SoundId::Eat,
            _ => SoundId::_Ext(v),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SoundId::Dig => "dig",
            SoundId::Place => "place",
            SoundId::Step => "step",
            SoundId::Hurt => "hurt",
            SoundId::Eat => "eat",
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
    Hello {
        version: u32,
        player_id: [u8; 16],
        name: String,
    },
    /// Player input snapshot at ~20 Hz: movement keys + look angles.
    Input { keys: u16, yaw: f32, pitch: f32 },
    /// Hotbar slot selection.
    SelectSlot { slot: u8 },
    /// Request to start digging (creative: instant break; survival:
    /// progress begins server-side).
    DigStart,
    /// Stop digging (release; cancels survival progress).
    DigStop,
    /// Request to place the selected hotbar block / eat the selected food
    /// (server raycasts and decides).
    Place,
    /// Creative: assign a block from the palette to a hotbar slot.
    SetHotbarSlot { slot: u8, block: u16 },
    /// Move items within the own inventory (slot indices 0..36).
    SwapSlots { a: u8, b: u8 },
    /// Craft one server-defined recipe from the own inventory.
    Craft { recipe: u8 },
    /// Drop one item from the selected survival hotbar stack (Q).
    DropSelected,
}

impl ClientPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = ByteWriter::new();
        match self {
            ClientPacket::Hello {
                version,
                player_id,
                name,
            } => {
                w.put_u8(0);
                w.put_u32(*version);
                w.put_bytes(player_id);
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
            ClientPacket::DigStart => w.put_u8(6),
            ClientPacket::DigStop => w.put_u8(7),
            ClientPacket::Place => w.put_u8(4),
            ClientPacket::SetHotbarSlot { slot, block } => {
                w.put_u8(5);
                w.put_u8(*slot);
                w.put_u16(*block);
            }
            ClientPacket::SwapSlots { a, b } => {
                w.put_u8(8);
                w.put_u8(*a);
                w.put_u8(*b);
            }
            ClientPacket::Craft { recipe } => {
                w.put_u8(9);
                w.put_u8(*recipe);
            }
            ClientPacket::DropSelected => w.put_u8(10),
        }
        w.into_bytes()
    }

    pub fn decode(data: &[u8]) -> DecodeResult<Self> {
        let mut r = ByteReader::new(data);
        let tag = r.get_u8()?;
        let pkt = match tag {
            0 => ClientPacket::Hello {
                version: r.get_u32()?,
                player_id: r.get_16_bytes()?,
                name: r.get_string()?,
            },
            1 => ClientPacket::Input {
                keys: r.get_u16()?,
                yaw: r.get_f32()?,
                pitch: r.get_f32()?,
            },
            2 => ClientPacket::SelectSlot { slot: r.get_u8()? },
            6 => ClientPacket::DigStart,
            7 => ClientPacket::DigStop,
            4 => ClientPacket::Place,
            5 => ClientPacket::SetHotbarSlot {
                slot: r.get_u8()?,
                block: r.get_u16()?,
            },
            8 => ClientPacket::SwapSlots {
                a: r.get_u8()?,
                b: r.get_u8()?,
            },
            9 => ClientPacket::Craft {
                recipe: r.get_u8()?,
            },
            10 => ClientPacket::DropSelected,
            _ => {
                return Err(crate::codec::DecodeError(format!(
                    "bad client packet tag {tag}"
                )))
            }
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
    /// hotbar contents, game mode.
    Welcome {
        entity_id: u16,
        spawn: [f32; 3],
        render_distance: u8,
        hotbar: [u16; 9],
        game_mode: u8,
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
    /// Authoritative entity state (20 Hz, own id included; clients
    /// interpolate everything the same way). `kind`: 0 = player,
    /// 1 = pig, 2 = zombie.
    EntityState {
        id: u16,
        x: f32,
        y: f32,
        z: f32,
        yaw: f32,
        pitch: f32,
        kind: u8,
    },
    /// An entity left the world.
    EntityRemove { id: u16 },
    /// A single block changed.
    BlockUpdate { x: i32, y: i32, z: i32, block: u16 },
    /// Server-driven sound trigger at a world position.
    Sound { sound: u16, x: f32, y: f32, z: f32 },
    /// Server is shutting down the session.
    Goodbye { reason: String },
    /// The creative block palette (what the picker may offer).
    CreativeBlocks { ids: Vec<u16> },
    /// The player's hotbar contents (after a creative assignment).
    Hotbar { slots: [u16; 9] },
    /// Time of day, 0..1 (0 = dawn). Minecraft pace: a full cycle is
    /// 24000 ticks (~20 minutes at 20 TPS).
    TimeOfDay { t: f32 },
    /// Full inventory sync (36 slots; hotbar = slots 0..9).
    Inventory { slots: Vec<(u16, u8)> },
    /// Survival status: health (0..20), hunger (0..20), air (0..300),
    /// remaining Hunger I duration in server ticks.
    Status {
        health: f32,
        hunger: u8,
        air: u16,
        hunger_effect_ticks: u16,
    },
    /// Dig progress broadcast (0..100; 101 = cancelled).
    DigProgress {
        x: i32,
        y: i32,
        z: i32,
        progress: u8,
    },
    /// Server-owned dropped item stack.
    ItemState {
        id: u16,
        item: u16,
        count: u8,
        x: f32,
        y: f32,
        z: f32,
    },
    /// Remaining uses for 36 inventory slots; zero for non-tools.
    ToolDurability { slots: Vec<u16> },
}

impl ServerPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = ByteWriter::new();
        match self {
            ServerPacket::Welcome {
                entity_id,
                spawn,
                render_distance,
                hotbar,
                game_mode,
            } => {
                w.put_u8(0);
                w.put_u16(*entity_id);
                for v in spawn {
                    w.put_f32(*v);
                }
                w.put_u8(*render_distance);
                for b in hotbar {
                    w.put_u16(*b);
                }
                w.put_u8(*game_mode);
            }
            ServerPacket::Subchunk {
                cx,
                cz,
                sub,
                blocks,
            } => {
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
            ServerPacket::EntityState {
                id,
                x,
                y,
                z,
                yaw,
                pitch,
                kind,
            } => {
                w.put_u8(6);
                w.put_u16(*id);
                w.put_f32(*x);
                w.put_f32(*y);
                w.put_f32(*z);
                w.put_f32(*yaw);
                w.put_f32(*pitch);
                w.put_u8(*kind);
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
            ServerPacket::CreativeBlocks { ids } => {
                w.put_u8(11);
                w.put_block_array(ids);
            }
            ServerPacket::Hotbar { slots } => {
                w.put_u8(12);
                for b in slots {
                    w.put_u16(*b);
                }
            }
            ServerPacket::TimeOfDay { t } => {
                w.put_u8(13);
                w.put_f32(*t);
            }
            ServerPacket::Inventory { slots } => {
                w.put_u8(14);
                debug_assert!(slots.len() <= u16::MAX as usize);
                w.put_u16(slots.len() as u16);
                for (item, count) in slots {
                    w.put_u16(*item);
                    w.put_u8(*count);
                }
            }
            ServerPacket::Status {
                health,
                hunger,
                air,
                hunger_effect_ticks,
            } => {
                w.put_u8(15);
                w.put_f32(*health);
                w.put_u8(*hunger);
                w.put_u16(*air);
                w.put_u16(*hunger_effect_ticks);
            }
            ServerPacket::DigProgress { x, y, z, progress } => {
                w.put_u8(16);
                w.put_i32(*x);
                w.put_i32(*y);
                w.put_i32(*z);
                w.put_u8(*progress);
            }
            ServerPacket::ItemState {
                id,
                item,
                count,
                x,
                y,
                z,
            } => {
                w.put_u8(17);
                w.put_u16(*id);
                w.put_u16(*item);
                w.put_u8(*count);
                w.put_f32(*x);
                w.put_f32(*y);
                w.put_f32(*z);
            }
            ServerPacket::ToolDurability { slots } => {
                w.put_u8(18);
                w.put_u16(slots.len() as u16);
                for wear in slots {
                    w.put_u16(*wear);
                }
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
                let spawn = [r.get_f32()?, r.get_f32()?, r.get_f32()?];
                let render_distance = r.get_u8()?;
                let mut hotbar = [0u16; 9];
                for b in &mut hotbar {
                    *b = r.get_u16()?;
                }
                let game_mode = r.get_u8()?;
                ServerPacket::Welcome {
                    entity_id,
                    spawn,
                    render_distance,
                    hotbar,
                    game_mode,
                }
            }
            1 => ServerPacket::Subchunk {
                cx: r.get_i32()?,
                cz: r.get_i32()?,
                sub: r.get_u8()?,
                blocks: r.get_block_array()?,
            },
            2 => ServerPacket::ColumnEnd {
                cx: r.get_i32()?,
                cz: r.get_i32()?,
            },
            3 => ServerPacket::UnloadColumn {
                cx: r.get_i32()?,
                cz: r.get_i32()?,
            },
            4 => {
                let cx = r.get_i32()?;
                let cz = r.get_i32()?;
                let mut cells = [0u16; 32];
                for c in &mut cells {
                    *c = r.get_u16()?;
                }
                ServerPacket::LodColumn { cx, cz, cells }
            }
            5 => ServerPacket::UnloadLod {
                cx: r.get_i32()?,
                cz: r.get_i32()?,
            },
            6 => ServerPacket::EntityState {
                id: r.get_u16()?,
                x: r.get_f32()?,
                y: r.get_f32()?,
                z: r.get_f32()?,
                yaw: r.get_f32()?,
                pitch: r.get_f32()?,
                kind: r.get_u8()?,
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
            10 => ServerPacket::Goodbye {
                reason: r.get_string()?,
            },
            11 => ServerPacket::CreativeBlocks {
                ids: r.get_block_array()?,
            },
            12 => {
                let mut slots = [0u16; 9];
                for b in &mut slots {
                    *b = r.get_u16()?;
                }
                ServerPacket::Hotbar { slots }
            }
            13 => ServerPacket::TimeOfDay { t: r.get_f32()? },
            14 => {
                let len = r.get_u16()? as usize;
                let mut slots = Vec::with_capacity(len);
                for _ in 0..len {
                    slots.push((r.get_u16()?, r.get_u8()?));
                }
                ServerPacket::Inventory { slots }
            }
            15 => ServerPacket::Status {
                health: r.get_f32()?,
                hunger: r.get_u8()?,
                air: r.get_u16()?,
                hunger_effect_ticks: r.get_u16()?,
            },
            16 => ServerPacket::DigProgress {
                x: r.get_i32()?,
                y: r.get_i32()?,
                z: r.get_i32()?,
                progress: r.get_u8()?,
            },
            17 => ServerPacket::ItemState {
                id: r.get_u16()?,
                item: r.get_u16()?,
                count: r.get_u8()?,
                x: r.get_f32()?,
                y: r.get_f32()?,
                z: r.get_f32()?,
            },
            18 => {
                let len = r.get_u16()? as usize;
                if len != 36 {
                    return Err(crate::codec::DecodeError(
                        "invalid tool durability length".into(),
                    ));
                }
                let mut slots = Vec::with_capacity(len);
                for _ in 0..len {
                    slots.push(r.get_u16()?);
                }
                ServerPacket::ToolDurability { slots }
            }
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

#[cfg(test)]
mod tests {
    use super::{ClientPacket, ServerPacket};

    #[test]
    fn hello_carries_anonymous_player_id() {
        let packet = ClientPacket::Hello {
            version: crate::PROTOCOL_VERSION,
            player_id: [0x5a; 16],
            name: "anonymous".into(),
        };
        assert_eq!(ClientPacket::decode(&packet.encode()).unwrap(), packet);
    }

    #[test]
    fn craft_recipe_survives_wire_round_trip() {
        let packet = ClientPacket::Craft { recipe: 3 };
        assert_eq!(ClientPacket::decode(&packet.encode()).unwrap(), packet);
    }

    #[test]
    fn drop_selected_survives_wire_round_trip() {
        let packet = ClientPacket::DropSelected;
        assert_eq!(ClientPacket::decode(&packet.encode()).unwrap(), packet);
    }

    #[test]
    fn tool_durability_sync_requires_exact_inventory_size() {
        let mut slots = vec![0; 36];
        slots[3] = 17;
        let packet = ServerPacket::ToolDurability { slots };
        assert_eq!(ServerPacket::decode(&packet.encode()).unwrap(), packet);
        assert!(
            ServerPacket::decode(&ServerPacket::ToolDurability { slots: vec![5] }.encode())
                .is_err()
        );
    }

    #[test]
    fn entity_kind_survives_wire_round_trip() {
        for kind in 0..=2 {
            let packet = ServerPacket::EntityState {
                id: 10_000,
                x: -12.5,
                y: 64.0,
                z: 9.25,
                yaw: 1.5,
                pitch: -0.25,
                kind,
            };
            assert_eq!(ServerPacket::decode(&packet.encode()).unwrap(), packet);
        }
    }

    #[test]
    fn item_state_survives_wire_round_trip() {
        let packet = ServerPacket::ItemState {
            id: 42_000,
            item: 102,
            count: 3,
            x: -1.5,
            y: 63.0,
            z: 2.25,
        };
        assert_eq!(ServerPacket::decode(&packet.encode()).unwrap(), packet);
    }

    #[test]
    fn status_carries_hunger_effect_duration() {
        let packet = ServerPacket::Status {
            health: 17.0,
            hunger: 14,
            air: 300,
            hunger_effect_ticks: 417,
        };
        assert_eq!(ServerPacket::decode(&packet.encode()).unwrap(), packet);
    }
}
