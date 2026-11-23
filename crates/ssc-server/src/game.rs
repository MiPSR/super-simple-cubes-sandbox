//! The authoritative game server.
//!
//! Owns the world, simulates every connected player at 20 TPS, streams
//! columns + LOD data per player, performs dig/place raycasts and emits
//! sound triggers — everything the clients know comes from here
//! (`specs.md` §2). Multi-player: one shared world, one session per
//! client wire.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ssc_math::{
    look_dir, yaw_forward, yaw_right, Vec3, GRAVITY, JUMP_VELOCITY, PLAYER_EYE_HEIGHT, REACH,
    SNEAK_SPEED, SPRINT_SPEED, TERMINAL_VELOCITY, WALK_SPEED,
};
use ssc_protocol::packets::{keys, ClientPacket, ServerPacket, SoundId};
use ssc_protocol::wire::{memory_pair, MemoryWire, Wire};
use ssc_protocol::PROTOCOL_VERSION;
use ssc_world::{is_solid, BlockId, ColumnKey, World, AIR, SAND, SANDSTONE, SNOW_BLOCK, WATER};

use crate::physics::{entity_aabb, move_player, player_aabb};
use crate::raycast::raycast;
use crate::worldgen::WorldGen;

/// Mobs occupy ids from here upward (players are 1..).
const FIRST_MOB_ID: EntityId = 10_000;
/// Dropped-item ids occupy a separate range when possible.
const FIRST_ITEM_ID: EntityId = 40_000;
/// Maximum mobs within a player's spawn range (per player).
const MOB_CAP_PER_PLAYER: usize = 6;
/// Ordinary melee attacks are shorter-ranged than block interactions.
const MELEE_REACH: f32 = 3.0;
const MIN_MOB_SPAWN_DISTANCE: f32 = 24.0;
/// Fixed simulation-distance-four spawning/despawning for now.
const MAX_MOB_DISTANCE: f32 = 44.0;
const ITEM_LIFETIME_TICKS: u32 = 6_000;
const ARROW_DAMAGE: f32 = 4.0;
const ARROW_SPEED: f32 = 30.0;
const ARROW_LIFETIME_TICKS: u8 = 60;
const BOW_COOLDOWN_TICKS: u8 = 10;

/// Server tick rate (matches Minecraft's 20 TPS).
pub const TICK_RATE: f32 = 20.0;
const TICK_LEN: Duration = Duration::from_millis(50);

/// Fixed hotbar for v0 (server-driven; inventory system comes later).
pub const HOTBAR: [BlockId; 9] = [
    ssc_world::BRICKS,
    ssc_world::OAK_PLANKS,
    ssc_world::STONE,
    ssc_world::COBBLESTONE,
    SAND,
    SANDSTONE,
    ssc_world::GRAVEL,
    ssc_world::OAK_LOG,
    SNOW_BLOCK,
];

/// Game mode. Protocol wire values live in `ssc_protocol::packets::game_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameMode {
    Survival,
    Creative,
}

impl GameMode {
    pub fn as_u8(self) -> u8 {
        match self {
            GameMode::Survival => ssc_protocol::packets::game_mode::SURVIVAL,
            GameMode::Creative => ssc_protocol::packets::game_mode::CREATIVE,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "survival" | "s" | "0" => Some(GameMode::Survival),
            "creative" | "c" | "1" => Some(GameMode::Creative),
            _ => None,
        }
    }
}

/// Creative default hotbar (block items).
const CREATIVE_DEFAULT: [BlockId; 9] = HOTBAR;

/// Default full-detail render distance (columns).
pub const DEFAULT_RENDER_DISTANCE: i32 = 8;
/// Default LOD distance (columns) — the far ring streamed as summaries.
pub const DEFAULT_LOD_DISTANCE: i32 = 40;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub seed: u64,
    /// Full-detail column streaming radius.
    pub render_distance: i32,
    /// LOD summary streaming radius.
    pub lod_distance: i32,
    /// Game mode for new players.
    pub game_mode: GameMode,
    /// Optional save path. `None` keeps test/embedded sessions ephemeral.
    pub world_path: Option<PathBuf>,
    /// Local (in-process) mode: burst the initial stream instead of pacing
    /// it (memory wire has no bandwidth concerns).
    pub local: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            seed: 20261118,
            render_distance: DEFAULT_RENDER_DISTANCE,
            lod_distance: DEFAULT_LOD_DISTANCE,
            game_mode: GameMode::Survival,
            world_path: None,
            local: false,
        }
    }
}

/// Entity id on the wire.
pub type EntityId = u16;

/// One connected player.
struct Player {
    name: String,
    /// Feet-center position (authoritative).
    pos: Vec3,
    vel: Vec3,
    yaw: f32,
    pitch: f32,
    keys: u16,
    /// Creative flight is authoritative and toggled by a server-detected
    /// double jump. `last_jump_tick` only tracks rising edges.
    flying: bool,
    last_jump_tick: Option<u64>,
    selected_slot: u8,
    on_ground: bool,
    in_water: bool,
    joined: bool,
    /// Ticks until the player can fire the bow again.
    bow_cooldown: u8,
}

/// A client connection: wire + player + its own streaming state.
struct Session {
    wire: Box<dyn Wire>,
    player: Player,
    /// Stable anonymous identity supplied by the client (not authenticated).
    player_id: Option<[u8; 16]>,
    /// A corrupt player save is preserved rather than overwritten.
    player_data_writable: bool,
    /// The player's inventory (36 slots; 0..9 = hotbar).
    inventory: Vec<crate::survival::Slot>,
    /// Remaining tool uses per inventory slot; zero for other items.
    durability: Vec<u16>,
    /// Helmet, chestplate, leggings, boots.
    armor: [crate::survival::Slot; crate::survival::ARMOR_SIZE],
    armor_durability: [u16; crate::survival::ARMOR_SIZE],
    armor_dirty: bool,
    xp_level: u8,
    xp_progress: u16,
    /// Survival-mode state (ignored in creative).
    survival: crate::survival::SurvivalState,
    /// In-progress use is transient, never restored from a player save.
    eating: Option<EatingState>,
    last_eating_sync: u8,
    open_furnace: Option<(i32, i32, i32)>,
    open_chest: Option<(i32, i32, i32)>,
    /// Whether this death already emitted inventory drops.
    inventory_dropped_on_death: bool,
    /// Respawn point (set at join).
    spawn: Vec3,
    /// Sync flags: send Inventory/Status when dirty.
    inv_dirty: bool,
    status_dirty: bool,
    /// Full columns queued to stream, nearest first.
    stream_queue: VecDeque<ColumnKey>,
    streamed: HashSet<ColumnKey>,
    /// LOD columns queued to stream.
    lod_queue: VecDeque<ColumnKey>,
    lod_streamed: HashSet<ColumnKey>,
    last_player_col: ColumnKey,
}

struct EatingState {
    slot: u8,
    item: ssc_world::ItemId,
    remaining: u8,
}

/// One dropped stack. Gameplay and pickup are entirely server-owned.
struct ItemEntity {
    item: ssc_world::ItemId,
    count: u8,
    durability: u16,
    pos: Vec3,
    vel: Vec3,
    age: u32,
    pickup_delay: u8,
}

/// A server-owned arrow projectile. Not persisted across restarts.
struct ArrowEntity {
    owner: EntityId,
    pos: Vec3,
    vel: Vec3,
    age: u8,
}

impl Session {
    fn new(wire: Box<dyn Wire>, mode: GameMode) -> Self {
        // Creative starts with the default hotbar blocks (64× each);
        // survival starts empty.
        let inventory: Vec<crate::survival::Slot> = if mode == GameMode::Creative {
            CREATIVE_DEFAULT
                .iter()
                .map(|&b| Some((b, 64)))
                .chain(std::iter::repeat(None).take(27))
                .collect()
        } else {
            vec![None; crate::survival::INVENTORY_SIZE]
        };
        Self {
            wire,
            player_id: None,
            player_data_writable: true,
            player: Player {
                name: String::new(),
                pos: Vec3::new(8.5, 128.0, 8.5),
                vel: Vec3::ZERO,
                yaw: 0.0,
                pitch: 0.0,
                keys: 0,
                flying: false,
                last_jump_tick: None,
                selected_slot: 0,
                on_ground: false,
                in_water: false,
                joined: false,
                bow_cooldown: 0,
            },
            inventory,
            durability: vec![0; crate::survival::INVENTORY_SIZE],
            armor: [None; crate::survival::ARMOR_SIZE],
            armor_durability: [0; crate::survival::ARMOR_SIZE],
            armor_dirty: true,
            xp_level: 0,
            xp_progress: 0,
            survival: crate::survival::SurvivalState::default(),
            eating: None,
            last_eating_sync: 0,
            open_furnace: None,
            open_chest: None,
            inventory_dropped_on_death: false,
            spawn: Vec3::new(8.5, 128.0, 8.5),
            inv_dirty: true,
            status_dirty: true,
            stream_queue: VecDeque::new(),
            streamed: HashSet::new(),
            lod_queue: VecDeque::new(),
            lod_streamed: HashSet::new(),
            last_player_col: (i32::MAX, i32::MAX),
        }
    }
}

/// The game server: one shared world, one session per client.
pub struct Server {
    cfg: ServerConfig,
    world: World,
    gen: WorldGen,
    edited: HashSet<ColumnKey>,
    block_edits: crate::persistence::BlockEdits,
    persistence_ready: bool,
    item_persistence_ready: bool,
    mob_persistence_ready: bool,
    furnace_persistence_ready: bool,
    furnaces: crate::persistence::FurnaceSaves,
    chest_persistence_ready: bool,
    chests: crate::persistence::ChestSaves,
    sessions: HashMap<EntityId, Session>,
    /// Mobs (ids from `FIRST_MOB_ID`).
    mobs: HashMap<EntityId, crate::mobs::Mob>,
    items: HashMap<EntityId, ItemEntity>,
    arrows: HashMap<EntityId, ArrowEntity>,
    next_id: EntityId,
    next_mob_id: EntityId,
    next_item_id: EntityId,
    /// Dedicated mode: accepted wires land here and are attached on tick.
    accept_rx: Option<std::sync::mpsc::Receiver<Box<dyn Wire>>>,
    tick: u64,
    /// Time of day, 0..1 (0 = dawn).
    time_of_day: f32,
    running: bool,
}

impl Server {
    pub fn new(cfg: ServerConfig, wire: Box<dyn Wire>) -> Self {
        let mut server = Self::empty(cfg);
        server.attach(wire);
        server
    }

    /// A dedicated (wireless) server: attach clients via
    /// [`Server::set_accept_channel`] and [`Server::attach`].
    pub fn empty_for_dedicated(cfg: ServerConfig) -> Self {
        Self::empty(cfg)
    }

    fn empty(cfg: ServerConfig) -> Self {
        let mut cfg = cfg;
        let (seed, time_of_day, block_edits, persistence_ready) = match cfg.world_path.as_deref() {
            Some(path) => match crate::persistence::load(path) {
                Ok(Some((seed, time_of_day, edits))) => {
                    eprintln!(
                        "[server] loaded seed {seed}, time {time_of_day:.3} and {} block edits from {}",
                        edits.values().map(HashMap::len).sum::<usize>(),
                        path.display()
                    );
                    (seed, time_of_day, edits, true)
                }
                Ok(None) => (cfg.seed, 0.3, HashMap::new(), true),
                Err(err) => {
                    eprintln!(
                        "[server] could not load {}: {err}; saves disabled to protect it",
                        path.display()
                    );
                    cfg.world_path = None;
                    (cfg.seed, 0.3, HashMap::new(), false)
                }
            },
            None => (cfg.seed, 0.3, HashMap::new(), true),
        };
        let (saved_items, item_persistence_ready) = match cfg.world_path.as_deref() {
            Some(path) => {
                match crate::persistence::load_items(&crate::persistence::item_save_path(path)) {
                    Ok(items) => (items.unwrap_or_default(), true),
                    Err(err) => {
                        eprintln!(
                            "[server] dropped-item save could not be loaded: {err}; preserving it"
                        );
                        (Vec::new(), false)
                    }
                }
            }
            None => (Vec::new(), false),
        };
        let (saved_mobs, mob_persistence_ready) = match cfg.world_path.as_deref() {
            Some(path) => {
                match crate::persistence::load_mobs(&crate::persistence::mob_save_path(path)) {
                    Ok(mobs) => (mobs.unwrap_or_default(), true),
                    Err(err) => {
                        eprintln!("[server] mob save could not be loaded: {err}; preserving it");
                        (Vec::new(), false)
                    }
                }
            }
            None => (Vec::new(), false),
        };
        let edited = block_edits.keys().copied().collect();
        let (furnaces, furnace_persistence_ready) = match cfg.world_path.as_deref() {
            Some(path) => match crate::persistence::load_furnaces(
                &crate::persistence::furnace_save_path(path),
            ) {
                Ok(data) => {
                    let data = data.unwrap_or_default();
                    let consistent = data.keys().all(|&(x, y, z)| {
                        block_edits
                            .get(&(x.div_euclid(16), z.div_euclid(16)))
                            .and_then(|edits| edits.get(&(x.rem_euclid(16), y, z.rem_euclid(16))))
                            .is_some_and(|block| ssc_world::is_furnace(*block))
                    });
                    if consistent {
                        (data, true)
                    } else {
                        eprintln!(
                            "[server] furnace save does not match world edits; preserving it"
                        );
                        (HashMap::new(), false)
                    }
                }
                Err(error) => {
                    eprintln!("[server] furnace save could not be loaded: {error}; preserving it");
                    (HashMap::new(), false)
                }
            },
            None => (HashMap::new(), false),
        };
        let (chests, chest_persistence_ready) = match cfg.world_path.as_deref() {
            Some(path) => {
                match crate::persistence::load_chests(&crate::persistence::chest_save_path(path)) {
                    Ok(data) => {
                        let data = data.unwrap_or_default();
                        let consistent = data.keys().all(|&(x, y, z)| {
                            block_edits
                                .get(&(x.div_euclid(16), z.div_euclid(16)))
                                .and_then(|edits| {
                                    edits.get(&(x.rem_euclid(16), y, z.rem_euclid(16)))
                                })
                                == Some(&ssc_world::CHEST)
                        });
                        if consistent {
                            (data, true)
                        } else {
                            eprintln!(
                                "[server] chest save does not match world edits; preserving it"
                            );
                            (HashMap::new(), false)
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "[server] chest save could not be loaded: {error}; preserving it"
                        );
                        (HashMap::new(), false)
                    }
                }
            }
            None => (HashMap::new(), false),
        };
        let mut server = Self {
            gen: WorldGen::new(seed),
            world: World::default(),
            edited,
            block_edits,
            persistence_ready,
            item_persistence_ready,
            mob_persistence_ready,
            furnace_persistence_ready,
            furnaces,
            chest_persistence_ready,
            chests,
            cfg,
            sessions: HashMap::new(),
            mobs: HashMap::new(),
            items: HashMap::new(),
            arrows: HashMap::new(),
            next_id: 1,
            next_mob_id: FIRST_MOB_ID,
            next_item_id: FIRST_ITEM_ID,
            accept_rx: None,
            tick: 0,
            time_of_day, // start in the morning for a new world
            running: true,
        };
        for saved in saved_items {
            let Some(id) = server.allocate_item_id() else {
                eprintln!("[server] item entity capacity exhausted while loading save");
                break;
            };
            server.items.insert(
                id,
                ItemEntity {
                    item: saved.item,
                    count: saved.count,
                    durability: saved.durability,
                    pos: Vec3::new(saved.pos[0], saved.pos[1], saved.pos[2]),
                    vel: Vec3::new(saved.vel[0], saved.vel[1], saved.vel[2]),
                    age: saved.age,
                    pickup_delay: saved.pickup_delay,
                },
            );
        }
        let mut mob_columns = HashSet::new();
        for saved in saved_mobs {
            let Some(id) = server.allocate_mob_id() else {
                eprintln!("[server] mob entity capacity exhausted while loading save");
                break;
            };
            let Some(kind) = crate::mobs::MobKind::from_u8(saved.kind) else {
                continue;
            };
            let pos = Vec3::new(saved.pos[0], saved.pos[1], saved.pos[2]);
            let mut mob = crate::mobs::Mob::new(kind, pos);
            mob.vel = Vec3::new(saved.vel[0], saved.vel[1], saved.vel[2]);
            mob.yaw = saved.yaw;
            mob.health = saved.health;
            mob.wander_timer = saved.wander_timer;
            mob.wander_dir = saved
                .wander_dir
                .map(|dir| ssc_math::Vec2::new(dir[0], dir[1]));
            mob.attack_cooldown = saved.attack_cooldown;
            mob.burn_timer = saved.burn_timer;
            mob.panic_timer = saved.panic_timer;
            mob.on_ground_hint = saved.on_ground;
            let column = (
                (pos.x.floor() as i32).div_euclid(16),
                (pos.z.floor() as i32).div_euclid(16),
            );
            for dx in -1..=1 {
                for dz in -1..=1 {
                    mob_columns.insert((column.0 + dx, column.1 + dz));
                }
            }
            server.mobs.insert(id, mob);
        }
        for key in mob_columns {
            server.ensure_column(key);
        }
        server
    }

    /// Register a client connection.
    pub fn attach(&mut self, wire: Box<dyn Wire>) -> EntityId {
        let id = self.next_id;
        self.next_id += 1;
        self.sessions
            .insert(id, Session::new(wire, self.cfg.game_mode));
        id
    }

    /// Dedicated mode: hook the accept channel; new wires become sessions.
    pub fn set_accept_channel(&mut self, rx: std::sync::mpsc::Receiver<Box<dyn Wire>>) {
        self.accept_rx = Some(rx);
    }

    fn save_world(&self) {
        if !self.persistence_ready {
            return;
        }
        let Some(path) = self.cfg.world_path.as_deref() else {
            return;
        };
        if let Err(err) =
            crate::persistence::save(path, self.gen.seed(), self.time_of_day, &self.block_edits)
        {
            eprintln!("[server] world save to {} failed: {err}", path.display());
        }
    }

    fn save_item_entities(&self) {
        if !self.item_persistence_ready {
            return;
        }
        let Some(world_path) = self.cfg.world_path.as_deref() else {
            return;
        };
        let items: Vec<_> = self
            .items
            .values()
            .map(|item| crate::persistence::SavedItem {
                item: item.item,
                count: item.count,
                durability: item.durability,
                pos: [item.pos.x, item.pos.y, item.pos.z],
                vel: [item.vel.x, item.vel.y, item.vel.z],
                age: item.age,
                pickup_delay: item.pickup_delay,
            })
            .collect();
        let path = crate::persistence::item_save_path(world_path);
        if let Err(err) = crate::persistence::save_items(&path, &items) {
            eprintln!(
                "[server] dropped-item save to {} failed: {err}",
                path.display()
            );
        }
    }

    fn save_mobs(&self) {
        if !self.mob_persistence_ready {
            return;
        }
        let Some(world_path) = self.cfg.world_path.as_deref() else {
            return;
        };
        let mobs: Vec<_> = self
            .mobs
            .values()
            .filter(|mob| !mob.dead)
            .map(|mob| crate::persistence::SavedMob {
                kind: mob.kind.as_u8(),
                pos: [mob.pos.x, mob.pos.y, mob.pos.z],
                vel: [mob.vel.x, mob.vel.y, mob.vel.z],
                yaw: mob.yaw,
                health: mob.health,
                wander_timer: mob.wander_timer,
                wander_dir: mob.wander_dir.map(|dir| [dir.x, dir.y]),
                attack_cooldown: mob.attack_cooldown,
                burn_timer: mob.burn_timer,
                panic_timer: mob.panic_timer,
                on_ground: mob.on_ground_hint,
                sheared: mob.sheared,
            })
            .collect();
        let path = crate::persistence::mob_save_path(world_path);
        if let Err(err) = crate::persistence::save_mobs(&path, &mobs) {
            eprintln!("[server] mob save to {} failed: {err}", path.display());
        }
    }

    fn load_player_save(
        &self,
        player_id: [u8; 16],
    ) -> std::io::Result<Option<crate::persistence::PlayerSave>> {
        let Some(world_path) = self.cfg.world_path.as_deref() else {
            return Ok(None);
        };
        let path = crate::persistence::player_path(world_path, &player_id);
        crate::persistence::load_player(&path, &player_id)
    }

    fn save_player_data(&self, entity_id: EntityId) {
        let Some(session) = self.sessions.get(&entity_id) else {
            return;
        };
        if !session.player_data_writable {
            return;
        }
        let (Some(world_path), Some(player_id)) =
            (self.cfg.world_path.as_deref(), session.player_id.as_ref())
        else {
            return;
        };
        let path = crate::persistence::player_path(world_path, player_id);
        let (position, health, hunger, air) = if session.survival.dead {
            (
                Some([session.spawn.x, session.spawn.y, session.spawn.z]),
                crate::survival::MAX_HEALTH,
                crate::survival::MAX_HUNGER,
                crate::survival::MAX_AIR,
            )
        } else {
            // A player may disconnect during a fall through the void. That
            // position cannot be restored (or encoded by PlayerSave), so
            // retain their current vitals but resume at the safe spawn.
            let pos = if session.player.pos.y < ssc_world::MIN_Y as f32 {
                session.spawn
            } else {
                session.player.pos
            };
            (
                Some([pos.x, pos.y, pos.z]),
                session.survival.health,
                session.survival.hunger,
                session.survival.air,
            )
        };
        let data = crate::persistence::PlayerSave {
            inventory: session.inventory.clone(),
            durability: session.durability.clone(),
            position,
            health,
            hunger,
            saturation: if session.survival.dead {
                crate::survival::INITIAL_SATURATION
            } else {
                session.survival.saturation
            },
            air,
            selected_slot: session.player.selected_slot,
            hunger_effect_ticks: if session.survival.dead {
                0
            } else {
                session.survival.hunger_effect_ticks
            },
            exhaustion: if session.survival.dead {
                0.0
            } else {
                session.survival.exhaustion
            },
            respawn: Some([session.spawn.x, session.spawn.y, session.spawn.z]),
            armor: session.armor.to_vec(),
            armor_durability: session.armor_durability.to_vec(),
            xp_level: session.xp_level,
            xp_progress: session.xp_progress,
        };
        if let Err(err) = crate::persistence::save_player(&path, player_id, &data) {
            eprintln!("[server] could not save anonymous player data: {err}");
        }
    }

    fn save_all_player_data(&self) {
        for id in self.sessions.keys().copied() {
            self.save_player_data(id);
        }
    }

    fn save_furnaces(&self) {
        if !self.furnace_persistence_ready {
            return;
        }
        let Some(path) = self.cfg.world_path.as_deref() else {
            return;
        };
        if let Err(error) = crate::persistence::save_furnaces(
            &crate::persistence::furnace_save_path(path),
            &self.furnaces,
        ) {
            eprintln!("[server] furnace save failed: {error}");
        }
    }

    fn save_chests(&self) {
        if !self.chest_persistence_ready {
            return;
        }
        let Some(path) = self.cfg.world_path.as_deref() else {
            return;
        };
        if let Err(error) = crate::persistence::save_chests(
            &crate::persistence::chest_save_path(path),
            &self.chests,
        ) {
            eprintln!("[server] chest save failed: {error}");
        }
    }

    fn record_block_edit(&mut self, x: i32, y: i32, z: i32, block: BlockId) {
        let key = (x.div_euclid(16), z.div_euclid(16));
        let local = (x.rem_euclid(16), y, z.rem_euclid(16));
        self.block_edits
            .entry(key)
            .or_default()
            .insert(local, block);
        self.edited.insert(key);
    }

    /// Current time of day, 0..1 (0 = dawn). Minecraft pace: 24000
    /// ticks per cycle (~20 min at 20 TPS).
    pub fn time_of_day(&self) -> f32 {
        self.time_of_day
    }

    /// Run until every client is gone. Call from a dedicated thread.
    pub fn run(mut self) {
        let mut next_tick = Instant::now();
        while self.running {
            // Attach any freshly accepted connections.
            if let Some(rx) = self.accept_rx.take() {
                while let Ok(wire) = rx.try_recv() {
                    self.attach(wire);
                }
                self.accept_rx = Some(rx);
            }

            // Drain every session's incoming packets (wires only, so the
            // world stays borrowable afterwards).
            let mut pending: Vec<(EntityId, ClientPacket)> = Vec::new();
            for (id, session) in &mut self.sessions {
                while let Some(bytes) = session.wire.try_recv() {
                    match ClientPacket::decode(&bytes) {
                        Ok(pkt) => pending.push((*id, pkt)),
                        Err(e) => {
                            // Malformed input: stop reading this client; the
                            // reaper will drop the session once the wire
                            // goes quiet.
                            eprintln!("[server] dropping client {id}: {e}");
                            break;
                        }
                    }
                }
            }

            // Apply all packets.
            for (id, pkt) in pending {
                let _ = self.handle_packet(id, pkt);
            }

            self.tick();

            // Retire dead sessions.
            let dead: Vec<EntityId> = self
                .sessions
                .iter()
                .filter(|(_, s)| s.player.joined && !s.wire.is_alive())
                .map(|(id, _)| *id)
                .collect();
            for id in dead {
                eprintln!("[server] client {id} disconnected");
                self.save_player_data(id);
                self.sessions.remove(&id);
                let _ = self.broadcast(&ServerPacket::EntityRemove { id });
            }
            // Local (embedded) servers exit when the single client leaves;
            // dedicated servers stay up for the next player.
            if self.cfg.local && self.sessions.is_empty() {
                break;
            }

            next_tick += TICK_LEN;
            let now = Instant::now();
            if next_tick > now {
                std::thread::sleep(next_tick - now);
            } else {
                next_tick = now; // we're behind; don't accumulate debt
            }
        }
        self.save_all_player_data();
        self.save_item_entities();
        self.save_mobs();
        self.save_furnaces();
        self.save_chests();
        self.save_world();
        let _ = self.broadcast(&ServerPacket::Goodbye {
            reason: "server closed".into(),
        });
    }

    fn tick(&mut self) {
        self.tick += 1;
        let dt = 1.0 / TICK_RATE;
        self.tick_eating();
        self.tick_furnaces();
        self.tick_chests();

        // --- Movement + survival simulation + dig progress, per session.
        // Effects are collected first, then applied (broadcasts need &mut
        // self while the loop borrows sessions).
        struct PlayerFx {
            id: EntityId,
            pos: (f32, f32, f32),
            hurt: bool,
            dig_broadcast: Option<(i32, i32, i32, u8)>,
            broke_block: Option<(i32, i32, i32, BlockId)>,
        }
        let mut fxs: Vec<PlayerFx> = Vec::new();

        // Respawn may teleport into a column that is no longer streamed.
        let respawn_columns: Vec<ColumnKey> = self
            .sessions
            .values()
            .filter(|session| {
                session.player.joined && session.survival.dead && session.survival.should_respawn()
            })
            .map(|session| {
                (
                    (session.spawn.x.floor() as i32).div_euclid(16),
                    (session.spawn.z.floor() as i32).div_euclid(16),
                )
            })
            .collect();
        for key in respawn_columns {
            self.ensure_column(key);
        }

        for (id, session) in self.sessions.iter_mut() {
            let p = &mut session.player;
            if !p.joined {
                continue;
            }
            p.bow_cooldown = p.bow_cooldown.saturating_sub(1);
            // Dead players lie still until respawn (auto, ~2 s).
            if session.survival.dead {
                if session.survival.should_respawn() {
                    let spawn = session.spawn;
                    p.pos = spawn;
                    p.vel = Vec3::ZERO;
                    session.survival.respawn();
                    session.inventory_dropped_on_death = false;
                    session.status_dirty = true;
                    fxs.push(PlayerFx {
                        id: *id,
                        pos: (spawn.x, spawn.y, spawn.z),
                        hurt: false,
                        dig_broadcast: None,
                        broke_block: None,
                    });
                }
                continue;
            }

            let was_on_ground = p.on_ground;
            let old_pos = p.pos;
            let old_y = p.pos.y;
            let jumped = p.keys & keys::JUMP != 0 && p.on_ground;
            let flying = p.flying;
            let can_sprint = self.cfg.game_mode == GameMode::Creative
                || (session.survival.hunger >= crate::survival::MIN_SPRINT_HUNGER
                    && session.eating.is_none());
            let sprinting = can_sprint
                && p.keys & keys::SPRINT != 0
                && p.keys & keys::FORWARD != 0
                && p.keys & keys::SNEAK == 0;
            step_player(
                &self.world,
                p,
                dt,
                flying,
                sprinting,
                session.eating.is_some(),
            );
            let dx = p.pos.x - old_pos.x;
            let dz = p.pos.z - old_pos.z;

            // Survival vitals (one tick).
            let eye = p.pos + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
            let eye_in_water = self.world.block_or_air(
                eye.x.floor() as i32,
                eye.y.floor() as i32,
                eye.z.floor() as i32,
            ) == WATER;
            let survival = self.cfg.game_mode == GameMode::Survival;
            let fx_tick = if survival {
                session.survival.tick(crate::survival::TickInput {
                    landed: !was_on_ground && p.on_ground && p.pos.y >= ssc_world::MIN_Y as f32,
                    in_water: p.in_water,
                    eye_in_water,
                    below_world: p.pos.y < ssc_world::MIN_Y as f32,
                    horizontal_distance: (dx * dx + dz * dz).sqrt(),
                    sprinting,
                    jumped,
                    dy: p.pos.y - old_y,
                })
            } else {
                Default::default()
            };
            if fx_tick.health_changed
                || fx_tick.hunger_changed
                || fx_tick.air_changed
                || fx_tick.effect_changed
            {
                session.status_dirty = true;
            }

            // Dig progress (survival): the target must stay in reach and
            // remain the block we started on; raycast each tick.
            let mut dig_broadcast = None;
            let mut broke_block = None;
            if let Some(dig) = &mut session.survival.digging {
                let held = session.inventory[p.selected_slot as usize % 9].map(|(item, _)| item);
                let dir = look_dir(p.yaw, p.pitch);
                let eye = p.pos + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
                let same_target = raycast(&self.world, eye, dir, REACH)
                    .map(|hit| hit.block == [dig.x, dig.y, dig.z])
                    .unwrap_or(false);
                if !same_target {
                    // Target changed (look moved / block broke): re-target
                    // like Minecraft — progress restarts on the new block.
                    match raycast(&self.world, eye, dir, REACH) {
                        Some(hit)
                            if crate::survival::dig_time_with_tool(
                                self.world
                                    .block_or_air(hit.block[0], hit.block[1], hit.block[2]),
                                held,
                            )
                            .is_some() =>
                        {
                            *dig = crate::survival::DigState {
                                x: hit.block[0],
                                y: hit.block[1],
                                z: hit.block[2],
                                progress: 0.0,
                            };
                        }
                        _ => {
                            dig_broadcast = Some((dig.x, dig.y, dig.z, 101));
                            session.survival.digging = None;
                        }
                    }
                } else {
                    let block = self.world.block_or_air(dig.x, dig.y, dig.z);
                    let time = crate::survival::dig_time_with_penalties(
                        block,
                        held,
                        eye_in_water,
                        p.on_ground,
                    );
                    match time {
                        Some(time) => {
                            dig.progress += dt / time;
                            if dig.progress >= 1.0 {
                                broke_block = Some((dig.x, dig.y, dig.z, block));
                                session.survival.digging = None;
                            } else if self.tick % 4 == 0 {
                                dig_broadcast =
                                    Some((dig.x, dig.y, dig.z, (dig.progress * 100.0) as u8));
                            }
                        }
                        None => {
                            // Unbreakable (bedrock/air): cancel.
                            dig_broadcast = Some((dig.x, dig.y, dig.z, 101));
                            session.survival.digging = None;
                        }
                    }
                }
            }

            fxs.push(PlayerFx {
                id: *id,
                pos: (p.pos.x, p.pos.y, p.pos.z),
                hurt: fx_tick.hurt,
                dig_broadcast,
                broke_block,
            });
        }

        // --- Apply effects (broadcasts + damage sounds + block breaks).
        for fx in fxs {
            if fx.hurt {
                let (x, y, z) = fx.pos;
                self.send_sound(
                    SoundId::Hurt,
                    x.floor() as i32,
                    y.floor() as i32,
                    z.floor() as i32,
                );
            }
            if let Some((x, y, z, progress)) = fx.dig_broadcast {
                let _ = self.broadcast(&ServerPacket::DigProgress { x, y, z, progress });
            }
            if let Some((x, y, z, _block)) = fx.broke_block {
                self.break_block(fx.id, x, y, z);
            }
        }

        // Footsteps: server-triggered like every other sound (specs.md §2).
        if self.tick % 4 == 0 {
            let mut steps: Vec<(f32, f32, f32)> = Vec::new();
            for session in self.sessions.values() {
                let p = &session.player;
                let moving =
                    p.keys & (keys::FORWARD | keys::BACKWARD | keys::LEFT | keys::RIGHT) != 0;
                if p.joined && p.on_ground && moving && !session.survival.dead {
                    steps.push((p.pos.x, p.pos.y, p.pos.z));
                }
            }
            for (x, y, z) in steps {
                let _ = self.broadcast(&ServerPacket::Sound {
                    sound: SoundId::Step.to_u16(),
                    x,
                    y,
                    z,
                });
            }
        }

        // --- Entity states (own id included; clients interpolate).
        let states: Vec<ServerPacket> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.player.joined)
            .map(|(id, s)| entity_state(*id, &s.player))
            .chain(self.mobs.iter().map(|(id, m)| mob_state(*id, m)))
            .chain(self.items.iter().map(|(id, item)| item_state(*id, item)))
            .collect();
        for pkt in &states {
            let _ = self.broadcast(pkt);
        }

        // --- Mobs: simulate, attack players, die, despawn, spawn.
        {
            // Player positions for the AI (zombies chase, pigs flee
            // nothing yet).
            let players: Vec<(EntityId, Vec3)> = self
                .sessions
                .iter()
                .filter(|(_, s)| s.player.joined && !s.survival.dead)
                .map(|(id, s)| (*id, s.player.pos))
                .collect();
            let positions: Vec<Vec3> = players.iter().map(|(_, p)| *p).collect();

            // Simulate.
            let mut mob_hits: Vec<(EntityId, usize)> = Vec::new();
            let mut dead_mobs: Vec<EntityId> = Vec::new();
            let daylight = self.time_of_day > 0.25 && self.time_of_day < 0.75;
            if !positions.is_empty() {
                let mob_columns: HashSet<ColumnKey> = self
                    .mobs
                    .values()
                    .flat_map(|mob| {
                        let cx = (mob.pos.x.floor() as i32).div_euclid(16);
                        let cz = (mob.pos.z.floor() as i32).div_euclid(16);
                        (-1..=1).flat_map(move |dx| (-1..=1).map(move |dz| (cx + dx, cz + dz)))
                    })
                    .collect();
                for key in mob_columns {
                    self.ensure_column(key);
                }
                for (id, mob) in self.mobs.iter_mut() {
                    if let Some(idx) = mob.tick(&self.world, &positions, dt, daylight) {
                        mob_hits.push((*id, idx));
                    }
                    if mob.dead {
                        dead_mobs.push(*id);
                    }
                }
            }

            // Zombie hits land on players (survival damage + sound).
            for (mob_id, idx) in mob_hits {
                let Some((player_id, _)) = players.get(idx) else {
                    continue;
                };
                if self.cfg.game_mode != GameMode::Survival {
                    continue;
                }
                let damage = self.mobs[&mob_id].kind.attack_damage();
                let from = self.mobs[&mob_id].pos;
                let mut fx = crate::survival::TickEffects::default();
                let session = self.sessions.get_mut(player_id).unwrap();
                let points = crate::survival::armor_points(&session.armor);
                let damage = damage * (1.0 - crate::survival::armor_reduction(points));
                session.survival.apply_combat_damage(damage, &mut fx);
                if fx.hurt {
                    session.survival.exhaustion += 0.1;
                    damage_armor(session, 1);
                }
                if !fx.hurt {
                    continue;
                }
                knock_player_back(&mut session.player, from);
                session.status_dirty = true;
                let pos = session.player.pos;
                self.send_sound(
                    SoundId::Hurt,
                    pos.x.floor() as i32,
                    pos.y.floor() as i32,
                    pos.z.floor() as i32,
                );
            }

            // Dead mobs: spawn loot entities, then remove the mob.
            for mob_id in dead_mobs {
                let (kind, pos, sheared) = {
                    let m = &self.mobs[&mob_id];
                    (m.kind, m.pos, m.sheared)
                };
                let drop_roll = hash_tick(self.tick, mob_id as i32, pos.x.floor() as i32);
                for (item, count) in crate::mobs::drops_for(kind, drop_roll) {
                    if sheared && item == ssc_world::WOOL {
                        continue;
                    }
                    self.spawn_item_entity(item, count, pos);
                }
                self.mobs.remove(&mob_id);
                let _ = self.broadcast(&ServerPacket::EntityRemove { id: mob_id });
            }

            // Despawn far-away mobs; remove those in nothing (void).
            let player_refs: Vec<Vec3> = positions.clone();
            let despawn: Vec<EntityId> = self
                .mobs
                .iter()
                .filter(|(_, m)| {
                    m.pos.y <= ssc_world::MIN_Y as f32 + 1.0
                        || (!player_refs.is_empty()
                            && player_refs
                                .iter()
                                .all(|p| (m.pos - *p).length() > MAX_MOB_DISTANCE))
                })
                .map(|(id, _)| *id)
                .collect();
            for id in despawn {
                self.mobs.remove(&id);
                let _ = self.broadcast(&ServerPacket::EntityRemove { id });
            }

            // Sheared sheep regrow wool by grazing grass after one minute.
            let mut grazed = Vec::new();
            {
                let world = &self.world;
                for (id, mob) in self.mobs.iter_mut() {
                    if mob.dead || mob.kind != crate::mobs::MobKind::Sheep || !mob.sheared {
                        continue;
                    }
                    mob.regrow_timer = mob.regrow_timer.saturating_add(1);
                    if mob.regrow_timer < 1200 {
                        continue;
                    }
                    let (x, z) = (mob.pos.x.floor() as i32, mob.pos.z.floor() as i32);
                    let y = mob.pos.y.floor() as i32 - 1;
                    if world.block_or_air(x, y, z) == ssc_world::GRASS_BLOCK {
                        grazed.push((*id, (x, y, z)));
                    }
                }
            }
            for (id, (x, y, z)) in grazed {
                self.world.set_block(x, y, z, ssc_world::DIRT);
                self.record_block_edit(x, y, z, ssc_world::DIRT);
                let _ = self.broadcast(&ServerPacket::BlockUpdate {
                    x,
                    y,
                    z,
                    block: ssc_world::DIRT,
                });
                self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
                if let Some(mob) = self.mobs.get_mut(&id) {
                    mob.sheared = false;
                    mob.regrow_timer = 0;
                }
            }
        }

        self.tick_arrows(dt);

        if self.cfg.game_mode == GameMode::Survival {
            let dead_players: Vec<(EntityId, Vec3, Vec<(ssc_world::ItemId, u8, u16)>)> = self
                .sessions
                .iter_mut()
                .filter(|(_, session)| {
                    session.player.joined
                        && session.survival.dead
                        && !session.inventory_dropped_on_death
                })
                .map(|(id, session)| {
                    session.inventory_dropped_on_death = true;
                    session.inv_dirty = true;
                    session.xp_level = 0;
                    session.xp_progress = 0;
                    session.status_dirty = true;
                    let drops = session
                        .inventory
                        .iter_mut()
                        .zip(&mut session.durability)
                        .filter_map(|(slot, wear)| {
                            let (item, count) = slot.take()?;
                            let remaining = crate::survival::effective_durability(item, *wear);
                            *wear = 0;
                            Some((item, count, remaining))
                        })
                        .collect();
                    (*id, session.player.pos, drops)
                })
                .collect();
            for (_id, pos, drops) in dead_players {
                for (item, count, wear) in drops {
                    self.spawn_item_entity_with_durability(item, count, pos, wear);
                }
            }
        }

        self.tick_items(dt);

        // --- Dirty syncs (inventory / status).
        let dirty: Vec<EntityId> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.player.joined && (s.inv_dirty || s.status_dirty || s.armor_dirty))
            .map(|(id, _)| *id)
            .collect();
        for id in dirty {
            let (inv_pkt, wear_pkt, armor_pkt, status_pkt) = {
                let session = self.sessions.get_mut(&id).unwrap();
                let inv_pkt = if session.inv_dirty {
                    session.inv_dirty = false;
                    Some(ServerPacket::Inventory {
                        slots: session
                            .inventory
                            .iter()
                            .map(|s| s.map(|(i, c)| (i, c)).unwrap_or((0, 0)))
                            .collect(),
                    })
                } else {
                    None
                };
                let wear_pkt = inv_pkt.as_ref().map(|_| ServerPacket::ToolDurability {
                    slots: session
                        .inventory
                        .iter()
                        .zip(&session.durability)
                        .map(|(slot, &wear)| {
                            slot.map_or(0, |(item, _)| {
                                crate::survival::effective_durability(item, wear)
                            })
                        })
                        .collect(),
                });
                let armor_pkt = if session.armor_dirty {
                    session.armor_dirty = false;
                    Some(ServerPacket::Armor {
                        slots: std::array::from_fn(|index| {
                            session.armor[index].map_or((0, 0, 0), |(item, count)| {
                                (
                                    item,
                                    count,
                                    crate::survival::effective_durability(
                                        item,
                                        session.armor_durability[index],
                                    ),
                                )
                            })
                        }),
                    })
                } else {
                    None
                };
                let status_pkt = if session.status_dirty {
                    session.status_dirty = false;
                    Some(ServerPacket::Status {
                        health: session.survival.health,
                        hunger: session.survival.hunger,
                        air: session.survival.air,
                        hunger_effect_ticks: session.survival.hunger_effect_ticks,
                        xp_level: session.xp_level,
                        xp_progress: session.xp_progress,
                        xp_needed: crate::survival::xp_needed(session.xp_level),
                    })
                } else {
                    None
                };
                (inv_pkt, wear_pkt, armor_pkt, status_pkt)
            };
            if let Some(pkt) = inv_pkt {
                let _ = self.direct(id, &pkt);
            }
            if let Some(pkt) = wear_pkt {
                let _ = self.direct(id, &pkt);
            }
            if let Some(pkt) = armor_pkt {
                let _ = self.direct(id, &pkt);
            }
            if let Some(pkt) = status_pkt {
                let _ = self.direct(id, &pkt);
            }
        }

        // Per-player streaming.
        let ids: Vec<EntityId> = self.sessions.keys().copied().collect();
        for id in ids {
            let cols_this_tick = if self.cfg.local { usize::MAX } else { 4 };
            let lods_this_tick = if self.cfg.local { usize::MAX } else { 48 };
            self.stream_to(id, cols_this_tick, lods_this_tick);
        }
        self.tick_blocks();

        // Time advances every tick; broadcast once a second.
        self.time_of_day += 1.0 / 24000.0;
        if self.time_of_day >= 1.0 {
            self.time_of_day -= 1.0;
        }
        if self.tick % 20 == 0 {
            let _ = self.broadcast(&ServerPacket::TimeOfDay {
                t: self.time_of_day,
            });
        }

        // Refresh streaming sets periodically (and right after joins).
        if self.tick % 20 == 0 {
            for id in self.sessions.keys().copied().collect::<Vec<_>>() {
                self.refresh_streaming(id);
            }
            self.evict_unused_columns();
            self.spawn_mobs();
        }
        if self.tick % 1200 == 0 {
            self.save_all_player_data();
            self.save_item_entities();
            self.save_mobs();
            self.save_furnaces();
            self.save_chests();
            self.save_world();
        }
    }

    fn spawn_item_entity(&mut self, item: ssc_world::ItemId, count: u8, pos: Vec3) {
        self.spawn_item_entity_with_durability(
            item,
            count,
            pos,
            crate::survival::max_durability(item),
        );
    }

    fn spawn_item_entity_with_durability(
        &mut self,
        item: ssc_world::ItemId,
        count: u8,
        pos: Vec3,
        durability: u16,
    ) {
        if count == 0 {
            return;
        }
        let Some(id) = self.allocate_item_id() else {
            eprintln!("[server] no free entity id for dropped item");
            return;
        };
        self.items.insert(
            id,
            ItemEntity {
                item,
                count,
                durability,
                pos,
                vel: Vec3::new(0.0, 1.5, 0.0),
                age: 0,
                pickup_delay: 10,
            },
        );
    }

    fn allocate_item_id(&mut self) -> Option<EntityId> {
        let range = (EntityId::MAX - FIRST_ITEM_ID) as usize + 1;
        for _ in 0..range {
            let id = self.next_item_id;
            self.next_item_id = if id == EntityId::MAX {
                FIRST_ITEM_ID
            } else {
                id + 1
            };
            if !self.sessions.contains_key(&id)
                && !self.mobs.contains_key(&id)
                && !self.items.contains_key(&id)
                && !self.arrows.contains_key(&id)
            {
                return Some(id);
            }
        }
        None
    }

    fn allocate_mob_id(&mut self) -> Option<EntityId> {
        for _ in FIRST_MOB_ID..FIRST_ITEM_ID {
            let id = self.next_mob_id;
            self.next_mob_id = if id + 1 >= FIRST_ITEM_ID {
                FIRST_MOB_ID
            } else {
                id + 1
            };
            if !self.sessions.contains_key(&id)
                && !self.mobs.contains_key(&id)
                && !self.items.contains_key(&id)
                && !self.arrows.contains_key(&id)
            {
                return Some(id);
            }
        }
        None
    }

    fn tick_items(&mut self, dt: f32) {
        // Unoccupied worlds are inactive: items must not expire or move while
        // nobody can keep their terrain loaded, including after a restart.
        if !self.sessions.values().any(|s| s.player.joined) {
            return;
        }
        let mut expired = Vec::new();
        let players: Vec<Vec3> = self
            .sessions
            .values()
            .filter(|session| session.player.joined && !session.survival.dead)
            .map(|session| session.player.pos + Vec3::new(0.0, 0.9, 0.0))
            .collect();
        for (id, item) in self.items.iter_mut() {
            // Only active chunks tick entities. Other saved stacks stay put
            // until a player returns, rather than falling through unloaded
            // terrain or despawning while nobody is nearby.
            if !players
                .iter()
                .any(|player| (item.pos - *player).length_squared() <= 64.0 * 64.0)
            {
                continue;
            }
            item.age += 1;
            item.pickup_delay = item.pickup_delay.saturating_sub(1);
            if item.age >= ITEM_LIFETIME_TICKS || item.pos.y < crate::physics::ITEM_VOID_Y {
                expired.push(*id);
                continue;
            }
            crate::physics::move_item(&self.world, &mut item.pos, &mut item.vel, dt);
            if item.pos.y < crate::physics::ITEM_VOID_Y {
                expired.push(*id);
            }
        }

        expired.sort_unstable();
        expired.dedup();
        for id in expired.drain(..) {
            if self.items.remove(&id).is_some() {
                let _ = self.broadcast(&ServerPacket::EntityRemove { id });
            }
        }
        self.merge_nearby_items();

        let ids: Vec<EntityId> = self.items.keys().copied().collect();
        for item_id in ids {
            let Some(item) = self.items.get(&item_id) else {
                continue;
            };
            let (item_value, mut count, pos, durability) =
                (item.item, item.count, item.pos, item.durability);
            if item.pickup_delay > 0 {
                continue;
            }
            let mut candidates: Vec<(EntityId, f32)> = self
                .sessions
                .iter()
                .filter(|(_, s)| {
                    s.player.joined
                        && !s.survival.dead
                        && crate::physics::item_in_pickup_range(s.player.pos, pos)
                })
                .map(|(id, s)| (*id, (s.player.pos - pos).length()))
                .collect();
            candidates.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            for (player_id, _) in candidates {
                let Some(session) = self.sessions.get_mut(&player_id) else {
                    continue;
                };
                let leftover = if crate::survival::max_durability(item_value) > 0 {
                    if let Some(slot) = session.inventory.iter().position(Option::is_none) {
                        session.inventory[slot] = Some((item_value, 1));
                        session.durability[slot] =
                            crate::survival::effective_durability(item_value, durability);
                        count - 1
                    } else {
                        count
                    }
                } else {
                    crate::survival::add_item(&mut session.inventory, item_value, count)
                };
                if leftover < count {
                    session.inv_dirty = true;
                    count = leftover;
                    if count == 0 {
                        break;
                    }
                }
            }
            if let Some(item) = self.items.get_mut(&item_id) {
                item.count = count;
            }
            if count == 0 {
                expired.push(item_id);
            }
        }
        expired.sort_unstable();
        expired.dedup();
        for id in expired {
            if self.items.remove(&id).is_some() {
                let _ = self.broadcast(&ServerPacket::EntityRemove { id });
            }
        }
    }

    fn merge_nearby_items(&mut self) {
        // Index half-block cells so separated drops don't incur a global
        // quadratic scan on every 20 Hz server tick.
        let mut cells: HashMap<(i32, i32, i32), Vec<EntityId>> = HashMap::new();
        for (&id, item) in &self.items {
            let cell = (
                (item.pos.x * 2.0).floor() as i32,
                (item.pos.y * 2.0).floor() as i32,
                (item.pos.z * 2.0).floor() as i32,
            );
            cells.entry(cell).or_default().push(id);
        }

        let mut ids: Vec<EntityId> = self.items.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let Some(a) = self.items.get(&id) else {
                continue;
            };
            let cell = (
                (a.pos.x * 2.0).floor() as i32,
                (a.pos.y * 2.0).floor() as i32,
                (a.pos.z * 2.0).floor() as i32,
            );
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for dz in -1..=1 {
                        let Some(neighbors) = cells.get(&(cell.0 + dx, cell.1 + dy, cell.2 + dz))
                        else {
                            continue;
                        };
                        for &other_id in neighbors {
                            if other_id <= id {
                                continue;
                            }
                            let (Some(a), Some(b)) =
                                (self.items.get(&id), self.items.get(&other_id))
                            else {
                                continue;
                            };
                            if a.count == 0
                                || b.count == 0
                                || a.item != b.item
                                || (a.pos - b.pos).length_squared() > 0.25
                            {
                                continue;
                            }
                            let max_stack = ssc_world::item_info(a.item).max_stack;
                            let transfer = max_stack.saturating_sub(a.count).min(b.count);
                            if transfer == 0 {
                                continue;
                            }
                            let age = a.age.min(b.age);
                            // A newly dropped stack must not become instantly
                            // collectible just because it merged with an old
                            // stack whose pickup delay already elapsed.
                            let pickup_delay = a.pickup_delay.max(b.pickup_delay);
                            if let Some(a) = self.items.get_mut(&id) {
                                a.count += transfer;
                                a.age = age;
                                a.pickup_delay = pickup_delay;
                            }
                            if let Some(b) = self.items.get_mut(&other_id) {
                                b.count -= transfer;
                            }
                        }
                    }
                }
            }
        }
        let empty: Vec<EntityId> = self
            .items
            .iter()
            .filter(|(_, item)| item.count == 0)
            .map(|(id, _)| *id)
            .collect();
        for id in empty {
            self.items.remove(&id);
            let _ = self.broadcast(&ServerPacket::EntityRemove { id });
        }
    }

    fn can_access_furnace(&self, id: EntityId, pos: (i32, i32, i32)) -> bool {
        let Some(session) = self.sessions.get(&id) else {
            return false;
        };
        let (x, y, z) = pos;
        if !session.player.joined
            || session.survival.dead
            || !ssc_world::is_furnace(self.world.block_or_air(x, y, z))
        {
            return false;
        }
        let eye = session.player.pos + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
        let min = Vec3::new(x as f32, y as f32, z as f32);
        (eye - eye.clamp(min, min + Vec3::ONE)).length() <= REACH
    }

    fn furnace_packet(&self, pos: (i32, i32, i32), open: bool) -> Option<ServerPacket> {
        let furnace = self.furnaces.get(&pos)?;
        Some(ServerPacket::Furnace {
            open,
            x: pos.0,
            y: pos.1,
            z: pos.2,
            slots: furnace.slots.map(|slot| slot.unwrap_or((0, 0))),
            burn_remaining: furnace.burn_remaining,
            burn_total: furnace.burn_total,
            cook_ticks: furnace.cook_ticks,
        })
    }

    fn sync_furnace(&mut self, pos: (i32, i32, i32)) {
        let Some(packet) = self.furnace_packet(pos, false) else {
            return;
        };
        let viewers: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| (session.open_furnace == Some(pos)).then_some(*id))
            .collect();
        for id in viewers {
            let _ = self.direct(id, &packet);
        }
    }

    fn can_access_chest(&self, id: EntityId, pos: (i32, i32, i32)) -> bool {
        let Some(session) = self.sessions.get(&id) else {
            return false;
        };
        let (x, y, z) = pos;
        if !session.player.joined
            || session.survival.dead
            || self.world.block_or_air(x, y, z) != ssc_world::CHEST
        {
            return false;
        }
        let eye = session.player.pos + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
        let min = Vec3::new(x as f32, y as f32, z as f32);
        (eye - eye.clamp(min, min + Vec3::ONE)).length() <= REACH
    }

    fn chest_packet(&self, pos: (i32, i32, i32), open: bool) -> Option<ServerPacket> {
        let chest = self.chests.get(&pos)?;
        Some(ServerPacket::Chest {
            open,
            x: pos.0,
            y: pos.1,
            z: pos.2,
            slots: chest
                .slots
                .iter()
                .map(|slot| slot.unwrap_or((0, 0)))
                .collect(),
        })
    }

    fn sync_chest(&mut self, pos: (i32, i32, i32)) {
        let Some(packet) = self.chest_packet(pos, false) else {
            return;
        };
        let viewers: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| (session.open_chest == Some(pos)).then_some(*id))
            .collect();
        for id in viewers {
            let _ = self.direct(id, &packet);
        }
    }

    fn transfer_chest(
        &mut self,
        id: EntityId,
        inventory_slot: u8,
        chest_slot: u8,
        from_chest: bool,
    ) -> Result<(), String> {
        let inv = inventory_slot as usize;
        let slot = chest_slot as usize;
        if inv >= crate::survival::INVENTORY_SIZE || slot >= crate::persistence::CHEST_SLOTS {
            return Err("chest slot out of range".into());
        }
        let Some(pos) = self.sessions.get(&id).and_then(|s| s.open_chest) else {
            return Ok(());
        };
        if !self.can_access_chest(id, pos) {
            return Ok(());
        }
        let chest = self.chests.entry(pos).or_default();
        let session = self.sessions.get_mut(&id).unwrap();
        let own = session.inventory[inv];
        let own_wear = session.durability[inv];
        let stored = chest.slots[slot];
        let stored_wear = chest.durability[slot];
        let (source, destination) = if from_chest {
            (stored, own)
        } else {
            (own, stored)
        };
        let Some((item, count)) = source else {
            return Ok(());
        };
        let (next_source, next_dest) = match destination {
            Some((target, target_count)) if target == item => {
                let moved = count.min(
                    ssc_world::item_info(item)
                        .max_stack
                        .saturating_sub(target_count),
                );
                if moved == 0 {
                    return Ok(());
                }
                (
                    if moved == count {
                        None
                    } else {
                        Some((item, count - moved))
                    },
                    Some((item, target_count + moved)),
                )
            }
            _ => (destination, source),
        };
        if from_chest {
            chest.slots[slot] = next_source;
            chest.durability[slot] = if next_source.is_some() {
                stored_wear
            } else {
                0
            };
            session.inventory[inv] = next_dest;
            session.durability[inv] = stored_wear;
        } else {
            session.inventory[inv] = next_source;
            session.durability[inv] = if next_source.is_some() { own_wear } else { 0 };
            chest.slots[slot] = next_dest;
            chest.durability[slot] = own_wear;
        }
        session.inv_dirty = true;
        if inv == session.player.selected_slot as usize {
            session.survival.digging = None;
            session.eating = None;
        }
        self.sync_chest(pos);
        Ok(())
    }

    fn tick_chests(&mut self) {
        let invalid: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                session
                    .open_chest
                    .filter(|pos| !self.can_access_chest(*id, *pos))
                    .map(|_| *id)
            })
            .collect();
        for id in invalid {
            if let Some(session) = self.sessions.get_mut(&id) {
                session.open_chest = None;
            }
            let _ = self.direct(id, &ServerPacket::ContainerClosed);
        }
    }

    fn tick_furnaces(&mut self) {
        let invalid: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                session
                    .open_furnace
                    .filter(|pos| !self.can_access_furnace(*id, *pos))
                    .map(|_| *id)
            })
            .collect();
        for id in invalid {
            self.sessions.get_mut(&id).unwrap().open_furnace = None;
            let _ = self.direct(id, &ServerPacket::ContainerClosed);
        }
        let players: Vec<_> = self
            .sessions
            .values()
            .filter(|session| session.player.joined)
            .map(|session| {
                (
                    (session.player.pos.x.floor() as i32).div_euclid(16),
                    (session.player.pos.z.floor() as i32).div_euclid(16),
                )
            })
            .collect();
        let mut changed = Vec::new();
        let mut blocks = Vec::new();
        for (&pos, furnace) in &mut self.furnaces {
            let (x, y, z) = pos;
            if !players.iter().any(|(px, pz)| {
                (x.div_euclid(16) - px).abs() <= 4 && (z.div_euclid(16) - pz).abs() <= 4
            }) {
                continue;
            }
            let old_block = self.world.block_or_air(x, y, z);
            if !ssc_world::is_furnace(old_block) {
                continue;
            }
            let old_slots = furnace.slots;
            if furnace.tick()
                && (self.tick % 4 == 0 || old_slots != furnace.slots || furnace.burn_remaining == 0)
            {
                changed.push(pos);
            }
            let block = if furnace.burn_remaining > 0 {
                ssc_world::LIT_FURNACE
            } else {
                ssc_world::FURNACE
            };
            if old_block != block {
                blocks.push((pos, block));
            }
        }
        for ((x, y, z), block) in blocks {
            self.world.set_block(x, y, z, block);
            self.record_block_edit(x, y, z, block);
            let _ = self.broadcast(&ServerPacket::BlockUpdate { x, y, z, block });
            self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
        }
        for pos in changed {
            self.sync_furnace(pos);
        }
    }

    fn transfer_furnace(
        &mut self,
        id: EntityId,
        inventory_slot: u8,
        furnace_slot: u8,
        from_furnace: bool,
    ) -> Result<(), String> {
        let inv = inventory_slot as usize;
        let slot = furnace_slot as usize;
        if inv >= crate::survival::INVENTORY_SIZE || slot >= 3 {
            return Err("furnace slot out of range".into());
        }
        let Some(pos) = self
            .sessions
            .get(&id)
            .and_then(|session| session.open_furnace)
        else {
            return Ok(());
        };
        if !self.can_access_furnace(id, pos) {
            return Ok(());
        }
        let furnace = self.furnaces.entry(pos).or_default();
        let session = self.sessions.get_mut(&id).unwrap();
        let own = session.inventory[inv];
        let stored = furnace.slots[slot];
        let (source, destination) = if from_furnace {
            (stored, own)
        } else {
            (own, stored)
        };
        let Some((item, count)) = source else {
            return Ok(());
        };
        if !from_furnace && !crate::furnace::accepts(slot, item) {
            return Ok(());
        }
        let mut moved_count = 0u8;
        let (next_source, next_dest) = match destination {
            Some((target, target_count)) if target == item => {
                let moved = count.min(
                    ssc_world::item_info(item)
                        .max_stack
                        .saturating_sub(target_count),
                );
                if moved == 0 {
                    return Ok(());
                }
                moved_count = moved;
                (
                    if moved == count {
                        None
                    } else {
                        Some((item, count - moved))
                    },
                    Some((item, target_count + moved)),
                )
            }
            _ => {
                if from_furnace
                    && destination.is_some_and(|(item, _)| !crate::furnace::accepts(slot, item))
                {
                    return Ok(());
                }
                moved_count = count;
                (destination, source)
            }
        };
        let previous_recipe =
            furnace.slots[crate::furnace::INPUT].and_then(|(item, _)| crate::furnace::recipe(item));
        if from_furnace {
            furnace.slots[slot] = next_source;
            session.inventory[inv] = next_dest;
        } else {
            session.inventory[inv] = next_source;
            furnace.slots[slot] = next_dest;
        }
        session.durability[inv] = 0;
        session.inv_dirty = true;
        if inv == session.player.selected_slot as usize {
            session.survival.digging = None;
            session.eating = None;
        }
        if previous_recipe
            != furnace.slots[crate::furnace::INPUT]
                .and_then(|(item, _)| crate::furnace::recipe(item))
        {
            furnace.cook_ticks = 0;
        }
        self.sync_furnace(pos);
        if from_furnace && slot == crate::furnace::OUTPUT {
            self.award_xp(id, moved_count as u32 * crate::furnace::xp_for(item));
        }
        Ok(())
    }

    /// Random block ticks only run in loaded columns near joined players.
    /// The fixed four-column radius is independent of visual LOD distance.
    fn tick_blocks(&mut self) {
        let players: Vec<ColumnKey> = self
            .sessions
            .values()
            .filter(|session| session.player.joined)
            .map(|session| {
                (
                    (session.player.pos.x.floor() as i32).div_euclid(16),
                    (session.player.pos.z.floor() as i32).div_euclid(16),
                )
            })
            .collect();
        if players.is_empty() {
            return;
        }
        let mut columns: Vec<ColumnKey> = self
            .world
            .columns
            .keys()
            .copied()
            .filter(|(cx, cz)| {
                players
                    .iter()
                    .any(|(px, pz)| (cx - px).abs() <= 4 && (cz - pz).abs() <= 4)
            })
            .collect();
        columns.sort_unstable();
        let mut picks = Vec::new();
        for (cx, cz) in columns {
            let column = &self.world.columns[&(cx, cz)];
            for (section, subchunk) in column.subchunks.iter().enumerate() {
                if subchunk.is_some() {
                    picks.extend_from_slice(&crate::block_ticks::samples(
                        self.tick,
                        self.gen.seed(),
                        cx,
                        cz,
                        section,
                    ));
                }
            }
        }
        let daylight = self.time_of_day > 0.25 && self.time_of_day < 0.75;
        let mut dirty_columns = HashSet::new();
        for ((x, y, z), roll) in picks {
            let sampled = self.world.block_or_air(x, y, z);
            let changes = if ssc_world::is_oak_sapling(sampled) {
                let changes =
                    crate::block_ticks::sapling_changes(&self.world, x, y, z, roll, daylight);
                if changes.contains(&((x, y, z), AIR)) {
                    self.spawn_item_entity(
                        ssc_world::OAK_SAPLING,
                        1,
                        Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5),
                    );
                }
                changes
            } else if ssc_world::is_wheat(sampled) {
                let soil = (x, y - 1, z);
                let mut changes = Vec::new();
                let soil_block = self.world.block_or_air(soil.0, soil.1, soil.2);
                let wet =
                    crate::block_ticks::farmland_should_be_wet(&self.world, soil.0, soil.1, soil.2);
                if ssc_world::is_farmland(soil_block) {
                    let desired = if wet {
                        ssc_world::FARMLAND_WET
                    } else {
                        ssc_world::FARMLAND
                    };
                    if soil_block != desired {
                        changes.push((soil, desired));
                    }
                }
                let wheat =
                    crate::block_ticks::wheat_changes(&self.world, x, y, z, roll, daylight, wet);
                if wheat.contains(&((x, y, z), AIR)) {
                    self.spawn_item_entity(
                        ssc_world::WHEAT_SEEDS,
                        1,
                        Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5),
                    );
                }
                changes.extend(wheat);
                changes
            } else if ssc_world::is_oak_leaves(sampled) {
                let column = (x.div_euclid(16), z.div_euclid(16));
                let local = (x.rem_euclid(16), y, z.rem_euclid(16));
                // Leaf overrides are player placements. Natural leaves are
                // regenerated from the seed and have no such override.
                let persistent = self
                    .block_edits
                    .get(&column)
                    .and_then(|edits| edits.get(&local))
                    == Some(&ssc_world::OAK_LEAVES);
                if !persistent && crate::block_ticks::leaf_can_decay(&self.world, x, y, z) {
                    for (item, count) in crate::survival::block_drops(sampled, roll, None) {
                        self.spawn_item_entity(
                            item,
                            count,
                            Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5),
                        );
                    }
                    vec![((x, y, z), AIR)]
                } else {
                    Vec::new()
                }
            } else {
                crate::block_ticks::grass_changes(&self.world, x, y, z, roll, daylight)
            };
            for ((x, y, z), block) in changes {
                if self.world.block_or_air(x, y, z) == block {
                    continue;
                }
                self.world.set_block(x, y, z, block);
                self.record_block_edit(x, y, z, block);
                let _ = self.broadcast(&ServerPacket::BlockUpdate { x, y, z, block });
                dirty_columns.insert((x.div_euclid(16), z.div_euclid(16)));
            }
        }
        for column in dirty_columns {
            self.refresh_lod_for(column);
        }
    }

    /// Mob spawning: pigs in small groups by day, zombies at night.
    /// Simple, deterministic-ish, capped.
    fn spawn_mobs(&mut self) {
        let night = self.time_of_day > 0.75 && self.time_of_day < 0.98;
        let player_cols: Vec<(i32, i32)> = self
            .sessions
            .values()
            .filter(|s| s.player.joined)
            .map(|s| {
                (
                    (s.player.pos.x.floor() as i32).div_euclid(16),
                    (s.player.pos.z.floor() as i32).div_euclid(16),
                )
            })
            .collect();
        if player_cols.is_empty() {
            return;
        }
        let mob_cap = player_cols.len() * MOB_CAP_PER_PLAYER;
        if self.mobs.len() >= mob_cap {
            return;
        }

        // Pigs by day, zombies at night.
        let want_zombie = night;
        let want_pig = !night;
        for (pcx, pcz) in player_cols {
            if self.mobs.len() >= mob_cap {
                break;
            }
            for attempt in 0..4 {
                // Sample a larger square, then enforce the spherical
                // 24–44 block shell against actual player feet positions.
                let ax =
                    (pcx * 16 + 8 + (hash_tick(self.tick + attempt, pcx, pcz) % 96) as i32) - 48;
                let az =
                    (pcz * 16 + 8 + (hash_tick(self.tick + attempt * 7, pcz, pcx) % 96) as i32)
                        - 48;
                let y = if want_pig {
                    crate::mobs::pig_spawn_column_ok(&self.world, ax, az)
                } else {
                    crate::mobs::spawn_column_ok(&self.world, ax, az)
                };
                let Some(y) = y else {
                    continue;
                };
                // Column must exist server-side (spawn only in streamed
                // areas so clients see the mob's ground).
                if self
                    .world
                    .column(ax.div_euclid(16), az.div_euclid(16))
                    .is_none()
                {
                    continue;
                }
                let kind = if want_zombie {
                    if hash_tick(self.tick, ax + 5, az + 13) % 5 == 0 {
                        crate::mobs::MobKind::Spider
                    } else {
                        crate::mobs::MobKind::Zombie
                    }
                } else {
                    match hash_tick(self.tick, ax + 3, az + 11) % 6 {
                        0 => crate::mobs::MobKind::Sheep,
                        1 => crate::mobs::MobKind::Cow,
                        _ => crate::mobs::MobKind::Pig,
                    }
                };
                // Bedrock passive mobs spawn in grass groups of 1–3. Each
                // member must independently have grass, clearance and player
                // distance; never exceed the server's shared mob cap.
                let group_size = if want_pig {
                    1 + (hash_tick(self.tick, ax, az) % 3) as usize
                } else {
                    1
                };
                let mut spawned = 0;
                for member in 0..group_size {
                    if self.mobs.len() >= mob_cap {
                        break;
                    }
                    let (x, z) = if member == 0 {
                        (ax, az)
                    } else {
                        let roll = hash_tick(self.tick + member as u64, ax, az);
                        (
                            ax + ((roll % 5) as i32 - 2),
                            az + (((roll >> 8) % 5) as i32 - 2),
                        )
                    };
                    let standing_y = if member == 0 {
                        Some(y)
                    } else {
                        crate::mobs::pig_spawn_column_ok(&self.world, x, z)
                    };
                    let Some(standing_y) = standing_y else {
                        continue;
                    };
                    if (standing_y - y).abs() > 1 {
                        continue;
                    }
                    let pos = Vec3::new(x as f32 + 0.5, standing_y as f32 + 0.02, z as f32 + 0.5);
                    if self.sessions.values().any(|s| {
                        s.player.joined && (s.player.pos - pos).length() < MIN_MOB_SPAWN_DISTANCE
                    }) || !self.sessions.values().any(|s| {
                        s.player.joined && (s.player.pos - pos).length() <= MAX_MOB_DISTANCE
                    }) || self.mobs.values().any(|m| (m.pos - pos).length() < 1.0)
                    {
                        continue;
                    }
                    let Some(id) = self.allocate_mob_id() else {
                        return;
                    };
                    self.mobs.insert(id, crate::mobs::Mob::new(kind, pos));
                    spawned += 1;
                }
                if spawned > 0 {
                    break; // at most one group per player per second
                }
            }
        }
    }

    /// Punch: if the crosshair ray hits an entity first, damage it;
    /// returns true when an entity was hit (skip block digging).
    fn try_attack(&mut self, id: EntityId) -> bool {
        let (yaw, pitch) = self.look_of(id);
        let dir = look_dir(yaw, pitch);
        let eye = self.eye_of(id);
        // Candidate entities: mobs + other players.
        let mut candidates: Vec<(EntityId, ssc_math::Aabb, bool)> = Vec::new();
        for (mid, m) in &self.mobs {
            if m.dead {
                continue;
            }
            let (hw, h) = m.kind.dims();
            candidates.push((*mid, entity_aabb(m.pos, hw, h), true));
        }
        for (pid, session) in &self.sessions {
            if *pid != id && session.player.joined && !session.survival.dead {
                candidates.push((*pid, player_aabb(session.player.pos), false));
            }
        }
        let mut best: Option<(EntityId, f32, bool)> = None;
        for (eid, aabb, is_mob) in candidates {
            if let Some(t) = ray_aabb(eye, dir, &aabb) {
                if t <= MELEE_REACH && best.map(|(_, bt, _)| t < bt).unwrap_or(true) {
                    best = Some((eid, t, is_mob));
                }
            }
        }
        let Some((target, t, is_mob)) = best else {
            return false;
        };
        // The nearest entity only takes priority when no solid voxel is
        // between its hitbox and the attacker's eyes.
        if crate::raycast::raycast_solids(&self.world, eye, dir, (t - 0.0001).max(0.0)).is_some() {
            return false;
        }
        let held = self.selected_item(id).map(|(item, _)| item);
        let attacker = &self.sessions[&id].player;
        // Unlike Java, Bedrock permits a sprinting falling critical.
        // No mounts, climbable blocks or status effects exist yet.
        let critical =
            attacker.vel.y < 0.0 && !attacker.on_ground && !attacker.in_water && !attacker.flying;
        let damage = crate::survival::attack_damage(held) * if critical { 1.5 } else { 1.0 };

        if is_mob {
            // Bedrock tool damage; knockback remains server-owned.
            let from = self.sessions[&id].player.pos;
            let mut killed = None;
            if let Some(mob) = self.mobs.get_mut(&target) {
                let before = mob.health;
                let died = mob.hurt(damage, from);
                if mob.health >= before {
                    // This still intercepted the attack ray: don't start
                    // digging terrain behind an immune mob.
                    return true;
                }
                if died {
                    killed = Some(mob.kind);
                }
            }
            let pos = self.mobs.get(&target).map(|m| m.pos);
            if let Some(p) = pos {
                self.send_sound(
                    SoundId::Hurt,
                    p.x.floor() as i32,
                    p.y.floor() as i32,
                    p.z.floor() as i32,
                );
            }
            // Death and entity removal are handled together during the
            // authoritative mob tick so clients don't receive remove/state/
            // remove in consecutive packets.
            if self.cfg.game_mode == GameMode::Survival {
                if let Some(session) = self.sessions.get_mut(&id) {
                    session.survival.exhaustion += 0.1;
                    damage_held_tool(
                        session,
                        if held.is_some_and(ssc_world::is_sword) {
                            1
                        } else {
                            2
                        },
                    );
                }
            }
            if let Some(kind) = killed {
                let xp = crate::mobs::xp_for(
                    kind,
                    hash_tick(self.tick, target as i32, from.x.floor() as i32),
                );
                self.award_xp(id, xp);
            }
            true
        } else {
            // PvP damage and knockback via velocity.
            let from = self.sessions[&id].player.pos;
            let session = self.sessions.get_mut(&target).unwrap();
            if self.cfg.game_mode == GameMode::Survival {
                let mut fx = crate::survival::TickEffects::default();
                let points = crate::survival::armor_points(&session.armor);
                let damage = damage * (1.0 - crate::survival::armor_reduction(points));
                session.survival.apply_combat_damage(damage, &mut fx);
                if !fx.hurt {
                    return true;
                }
                knock_player_back(&mut session.player, from);
                session.survival.exhaustion += 0.1;
                damage_armor(session, 1);
                session.status_dirty = true;
            }
            let p = self.sessions[&target].player.pos;
            self.send_sound(
                SoundId::Hurt,
                p.x.floor() as i32,
                p.y.floor() as i32,
                p.z.floor() as i32,
            );
            if self.cfg.game_mode == GameMode::Survival {
                if let Some(session) = self.sessions.get_mut(&id) {
                    session.survival.exhaustion += 0.1;
                    damage_held_tool(
                        session,
                        if held.is_some_and(ssc_world::is_sword) {
                            1
                        } else {
                            2
                        },
                    );
                }
            }
            true
        }
    }
}

/// Wear follows the selected tool, not the slot. Last-use hits still land.
fn damage_held_tool(session: &mut Session, uses: u16) {
    let slot = session.player.selected_slot as usize % 9;
    let Some((item, _)) = session.inventory[slot] else {
        return;
    };
    if crate::survival::max_durability(item) == 0 {
        return;
    }
    let remaining = crate::survival::effective_durability(item, session.durability[slot]);
    if remaining <= uses {
        session.inventory[slot] = None;
        session.durability[slot] = 0;
    } else {
        session.durability[slot] = remaining - uses;
    }
    session.inv_dirty = true;
}

/// One durability point per equipped piece on a damaging hit. Bedrock's
/// exact damage-to-durability formula is simplified.
fn damage_armor(session: &mut Session, usage: u16) -> bool {
    let mut changed = false;
    for index in 0..crate::survival::ARMOR_SIZE {
        let Some((item, _)) = session.armor[index] else {
            continue;
        };
        let remaining =
            crate::survival::effective_durability(item, session.armor_durability[index]);
        changed = true;
        if remaining <= usage {
            session.armor[index] = None;
            session.armor_durability[index] = 0;
        } else {
            session.armor_durability[index] = remaining - usage;
        }
    }
    if changed {
        session.armor_dirty = true;
        session.inv_dirty = true;
    }
    changed
}

// ---------------------------------------------------------------------------
// Player simulation
// ---------------------------------------------------------------------------

fn knock_player_back(player: &mut Player, from: Vec3) {
    let offset = player.pos - from;
    let direction = Vec3::new(offset.x, 0.0, offset.z).normalize_or_zero();
    player.vel.x += direction.x * 6.0;
    player.vel.z += direction.z * 6.0;
    player.vel.y = (player.vel.y + 4.0).min(6.0);
    // The impulse lifts the player: use air steering next tick instead of
    // replacing horizontal knockback with grounded walking input.
    player.on_ground = false;
}

/// Simulate one player for one tick (server-authoritative).
fn step_player(
    world: &World,
    player: &mut Player,
    dt: f32,
    flying: bool,
    sprint: bool,
    using_item: bool,
) {
    let k = player.keys;
    let yaw = player.yaw;

    // Swimming: check the block at the feet and at the eyes.
    let feet = player.pos;
    let eye = player.pos + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
    let in_water = world.block_or_air(
        feet.x.floor() as i32,
        (feet.y + 0.4).floor() as i32,
        feet.z.floor() as i32,
    ) == WATER
        || world.block_or_air(
            eye.x.floor() as i32,
            eye.y.floor() as i32,
            eye.z.floor() as i32,
        ) == WATER;
    let in_ladder = [0.1, 0.9, 1.6].iter().any(|dy| {
        world.block_or_air(
            feet.x.floor() as i32,
            (feet.y + dy).floor() as i32,
            feet.z.floor() as i32,
        ) == ssc_world::LADDER
    });

    let fwd = yaw_forward(yaw);
    let right = yaw_right(yaw);
    let mut wish = [0.0f32; 2];
    if k & keys::FORWARD != 0 {
        wish[0] += fwd.x;
        wish[1] += fwd.y;
    }
    if k & keys::BACKWARD != 0 {
        wish[0] -= fwd.x;
        wish[1] -= fwd.y;
    }
    if k & keys::LEFT != 0 {
        wish[0] -= right.x;
        wish[1] -= right.y;
    }
    if k & keys::RIGHT != 0 {
        wish[0] += right.x;
        wish[1] += right.y;
    }
    let sneak = k & keys::SNEAK != 0;
    let speed = if flying {
        if sprint {
            21.78
        } else {
            10.89
        }
    } else if in_water {
        WALK_SPEED * 0.55 // swimming is slow
    } else if sprint {
        SPRINT_SPEED
    } else if sneak {
        SNEAK_SPEED
    } else {
        WALK_SPEED
    };
    let speed = speed * if using_item && !flying { 0.2 } else { 1.0 };
    let wish_len = (wish[0] * wish[0] + wish[1] * wish[1]).sqrt();
    let target = if wish_len > 0.0 {
        (wish[0] / wish_len * speed, wish[1] / wish_len * speed)
    } else {
        (0.0, 0.0)
    };

    if flying {
        player.vel.x = target.0;
        player.vel.z = target.1;
        player.vel.y = if k & keys::JUMP != 0 {
            10.89
        } else if sneak {
            -10.89
        } else {
            0.0
        };
    } else if in_water {
        // Buoyant drag physics: slow sink, jump key swims up.
        player.vel.x += (target.0 - player.vel.x) * 0.4;
        player.vel.z += (target.1 - player.vel.z) * 0.4;
        player.vel.y -= GRAVITY * 0.3 * dt;
        if k & keys::JUMP != 0 {
            player.vel.y = (player.vel.y + 2.4).min(3.5);
        }
        player.vel.y = player.vel.y.clamp(-3.5, 4.0);
    } else if in_ladder {
        // Climb up with jump, hold still while sneaking, otherwise slide.
        player.vel.x += (target.0 - player.vel.x) * 0.5;
        player.vel.z += (target.1 - player.vel.z) * 0.5;
        if k & keys::JUMP != 0 {
            player.vel.y = (player.vel.y + GRAVITY * 0.6 * dt).min(2.35);
        } else if sneak {
            player.vel.y = 0.0;
        } else {
            player.vel.y = (player.vel.y - GRAVITY * 0.2 * dt).max(-1.5);
        }
    } else {
        // Ground: instant target velocity. Air: limited steering.
        if player.on_ground {
            player.vel.x = target.0;
            player.vel.z = target.1;
            if k & keys::JUMP != 0 {
                player.vel.y = JUMP_VELOCITY;
                player.on_ground = false;
            }
        } else {
            let blend = 0.15;
            player.vel.x += (target.0 - player.vel.x) * blend;
            player.vel.z += (target.1 - player.vel.z) * blend;
        }

        player.vel.y -= GRAVITY * dt;
        if player.vel.y < -TERMINAL_VELOCITY {
            player.vel.y = -TERMINAL_VELOCITY;
        }
    }

    let mut pos = player.pos;
    let mut vel = player.vel;
    let res = if sneak && player.on_ground && !flying && !in_water {
        crate::physics::move_sneaking_player(world, &mut pos, &mut vel, dt)
    } else {
        move_player(world, &mut pos, &mut vel, dt)
    };
    player.pos = pos;
    player.vel = vel;
    player.on_ground = res.on_ground;
    player.in_water = res.in_water;
}

fn entity_state(id: EntityId, p: &Player) -> ServerPacket {
    ServerPacket::EntityState {
        id,
        x: p.pos.x,
        y: p.pos.y,
        z: p.pos.z,
        yaw: p.yaw,
        pitch: p.pitch,
        kind: 0, // player
    }
}

fn mob_state(id: EntityId, m: &crate::mobs::Mob) -> ServerPacket {
    ServerPacket::EntityState {
        id,
        x: m.pos.x,
        y: m.pos.y,
        z: m.pos.z,
        yaw: m.yaw,
        pitch: 0.0,
        kind: m.kind.as_u8(),
    }
}

fn arrow_state(id: EntityId, arrow: &ArrowEntity) -> ServerPacket {
    ServerPacket::EntityState {
        id,
        x: arrow.pos.x,
        y: arrow.pos.y,
        z: arrow.pos.z,
        yaw: arrow.vel.x.atan2(arrow.vel.z),
        pitch: arrow
            .vel
            .y
            .atan2((arrow.vel.x * arrow.vel.x + arrow.vel.z * arrow.vel.z).sqrt()),
        kind: 7,
    }
}

fn item_state(id: EntityId, item: &ItemEntity) -> ServerPacket {
    ServerPacket::ItemState {
        id,
        item: item.item,
        count: item.count,
        x: item.pos.x,
        y: item.pos.y,
        z: item.pos.z,
    }
}

/// Ray vs AABB slab test: entry distance if the ray hits.
fn ray_aabb(origin: Vec3, dir: Vec3, aabb: &ssc_math::Aabb) -> Option<f32> {
    let mut tmin = 0.0f32;
    let mut tmax = f32::INFINITY;
    for (o, d, (lo, hi)) in [
        (origin.x, dir.x, (aabb.min.x, aabb.max.x)),
        (origin.y, dir.y, (aabb.min.y, aabb.max.y)),
        (origin.z, dir.z, (aabb.min.z, aabb.max.z)),
    ] {
        if d.abs() < 1e-8 {
            if o < lo || o > hi {
                return None;
            }
            continue;
        }
        let mut t1 = (lo - o) / d;
        let mut t2 = (hi - o) / d;
        if t1 > t2 {
            std::mem::swap(&mut t1, &mut t2);
        }
        tmin = tmin.max(t1);
        tmax = tmax.min(t2);
        if tmin > tmax {
            return None;
        }
    }
    Some(tmin)
}

impl Server {
    fn player_col(&self, id: EntityId) -> ColumnKey {
        let p = &self.sessions[&id].player.pos;
        (
            (p.x.floor() as i32).div_euclid(16),
            (p.z.floor() as i32).div_euclid(16),
        )
    }

    /// Stream a bounded amount of queued data to one session.
    fn stream_to(&mut self, id: EntityId, max_cols: usize, max_lods: usize) {
        for _ in 0..max_cols {
            let key = {
                let Some(session) = self.sessions.get_mut(&id) else {
                    return;
                };
                match session.stream_queue.pop_front() {
                    Some(k) => k,
                    None => break,
                }
            };
            self.stream_column(id, key);
        }
        for _ in 0..max_lods {
            let key = {
                let Some(session) = self.sessions.get_mut(&id) else {
                    return;
                };
                match session.lod_queue.pop_front() {
                    Some(k) => k,
                    None => break,
                }
            };
            self.stream_lod(id, key);
        }
    }

    fn refresh_streaming(&mut self, id: EntityId) {
        let (pcx, pcz) = self.player_col(id);
        let rd = self.cfg.render_distance;
        let ld = self.cfg.lod_distance;

        // Queue missing full columns, nearest first.
        let mut wanted: Vec<ColumnKey> = Vec::new();
        for dx in -rd..=rd {
            for dz in -rd..=rd {
                wanted.push((pcx + dx, pcz + dz));
            }
        }
        wanted.sort_by_key(|(cx, cz)| (cx - pcx).abs() + (cz - pcz).abs());
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        for key in wanted {
            if !session.streamed.contains(&key) && !session.stream_queue.contains(&key) {
                session.stream_queue.push_back(key);
            }
        }

        // Queue missing LOD columns, nearest first.
        let mut lod_wanted: Vec<ColumnKey> = Vec::new();
        for dx in -ld..=ld {
            for dz in -ld..=ld {
                lod_wanted.push((pcx + dx, pcz + dz));
            }
        }
        lod_wanted.sort_by_key(|(cx, cz)| (cx - pcx).abs() + (cz - pcz).abs());
        for key in lod_wanted {
            if !session.lod_streamed.contains(&key) && !session.lod_queue.contains(&key) {
                session.lod_queue.push_back(key);
            }
        }

        // Unload what fell out of range.
        let out_of_range: Vec<ColumnKey> = session
            .streamed
            .iter()
            .filter(|(cx, cz)| (cx - pcx).abs() > rd + 1 || (cz - pcz).abs() > rd + 1)
            .copied()
            .collect();
        for key in out_of_range {
            session.streamed.remove(&key);
            let _ = session.wire.send(
                &ServerPacket::UnloadColumn {
                    cx: key.0,
                    cz: key.1,
                }
                .encode(),
            );
        }
        let lod_out: Vec<ColumnKey> = session
            .lod_streamed
            .iter()
            .filter(|(cx, cz)| (cx - pcx).abs() > ld + 1 || (cz - pcz).abs() > ld + 1)
            .copied()
            .collect();
        for key in lod_out {
            session.lod_streamed.remove(&key);
            let _ = session.wire.send(
                &ServerPacket::UnloadLod {
                    cx: key.0,
                    cz: key.1,
                }
                .encode(),
            );
        }
        session.last_player_col = (pcx, pcz);
    }

    /// Ensure a column exists server-side and stream it fully to one player.
    fn stream_column(&mut self, id: EntityId, key: ColumnKey) {
        self.ensure_column(key);
        // Collect sub-chunk payloads first (immutable borrow of the world),
        // then send (mutable borrow of the session). Wire layout:
        // SubChunk::to_wire — the single source of truth.
        let payloads: Vec<(u8, Vec<u16>)> = {
            let column = self.world.column(key.0, key.1).unwrap();
            let mut out = Vec::new();
            for (sub, slot) in column.subchunks.iter().enumerate() {
                if let Some(sc) = slot {
                    out.push((sub as u8, sc.to_wire()));
                }
            }
            out
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        for (sub, blocks) in payloads {
            let _ = session.wire.send(
                &ServerPacket::Subchunk {
                    cx: key.0,
                    cz: key.1,
                    sub,
                    blocks,
                }
                .encode(),
            );
        }
        let _ = session.wire.send(
            &ServerPacket::ColumnEnd {
                cx: key.0,
                cz: key.1,
            }
            .encode(),
        );
        session.streamed.insert(key);
    }

    fn ensure_column(&mut self, key: ColumnKey) {
        if self.world.column(key.0, key.1).is_none() {
            let mut col = self.gen.generate_column(key.0, key.1);
            if let Some(edits) = self.block_edits.get(&key) {
                for (&(x, y, z), &block) in edits {
                    col.set(x, y, z, block);
                }
            }
            self.world.columns.insert(key, col);
        }
    }

    /// Stream the LOD summary of one column to one player.
    fn stream_lod(&mut self, id: EntityId, key: ColumnKey) {
        if self.edited.contains(&key) {
            self.ensure_column(key);
        }
        let cells = self
            .gen
            .lod_summary(key.0, key.1, &self.world, &self.edited);
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let _ = session.wire.send(
            &ServerPacket::LodColumn {
                cx: key.0,
                cz: key.1,
                cells,
            }
            .encode(),
        );
        session.lod_streamed.insert(key);
    }

    /// Drop server-side world memory that no session needs anymore.
    fn evict_unused_columns(&mut self) {
        let rd = self.cfg.render_distance;
        let ranges: Vec<(i32, i32)> = self
            .sessions
            .values()
            .map(|s| {
                let p = &s.player.pos;
                (
                    (p.x.floor() as i32).div_euclid(16),
                    (p.z.floor() as i32).div_euclid(16),
                )
            })
            .collect();
        let mob_columns: HashSet<ColumnKey> = self
            .mobs
            .values()
            .flat_map(|mob| {
                let cx = (mob.pos.x.floor() as i32).div_euclid(16);
                let cz = (mob.pos.z.floor() as i32).div_euclid(16);
                (-1..=1).flat_map(move |dx| (-1..=1).map(move |dz| (cx + dx, cz + dz)))
            })
            .collect();
        self.world.columns.retain(|(cx, cz), _| {
            if self.edited.contains(&(*cx, *cz)) {
                return true;
            }
            if mob_columns.contains(&(*cx, *cz)) {
                return true;
            }
            ranges
                .iter()
                .any(|(pcx, pcz)| (cx - pcx).abs() <= rd + 4 && (cz - pcz).abs() <= rd + 4)
        });
    }
}

// ---------------------------------------------------------------------------
// Packet handling
// ---------------------------------------------------------------------------

impl Server {
    fn handle_packet(&mut self, id: EntityId, pkt: ClientPacket) -> Result<(), String> {
        match pkt {
            ClientPacket::Hello {
                version,
                player_id,
                name,
            } => self.on_hello(id, version, player_id, name),
            ClientPacket::Input { keys, yaw, pitch } => {
                if !yaw.is_finite() || !pitch.is_finite() {
                    return Err("non-finite input angles".into());
                }
                let creative = self.cfg.game_mode == GameMode::Creative;
                let tick = self.tick;
                let Some(session) = self.sessions.get_mut(&id) else {
                    return Ok(());
                };
                if !session.player.joined {
                    return Err("input before hello".into());
                }
                // Dead players give no input (server decides, specs.md §2).
                if !session.survival.dead {
                    let jump_pressed =
                        keys & keys::JUMP != 0 && session.player.keys & keys::JUMP == 0;
                    if creative && jump_pressed {
                        if session
                            .player
                            .last_jump_tick
                            .is_some_and(|last| tick.saturating_sub(last) <= 7 && tick > last)
                        {
                            session.player.flying = !session.player.flying;
                            if !session.player.flying {
                                session.player.vel.y = 0.0;
                            }
                        }
                        session.player.last_jump_tick = Some(tick);
                    }
                    session.player.keys = keys;
                }
                session.player.yaw = yaw;
                session.player.pitch = pitch.clamp(-89.9, 89.9);
            }
            ClientPacket::SelectSlot { slot } => {
                let Some(session) = self.sessions.get_mut(&id) else {
                    return Ok(());
                };
                let selected = slot % 9;
                if session.player.selected_slot != selected {
                    session.survival.digging = None;
                    session.eating = None;
                }
                session.player.selected_slot = selected;
            }
            ClientPacket::SetHotbarSlot { slot, block } => {
                // Creative only: validate against the registry, write the
                // item into the hotbar slot (infinite stack).
                if block == ssc_world::AIR || !ssc_world::valid_block_id(block) {
                    return Err(format!("invalid block id {block}"));
                }
                let Some(session) = self.sessions.get_mut(&id) else {
                    return Ok(());
                };
                if !session.player.joined || self.cfg.game_mode != GameMode::Creative {
                    return Ok(());
                }
                let idx = (slot as usize) % 9;
                session.inventory[idx] = Some((block, 64));
                session.durability[idx] = 0;
                session.inv_dirty = true;
            }
            ClientPacket::SwapSlots { a, b } => {
                // Own-inventory item movement (survival inventory UI).
                let Some(session) = self.sessions.get_mut(&id) else {
                    return Ok(());
                };
                if !session.player.joined || session.survival.dead {
                    return Ok(());
                }
                if (a as usize) >= crate::survival::INVENTORY_SIZE
                    || (b as usize) >= crate::survival::INVENTORY_SIZE
                {
                    return Err(format!("slot out of range: {a}/{b}"));
                }
                let a = a as usize;
                let b = b as usize;
                if a != b {
                    if a == session.player.selected_slot as usize
                        || b == session.player.selected_slot as usize
                    {
                        session.survival.digging = None;
                        session.eating = None;
                    }
                    match (session.inventory[a], session.inventory[b]) {
                        (Some((item, source_count)), Some((target_item, target_count)))
                            if item == target_item && ssc_world::item_info(item).max_stack > 1 =>
                        {
                            let capacity = ssc_world::item_info(item)
                                .max_stack
                                .saturating_sub(target_count);
                            let moved = capacity.min(source_count);
                            if moved > 0 {
                                session.inventory[b] = Some((item, target_count + moved));
                                session.inventory[a] = if source_count == moved {
                                    None
                                } else {
                                    Some((item, source_count - moved))
                                };
                                session.inv_dirty = true;
                            }
                        }
                        _ => {
                            session.inventory.swap(a, b);
                            session.durability.swap(a, b);
                            session.inv_dirty = true;
                        }
                    }
                }
            }
            ClientPacket::Craft { recipe } => {
                let Some(session) = self.sessions.get_mut(&id) else {
                    return Ok(());
                };
                if recipe == 8 {
                    if session.player.joined
                        && self.cfg.game_mode == GameMode::Survival
                        && !session.survival.dead
                        && crate::survival::repair_tool(
                            &mut session.inventory,
                            &mut session.durability,
                            session.player.selected_slot as usize,
                        )
                    {
                        session.inv_dirty = true;
                        session.survival.digging = None;
                        session.eating = None;
                    }
                    return Ok(());
                }
                let before = session.inventory.clone();
                if session.player.joined
                    && self.cfg.game_mode == GameMode::Survival
                    && !session.survival.dead
                    && crate::survival::craft(&mut session.inventory, recipe)
                {
                    for (i, slot) in session.inventory.iter().enumerate() {
                        if let Some((item, _)) = slot {
                            if crate::survival::max_durability(*item) > 0 && before[i] != *slot {
                                session.durability[i] = crate::survival::max_durability(*item);
                            }
                        }
                    }
                    session.inv_dirty = true;
                    session.survival.digging = None;
                    session.eating = None;
                }
            }
            request @ (ClientPacket::DropSelected | ClientPacket::DropStack) => {
                if self.cfg.game_mode != GameMode::Survival {
                    return Ok(());
                }
                let Some(session) = self.sessions.get(&id) else {
                    return Ok(());
                };
                if !session.player.joined || session.survival.dead {
                    return Ok(());
                }
                let slot = session.player.selected_slot as usize % 9;
                let Some((item, count)) = session.inventory[slot] else {
                    return Ok(());
                };
                let dropped_count = if matches!(request, ClientPacket::DropStack) {
                    count
                } else {
                    1
                };
                // Reserve the entity before mutating inventory, so an
                // exhausted entity-id range cannot silently delete an item.
                let Some(entity_id) = self.allocate_item_id() else {
                    return Ok(());
                };
                let Some(session) = self.sessions.get_mut(&id) else {
                    return Ok(());
                };
                let direction = look_dir(session.player.yaw, session.player.pitch);
                let pos =
                    session.player.pos + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0) + direction * 0.5;
                let durability =
                    crate::survival::effective_durability(item, session.durability[slot]);
                session.inventory[slot] = if count == dropped_count {
                    session.durability[slot] = 0;
                    None
                } else {
                    Some((item, count - dropped_count))
                };
                session.inv_dirty = true;
                session.survival.digging = None;
                session.eating = None;
                self.items.insert(
                    entity_id,
                    ItemEntity {
                        item,
                        count: dropped_count,
                        durability,
                        pos,
                        vel: direction * 4.0 + Vec3::new(0.0, 2.0, 0.0),
                        age: 0,
                        pickup_delay: 40,
                    },
                );
            }
            ClientPacket::DigStart => {
                if self
                    .sessions
                    .get(&id)
                    .is_some_and(|s| s.player.joined && !s.survival.dead)
                {
                    self.handle_dig_start(id);
                }
            }
            ClientPacket::DigStop => {
                let Some(session) = self.sessions.get_mut(&id) else {
                    return Ok(());
                };
                if let Some(dig) = session.survival.digging.take() {
                    let _ = self.broadcast(&ServerPacket::DigProgress {
                        x: dig.x,
                        y: dig.y,
                        z: dig.z,
                        progress: 101,
                    });
                }
            }
            ClientPacket::UseStop => {
                if let Some(session) = self.sessions.get_mut(&id) {
                    session.eating = None;
                }
            }
            ClientPacket::FurnaceSwap {
                inventory_slot,
                furnace_slot,
                from_furnace,
            } => {
                self.transfer_furnace(id, inventory_slot, furnace_slot, from_furnace)?;
            }
            ClientPacket::ChestSwap {
                inventory_slot,
                chest_slot,
                from_chest,
            } => {
                self.transfer_chest(id, inventory_slot, chest_slot, from_chest)?;
            }
            ClientPacket::CloseContainer => {
                if let Some(session) = self.sessions.get_mut(&id) {
                    session.open_furnace = None;
                    session.open_chest = None;
                }
            }
            ClientPacket::Place => {
                if self
                    .sessions
                    .get(&id)
                    .is_some_and(|s| s.player.joined && !s.survival.dead)
                {
                    self.handle_use(id);
                }
            }
        }
        Ok(())
    }

    fn on_hello(&mut self, id: EntityId, version: u32, player_id: [u8; 16], name: String) {
        if self
            .sessions
            .get(&id)
            .is_some_and(|session| session.player.joined)
        {
            return;
        }
        if version != PROTOCOL_VERSION {
            let _ = self.direct(
                id,
                &ServerPacket::Goodbye {
                    reason: format!(
                        "protocol mismatch: client {version}, server {PROTOCOL_VERSION}"
                    ),
                },
            );
            // The session will be reaped once the wire goes quiet/dead.
            if let Some(session) = self.sessions.get_mut(&id) {
                session.player.joined = false;
            }
            return;
        }
        if self.sessions.iter().any(|(other_id, session)| {
            *other_id != id && session.player.joined && session.player_id == Some(player_id)
        }) {
            let _ = self.direct(
                id,
                &ServerPacket::Goodbye {
                    reason: "this anonymous player ID is already connected".into(),
                },
            );
            return;
        }
        let (saved_player, player_data_writable) = match self.load_player_save(player_id) {
            Ok(data) => (data, true),
            Err(err) => {
                eprintln!("[server] player save could not be loaded: {err}");
                (None, false)
            }
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        // Death is an in-session timer, not a persistent state. If the player
        // quit after dying, reconnect them through the same fresh respawn path
        // rather than reviving at the saved death position with zero health.
        let saved_death = saved_player.as_ref().is_some_and(|data| data.health <= 0.0);
        let restored_respawn = saved_player.as_ref().and_then(|data| data.respawn);
        let restored_position = if saved_death {
            restored_respawn
        } else {
            saved_player.as_ref().and_then(|data| data.position)
        };
        if let Some(data) = saved_player {
            session.inventory = data.inventory;
            session.durability = data.durability;
            if saved_death {
                session.survival = crate::survival::SurvivalState::default();
            } else {
                session.survival.health = data.health;
                session.survival.hunger = data.hunger;
                session.survival.saturation = data.saturation;
                session.survival.air = data.air;
                session.survival.hunger_effect_ticks = data.hunger_effect_ticks;
                session.survival.exhaustion = data.exhaustion;
            }
            for index in 0..crate::survival::ARMOR_SIZE {
                session.armor[index] = data.armor.get(index).copied().flatten();
                session.armor_durability[index] =
                    data.armor_durability.get(index).copied().unwrap_or(0);
            }
            session.armor_dirty = true;
            session.xp_level = data.xp_level;
            session.xp_progress = data.xp_progress;
            session.player.selected_slot = data.selected_slot;
        }
        session.player_id = Some(player_id);
        session.player_data_writable = player_data_writable;
        session.player.name = name;

        // Find a spawn point on dry land: spiral out from a per-player
        // origin so players don't stack on each other.
        let origin = (id as i32).wrapping_mul(64);
        let mut spawn_x = (8 + origin) as f32 + 0.5;
        let mut spawn_z = (8 + origin) as f32 + 0.5;
        let mut spawn_y = 0f32;
        'search: for r in 0..48i32 {
            for dx in -r..=r {
                for dz in -r..=r {
                    if dx.abs() != r && dz.abs() != r {
                        continue; // ring walk
                    }
                    let h = self.gen.height(8 + origin + dx * 16, 8 + origin + dz * 16);
                    if h > ssc_world::MIN_Y + 20 && h > crate::worldgen::SEA_LEVEL + 2 {
                        spawn_x = (8 + origin + dx * 16) as f32 + 0.5;
                        spawn_z = (8 + origin + dz * 16) as f32 + 0.5;
                        spawn_y = (h + 1) as f32 + 0.02;
                        break 'search;
                    }
                }
            }
        }
        session.player.pos = restored_position
            .map(|pos| Vec3::new(pos[0], pos[1], pos[2]))
            .unwrap_or_else(|| Vec3::new(spawn_x, spawn_y, spawn_z));
        // A saved logout position is not the world's respawn point; a saved
        // bed or previous respawn point takes precedence.
        session.spawn = restored_respawn
            .map(|p| Vec3::new(p[0], p[1], p[2]))
            .unwrap_or_else(|| Vec3::new(spawn_x, spawn_y, spawn_z));
        session.player.vel = Vec3::ZERO;
        session.player.joined = true;

        let hotbar: [u16; 9] = {
            let mut hb = [0u16; 9];
            for (i, slot) in session.inventory.iter().take(9).enumerate() {
                if let Some((item, _)) = slot {
                    hb[i] = *item;
                }
            }
            hb
        };
        let spawn = [
            session.player.pos.x,
            session.player.pos.y,
            session.player.pos.z,
        ];
        session.inv_dirty = true;
        session.status_dirty = true;
        let game_mode = self.cfg.game_mode.as_u8();
        let _ = self.direct(
            id,
            &ServerPacket::Welcome {
                entity_id: id,
                spawn,
                render_distance: self.cfg.render_distance as u8,
                hotbar,
                game_mode,
            },
        );
        // The creative palette: every registered block, server-driven
        // like all gameplay data (specs.md §2).
        let palette: Vec<BlockId> = (1..ssc_world::BLOCK_COUNT)
            .filter(|block| {
                !matches!(
                    *block,
                    ssc_world::OAK_SAPLING_READY
                        | ssc_world::GROWN_OAK_LEAVES
                        | ssc_world::LIT_FURNACE
                        | ssc_world::FARMLAND_WET
                        | ssc_world::WHEAT_1
                        | ssc_world::WHEAT_2
                        | ssc_world::WHEAT_3
                        | ssc_world::WHEAT_4
                        | ssc_world::WHEAT_5
                        | ssc_world::WHEAT_6
                        | ssc_world::WHEAT_7
                        | ssc_world::DOOR_UPPER
                        | ssc_world::DOOR_OPEN
                        | ssc_world::DOOR_OPEN_UPPER
                )
            })
            .collect();
        let _ = self.direct(id, &ServerPacket::CreativeBlocks { ids: palette });
        // Movement is simulated before the bounded streaming queue is drained.
        // Generate collision terrain at the joined (possibly saved) position
        // now so the first server tick cannot fall through unloaded air.
        let (pcx, pcz) = self.player_col(id);
        for dx in -1..=1 {
            for dz in -1..=1 {
                self.ensure_column((pcx + dx, pcz + dz));
            }
        }
        self.refresh_streaming(id);
    }

    // -----------------------------------------------------------------------
    // Interaction (dig / place) — the server decides everything.
    // -----------------------------------------------------------------------

    fn eye_of(&self, id: EntityId) -> Vec3 {
        let p = &self.sessions[&id].player.pos;
        *p + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0)
    }

    fn look_of(&self, id: EntityId) -> (f32, f32) {
        let p = &self.sessions[&id].player;
        (p.yaw, p.pitch)
    }

    fn selected_item(&self, id: EntityId) -> Option<(ssc_world::ItemId, u8)> {
        let session = &self.sessions[&id];
        session
            .inventory
            .get(session.player.selected_slot as usize % 9)
            .copied()
            .flatten()
    }

    /// Start digging or punching: entity hit → attack; otherwise creative
    /// breaks instantly, survival begins timed progress.
    fn handle_dig_start(&mut self, id: EntityId) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.eating = None;
        }
        if self.try_attack(id) {
            return;
        }
        let (yaw, pitch) = self.look_of(id);
        let dir = look_dir(yaw, pitch);
        let Some(hit) = raycast(&self.world, self.eye_of(id), dir, REACH) else {
            return;
        };
        let [x, y, z] = hit.block;
        if self.cfg.game_mode == GameMode::Creative {
            if self
                .selected_item(id)
                .is_some_and(|(item, _)| ssc_world::is_sword(item))
            {
                return;
            }
            self.break_block(id, x, y, z);
        } else {
            let block = self.world.block_or_air(x, y, z);
            if crate::survival::dig_time_with_tool(
                block,
                self.selected_item(id).map(|(item, _)| item),
            )
            .is_some()
            {
                if let Some(session) = self.sessions.get_mut(&id) {
                    session.survival.digging = Some(crate::survival::DigState {
                        x,
                        y,
                        z,
                        progress: 0.0,
                    });
                }
            }
        }
    }

    /// Break a block: world edit, sounds, LOD refresh, and (survival)
    /// the item drop into the digger's inventory.
    fn break_block(&mut self, id: EntityId, x: i32, y: i32, z: i32) {
        let block = self.world.block_or_air(x, y, z);
        if !self.world.set_block(x, y, z, AIR) {
            return;
        }
        self.record_block_edit(x, y, z, AIR);
        if let Some(furnace) = self.furnaces.remove(&(x, y, z)) {
            for (item, count) in furnace.slots.into_iter().flatten() {
                self.spawn_item_entity(
                    item,
                    count,
                    Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5),
                );
            }
        }
        if let Some(chest) = self.chests.remove(&(x, y, z)) {
            for (slot, &wear) in chest.slots.iter().zip(&chest.durability) {
                if let Some((item, count)) = slot {
                    self.spawn_item_entity_with_durability(
                        *item,
                        *count,
                        Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5),
                        crate::survival::effective_durability(*item, wear),
                    );
                }
            }
            let viewers: Vec<EntityId> = self
                .sessions
                .iter_mut()
                .filter_map(|(session_id, session)| {
                    (session.open_chest == Some((x, y, z))).then(|| {
                        session.open_chest = None;
                        *session_id
                    })
                })
                .collect();
            for viewer in viewers {
                let _ = self.direct(viewer, &ServerPacket::ContainerClosed);
            }
        }
        let _ = self.broadcast(&ServerPacket::BlockUpdate {
            x,
            y,
            z,
            block: AIR,
        });
        self.send_sound(SoundId::Dig, x, y, z);
        // LOD must reflect the edit (server-driven LOD, specs.md §3).
        self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));

        if ssc_world::is_door(block) {
            let partner_y = if matches!(block, ssc_world::DOOR_UPPER | ssc_world::DOOR_OPEN_UPPER) {
                y - 1
            } else {
                y + 1
            };
            if ssc_world::is_door(self.world.block_or_air(x, partner_y, z)) {
                self.world.set_block(x, partner_y, z, AIR);
                self.record_block_edit(x, partner_y, z, AIR);
                let _ = self.broadcast(&ServerPacket::BlockUpdate {
                    x,
                    y: partner_y,
                    z,
                    block: AIR,
                });
            }
        }

        // Survival: deposit into inventory, then spawn any overflow as a
        // server-owned item entity.
        if self.cfg.game_mode == GameMode::Survival {
            if let Some(session) = self.sessions.get_mut(&id) {
                // Breaking a block costs exhaustion even when it drops
                // nothing (wrong tool, leaves or an unharvestable block).
                session.survival.exhaustion += 0.005;
            }
            let roll = hash_tick(self.tick, x, z);
            for (item, count) in crate::survival::block_drops(
                block,
                roll,
                self.selected_item(id).map(|(item, _)| item),
            ) {
                let leftover = {
                    let Some(session) = self.sessions.get_mut(&id) else {
                        return;
                    };
                    let inv = &mut session.inventory;
                    crate::survival::add_item(inv, item, count)
                };
                if leftover < count {
                    if let Some(session) = self.sessions.get_mut(&id) {
                        session.inv_dirty = true;
                    }
                }
                if leftover > 0 {
                    self.spawn_item_entity(
                        item,
                        leftover,
                        Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5),
                    );
                }
            }
        }
        self.settle_gravity_stack(x, y + 1, z);
        if ssc_world::is_oak_sapling(self.world.block_or_air(x, y + 1, z))
            && !crate::block_ticks::can_plant_sapling(&self.world, x, y + 1, z)
        {
            self.world.set_block(x, y + 1, z, AIR);
            self.record_block_edit(x, y + 1, z, AIR);
            let _ = self.broadcast(&ServerPacket::BlockUpdate {
                x,
                y: y + 1,
                z,
                block: AIR,
            });
            self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
            self.spawn_item_entity(
                ssc_world::OAK_SAPLING,
                1,
                Vec3::new(x as f32 + 0.5, y as f32 + 1.5, z as f32 + 0.5),
            );
        }
        if ssc_world::is_wheat(self.world.block_or_air(x, y + 1, z)) {
            let crop = self.world.block_or_air(x, y + 1, z);
            self.world.set_block(x, y + 1, z, AIR);
            self.record_block_edit(x, y + 1, z, AIR);
            let _ = self.broadcast(&ServerPacket::BlockUpdate {
                x,
                y: y + 1,
                z,
                block: AIR,
            });
            self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
            for (item, count) in
                crate::survival::block_drops(crop, hash_tick(self.tick, x, z.wrapping_add(1)), None)
            {
                self.spawn_item_entity(
                    item,
                    count,
                    Vec3::new(x as f32 + 0.5, y as f32 + 1.5, z as f32 + 0.5),
                );
            }
        }
        if self.cfg.game_mode == GameMode::Survival && ssc_world::block_info(block).hardness > 0.0 {
            if let Some(session) = self.sessions.get_mut(&id) {
                let sword = session.inventory[session.player.selected_slot as usize % 9]
                    .is_some_and(|(item, _)| ssc_world::is_sword(item));
                damage_held_tool(session, if sword { 2 } else { 1 });
            }
        }
    }

    /// Settle the contiguous sand/gravel stack above a changed support.
    /// This is an immediate server-side block update; animated falling
    /// entities, water displacement and entity suffocation are future work.
    fn settle_gravity_stack(&mut self, x: i32, start_y: i32, z: i32) {
        let mut y = start_y;
        while y < ssc_world::MAX_Y {
            let block = self.world.block_or_air(x, y, z);
            if !matches!(block, ssc_world::SAND | ssc_world::GRAVEL) {
                break;
            }
            let mut dest = y;
            while dest > ssc_world::MIN_Y && self.world.block_or_air(x, dest - 1, z) == AIR {
                dest -= 1;
            }
            if dest != y {
                if !self.world.set_block(x, dest, z, block) {
                    break;
                }
                self.world.set_block(x, y, z, AIR);
                self.record_block_edit(x, y, z, AIR);
                self.record_block_edit(x, dest, z, block);
                let _ = self.broadcast(&ServerPacket::BlockUpdate {
                    x,
                    y,
                    z,
                    block: AIR,
                });
                let _ = self.broadcast(&ServerPacket::BlockUpdate {
                    x,
                    y: dest,
                    z,
                    block,
                });
                self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
            }
            y += 1;
        }
    }

    /// Right-click use: eat the selected food, or place the selected
    /// block item (survival consumes, creative does not).
    fn use_bucket(&mut self, id: EntityId, item: ssc_world::ItemId) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        if !session.player.joined || session.survival.dead {
            return;
        }
        let dir = look_dir(session.player.yaw, session.player.pitch);
        let empty = item == ssc_world::BUCKET;
        let hit = if empty {
            crate::raycast::raycast_water(&self.world, self.eye_of(id), dir, REACH)
        } else {
            raycast(&self.world, self.eye_of(id), dir, REACH)
        };
        let Some(hit) = hit else { return };
        let [hx, hy, hz] = hit.block;
        let [nx, ny, nz] = hit.normal;
        let (x, y, z) = if empty {
            (hx, hy, hz)
        } else {
            (hx + nx, hy + ny, hz + nz)
        };
        if !(ssc_world::MIN_Y..ssc_world::MAX_Y).contains(&y) {
            return;
        }
        let expected = if empty { ssc_world::WATER } else { AIR };
        if self.world.get_block(x, y, z) != Some(expected) {
            return;
        }
        let slot = session.player.selected_slot as usize % 9;
        let Some((held, count)) = session.inventory[slot] else {
            return;
        };
        if held != item {
            return;
        }
        let mut inventory = session.inventory.clone();
        let result = if empty {
            ssc_world::WATER_BUCKET
        } else {
            ssc_world::BUCKET
        };
        let overflow = if self.cfg.game_mode == GameMode::Creative {
            if empty {
                inventory[slot] = Some((result, 1));
            }
            0
        } else if count == 1 {
            inventory[slot] = Some((result, 1));
            0
        } else {
            inventory[slot] = Some((held, count - 1));
            crate::survival::add_item(&mut inventory, result, 1)
        };
        let drop_pos = session.player.pos + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
        // Reserve overflow capacity before removing water or consuming a bucket.
        let overflow_id = if overflow > 0 {
            let Some(id) = self.allocate_item_id() else {
                return;
            };
            Some(id)
        } else {
            None
        };
        let block = if empty { AIR } else { ssc_world::WATER };
        self.world.set_block(x, y, z, block);
        self.record_block_edit(x, y, z, block);
        let _ = self.broadcast(&ServerPacket::BlockUpdate { x, y, z, block });
        self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
        self.update_farmland_moisture(x, y, z);
        self.send_sound(SoundId::Place, x, y, z);
        let session = self.sessions.get_mut(&id).unwrap();
        session.inventory = inventory;
        session.durability[slot] = 0;
        session.inv_dirty = true;
        session.eating = None;
        session.survival.digging = None;
        if let Some(entity_id) = overflow_id {
            self.items.insert(
                entity_id,
                ItemEntity {
                    item: result,
                    count: overflow,
                    durability: 0,
                    pos: drop_pos,
                    vel: dir * 2.0,
                    age: 0,
                    pickup_delay: 10,
                },
            );
        }
    }

    /// Update wet/dry farmland state around a water edit. Every changed cell
    /// is a normal persisted block update, so reloads keep hydration.
    fn update_farmland_moisture(&mut self, x: i32, y: i32, z: i32) {
        let mut changes = Vec::new();
        for fx in x - 4..=x + 4 {
            for fz in z - 4..=z + 4 {
                for fy in y - 1..=y + 1 {
                    let block = self.world.block_or_air(fx, fy, fz);
                    if !ssc_world::is_farmland(block) {
                        continue;
                    }
                    let wet = crate::block_ticks::farmland_should_be_wet(&self.world, fx, fy, fz);
                    let desired = if wet {
                        ssc_world::FARMLAND_WET
                    } else {
                        ssc_world::FARMLAND
                    };
                    if block != desired {
                        changes.push(((fx, fy, fz), desired));
                    }
                }
            }
        }
        let mut dirty = HashSet::new();
        for ((x, y, z), block) in changes {
            self.world.set_block(x, y, z, block);
            self.record_block_edit(x, y, z, block);
            let _ = self.broadcast(&ServerPacket::BlockUpdate { x, y, z, block });
            dirty.insert((x.div_euclid(16), z.div_euclid(16)));
        }
        for column in dirty {
            self.refresh_lod_for(column);
        }
    }

    fn fire_bow(&mut self, id: EntityId) {
        let (arrow_slot, pos, dir) = {
            let Some(session) = self.sessions.get(&id) else {
                return;
            };
            if !session.player.joined || session.survival.dead || session.player.bow_cooldown > 0 {
                return;
            }
            let arrow_slot = session.inventory.iter().position(|slot| {
                matches!(slot, Some((item, count)) if *item == ssc_world::ARROW && *count > 0)
            });
            if self.cfg.game_mode == GameMode::Survival && arrow_slot.is_none() {
                return;
            }
            let dir = look_dir(session.player.yaw, session.player.pitch);
            let pos = session.player.pos + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0) + dir * 0.4;
            (arrow_slot, pos, dir)
        };
        let Some(arrow_id) = self.allocate_item_id() else {
            return;
        };
        if self.cfg.game_mode == GameMode::Survival {
            if let Some(slot) = arrow_slot {
                if let Some(session) = self.sessions.get_mut(&id) {
                    if let Some((item, count)) = session.inventory[slot] {
                        session.inventory[slot] = if count <= 1 {
                            None
                        } else {
                            Some((item, count - 1))
                        };
                        session.inv_dirty = true;
                    }
                }
            }
        }
        if let Some(session) = self.sessions.get_mut(&id) {
            session.player.bow_cooldown = BOW_COOLDOWN_TICKS;
            if self.cfg.game_mode == GameMode::Survival {
                damage_held_tool(session, 1);
            }
        }
        self.arrows.insert(
            arrow_id,
            ArrowEntity {
                owner: id,
                pos,
                vel: dir * ARROW_SPEED,
                age: 0,
            },
        );
        let packet = arrow_state(arrow_id, &self.arrows[&arrow_id]);
        let _ = self.broadcast(&packet);
    }

    /// Advance arrows, collide with blocks and entities, and resolve hits.
    fn tick_arrows(&mut self, dt: f32) {
        let mut removal = Vec::new();
        let mut hits = Vec::new();
        {
            let world = &self.world;
            let mobs = &self.mobs;
            let sessions = &self.sessions;
            for (id, arrow) in self.arrows.iter_mut() {
                arrow.age = arrow.age.saturating_add(1);
                if arrow.age > ARROW_LIFETIME_TICKS || arrow.pos.y < ssc_world::MIN_Y as f32 {
                    removal.push(*id);
                    continue;
                }
                arrow.vel.y -= ssc_math::GRAVITY * 0.35 * dt;
                let step = arrow.vel * dt;
                let len = step.length();
                if len < 1e-5 {
                    continue;
                }
                let dir = step / len;
                if crate::raycast::raycast_solids(world, arrow.pos, dir, len).is_some() {
                    removal.push(*id);
                    continue;
                }
                let mut best: Option<(bool, EntityId, f32)> = None;
                for (mid, mob) in mobs {
                    if mob.dead {
                        continue;
                    }
                    let (hw, height) = mob.kind.dims();
                    if let Some(t) = ray_aabb(arrow.pos, dir, &entity_aabb(mob.pos, hw, height)) {
                        if (0.0..=len).contains(&t) && best.map(|(_, _, bt)| t < bt).unwrap_or(true)
                        {
                            best = Some((true, *mid, t));
                        }
                    }
                }
                for (pid, session) in sessions {
                    if *pid == arrow.owner || !session.player.joined || session.survival.dead {
                        continue;
                    }
                    if let Some(t) = ray_aabb(arrow.pos, dir, &player_aabb(session.player.pos)) {
                        if (0.0..=len).contains(&t) && best.map(|(_, _, bt)| t < bt).unwrap_or(true)
                        {
                            best = Some((false, *pid, t));
                        }
                    }
                }
                if let Some((is_mob, target, t)) = best {
                    hits.push((arrow.owner, arrow.pos, is_mob, target, arrow.pos + dir * t));
                    removal.push(*id);
                    continue;
                }
                arrow.pos += step;
            }
        }
        let states: Vec<_> = self
            .arrows
            .iter()
            .filter(|(id, _)| !removal.contains(id))
            .map(|(id, arrow)| arrow_state(*id, arrow))
            .collect();
        for packet in states {
            let _ = self.broadcast(&packet);
        }
        for (owner, from, is_mob, target, at) in hits {
            if is_mob {
                let mut killed = None;
                if let Some(mob) = self.mobs.get_mut(&target) {
                    if mob.hurt(ARROW_DAMAGE, from) {
                        killed = Some(mob.kind);
                    }
                }
                if let Some(kind) = killed {
                    let xp = crate::mobs::xp_for(
                        kind,
                        hash_tick(self.tick, target as i32, at.x.floor() as i32),
                    );
                    self.award_xp(owner, xp);
                }
            } else if self.cfg.game_mode == GameMode::Survival {
                if let Some(session) = self.sessions.get_mut(&target) {
                    let mut fx = crate::survival::TickEffects::default();
                    let points = crate::survival::armor_points(&session.armor);
                    session.survival.apply_combat_damage(
                        ARROW_DAMAGE * (1.0 - crate::survival::armor_reduction(points)),
                        &mut fx,
                    );
                    if fx.hurt {
                        damage_armor(session, 1);
                        session.status_dirty = true;
                        knock_player_back(&mut session.player, from);
                    }
                }
            }
        }
        for id in removal {
            if self.arrows.remove(&id).is_some() {
                let _ = self.broadcast(&ServerPacket::EntityRemove { id });
            }
        }
    }

    /// Award experience directly (no orb entities yet).
    fn award_xp(&mut self, id: EntityId, points: u32) {
        if points == 0 || self.cfg.game_mode != GameMode::Survival {
            return;
        }
        let leveled_at = {
            let Some(session) = self.sessions.get_mut(&id) else {
                return;
            };
            if !session.player.joined || session.survival.dead {
                return;
            }
            let before = session.xp_level;
            crate::survival::add_xp(&mut session.xp_level, &mut session.xp_progress, points);
            session.status_dirty = true;
            (session.xp_level != before).then_some(session.player.pos)
        };
        if let Some(pos) = leveled_at {
            self.send_sound(SoundId::Eat, pos.x as i32, pos.y as i32, pos.z as i32);
        }
    }

    /// First mob hit by the player's crosshair, without dealing damage.
    fn target_mob(&self, id: EntityId) -> Option<EntityId> {
        let (yaw, pitch) = self.look_of(id);
        let eye = self.eye_of(id);
        let dir = look_dir(yaw, pitch);
        let mut best: Option<(EntityId, f32)> = None;
        for (mid, mob) in &self.mobs {
            if mob.dead {
                continue;
            }
            let (hw, height) = mob.kind.dims();
            if let Some(t) = ray_aabb(eye, dir, &entity_aabb(mob.pos, hw, height)) {
                if t <= MELEE_REACH && best.map(|(_, bt)| t < bt).unwrap_or(true) {
                    best = Some((*mid, t));
                }
            }
        }
        let (mid, t) = best?;
        if crate::raycast::raycast_solids(&self.world, eye, dir, (t - 0.0001).max(0.0)).is_some() {
            return None;
        }
        Some(mid)
    }

    fn milk_cow(&mut self, id: EntityId) -> bool {
        let Some(target) = self.target_mob(id) else {
            return false;
        };
        if self
            .mobs
            .get(&target)
            .is_none_or(|mob| mob.kind != crate::mobs::MobKind::Cow || mob.dead)
        {
            return false;
        }
        if self.cfg.game_mode == GameMode::Creative {
            return true;
        }
        let slot = self.sessions[&id].player.selected_slot as usize % 9;
        let Some((item, count)) = self.sessions[&id].inventory[slot] else {
            return false;
        };
        if item != ssc_world::BUCKET {
            return false;
        }
        let mut inventory = self.sessions[&id].inventory.clone();
        let overflow = if count == 1 {
            inventory[slot] = Some((ssc_world::MILK_BUCKET, 1));
            0
        } else {
            inventory[slot] = Some((ssc_world::BUCKET, count - 1));
            crate::survival::add_item(&mut inventory, ssc_world::MILK_BUCKET, 1)
        };
        if overflow > 0 {
            return false;
        }
        let session = self.sessions.get_mut(&id).unwrap();
        session.inventory = inventory;
        session.durability[slot] = 0;
        session.inv_dirty = true;
        session.eating = None;
        session.survival.digging = None;
        true
    }

    fn drink_milk(&mut self, id: EntityId) {
        if self.cfg.game_mode != GameMode::Survival {
            return;
        }
        let slot = self.sessions[&id].player.selected_slot as usize % 9;
        let Some((item, count)) = self.sessions[&id].inventory[slot] else {
            return;
        };
        if item != ssc_world::MILK_BUCKET || count != 1 {
            return;
        }
        let session = self.sessions.get_mut(&id).unwrap();
        session.inventory[slot] = Some((ssc_world::BUCKET, 1));
        session.durability[slot] = 0;
        session.survival.hunger_effect_ticks = 0;
        session.survival.digging = None;
        session.eating = None;
        session.inv_dirty = true;
        session.status_dirty = true;
    }

    fn use_shears(&mut self, id: EntityId) {
        let Some(target) = self.target_mob(id) else {
            return;
        };
        let sheared = {
            let Some(mob) = self.mobs.get_mut(&target) else {
                return;
            };
            if mob.kind != crate::mobs::MobKind::Sheep || mob.sheared {
                return;
            }
            mob.sheared = true;
            mob.regrow_timer = 0;
            mob.pos
        };
        let count = 1 + (hash_tick(self.tick, target as i32, sheared.x.floor() as i32) % 3) as u8;
        self.spawn_item_entity(ssc_world::WOOL, count, sheared);
        if self.cfg.game_mode == GameMode::Survival {
            if let Some(session) = self.sessions.get_mut(&id) {
                damage_held_tool(session, 1);
            }
        }
    }

    fn equip_armor(&mut self, id: EntityId, item: ssc_world::ItemId) {
        let Some(slot) = ssc_world::items::armor_slot(item) else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if !session.player.joined || session.survival.dead {
            return;
        }
        let hotbar = session.player.selected_slot as usize % 9;
        let Some((held, count)) = session.inventory[hotbar] else {
            return;
        };
        if held != item || count != 1 {
            return;
        }
        let new_wear = crate::survival::effective_durability(item, session.durability[hotbar]);
        if self.cfg.game_mode == GameMode::Creative {
            // Creative keeps its palette item and replaces worn gear freely.
            session.armor[slot] = Some((item, 1));
            session.armor_durability[slot] = new_wear;
        } else {
            let old = session.armor[slot].replace((item, 1));
            let old_wear = session.armor_durability[slot];
            session.armor_durability[slot] = new_wear;
            match old {
                Some((old_item, old_count)) => {
                    session.inventory[hotbar] = Some((old_item, old_count));
                    session.durability[hotbar] =
                        crate::survival::effective_durability(old_item, old_wear);
                }
                None => {
                    session.inventory[hotbar] = None;
                    session.durability[hotbar] = 0;
                }
            }
        }
        session.inv_dirty = true;
        session.armor_dirty = true;
        session.eating = None;
        session.survival.digging = None;
    }

    /// Toggle both halves of the door containing this block.
    fn toggle_door(&mut self, x: i32, y: i32, z: i32) {
        let block = self.world.block_or_air(x, y, z);
        if !ssc_world::is_door(block) {
            return;
        }
        let lower_y = if matches!(block, ssc_world::DOOR_UPPER | ssc_world::DOOR_OPEN_UPPER) {
            y - 1
        } else {
            y
        };
        let open = matches!(self.world.block_or_air(x, lower_y, z), ssc_world::DOOR_OPEN);
        let (lower, upper) = ssc_world::door_pair(!open);
        for (block_y, block_id) in [(lower_y, lower), (lower_y + 1, upper)] {
            if self.world.block_or_air(x, block_y, z) == block_id {
                continue;
            }
            self.world.set_block(x, block_y, z, block_id);
            self.record_block_edit(x, block_y, z, block_id);
            let _ = self.broadcast(&ServerPacket::BlockUpdate {
                x,
                y: block_y,
                z,
                block: block_id,
            });
        }
        self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
        self.send_sound(SoundId::Place, x, y, z);
    }

    /// Sleep in a bed at night: skip to dawn and set the respawn point.
    /// Only zombies count as nearby monsters for now.
    fn use_bed(&mut self, id: EntityId, pos: (i32, i32, i32)) -> bool {
        let Some(session) = self.sessions.get(&id) else {
            return false;
        };
        if !session.player.joined || session.survival.dead {
            return false;
        }
        if !(self.time_of_day > 0.75 && self.time_of_day < 0.98) {
            return false;
        }
        if self.mobs.values().any(|mob| {
            !mob.dead
                && mob.kind == crate::mobs::MobKind::Zombie
                && (mob.pos - session.player.pos).length() < 8.0
        }) {
            return false;
        }
        if is_solid(self.world.block_or_air(pos.0, pos.1 + 1, pos.2))
            || is_solid(self.world.block_or_air(pos.0, pos.1 + 2, pos.2))
        {
            return false;
        }
        let spawn = Vec3::new(pos.0 as f32 + 0.5, pos.1 as f32 + 1.0, pos.2 as f32 + 0.5);
        let session = self.sessions.get_mut(&id).unwrap();
        session.spawn = spawn;
        session.survival.digging = None;
        session.eating = None;
        session.open_furnace = None;
        self.time_of_day = 0.26;
        let _ = self.broadcast(&ServerPacket::TimeOfDay {
            t: self.time_of_day,
        });
        self.save_player_data(id);
        true
    }

    /// Hoe use: till grass or dirt into farmland when the space above is
    /// clear. Water within four blocks decides the initial wet state.
    fn use_hoe(&mut self, id: EntityId) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        if !session.player.joined || session.survival.dead {
            return;
        }
        let dir = look_dir(session.player.yaw, session.player.pitch);
        let Some(hit) = raycast(&self.world, self.eye_of(id), dir, REACH) else {
            return;
        };
        let [x, y, z] = hit.block;
        if !matches!(
            self.world.block_or_air(x, y, z),
            ssc_world::GRASS_BLOCK | ssc_world::DIRT
        ) || y + 1 >= ssc_world::MAX_Y
            || self.world.block_or_air(x, y + 1, z) != AIR
        {
            return;
        }
        let wet = crate::block_ticks::farmland_should_be_wet(&self.world, x, y, z);
        let block = if wet {
            ssc_world::FARMLAND_WET
        } else {
            ssc_world::FARMLAND
        };
        self.world.set_block(x, y, z, block);
        self.record_block_edit(x, y, z, block);
        let _ = self.broadcast(&ServerPacket::BlockUpdate { x, y, z, block });
        self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
        self.send_sound(SoundId::Place, x, y, z);
        let session = self.sessions.get_mut(&id).unwrap();
        session.survival.digging = None;
        session.eating = None;
        if self.cfg.game_mode == GameMode::Survival {
            damage_held_tool(session, 1);
        }
    }

    fn handle_use(&mut self, id: EntityId) {
        if let Some(session) = self.sessions.get(&id) {
            if session.player.keys & keys::SNEAK == 0 {
                let (yaw, pitch) = self.look_of(id);
                if let Some(hit) =
                    raycast(&self.world, self.eye_of(id), look_dir(yaw, pitch), REACH)
                {
                    let [x, y, z] = hit.block;
                    let pos = (x, y, z);
                    if ssc_world::is_door(self.world.block_or_air(x, y, z)) {
                        self.toggle_door(x, y, z);
                        return;
                    }
                    if self.world.block_or_air(x, y, z) == ssc_world::BED && self.use_bed(id, pos) {
                        return;
                    }
                    if self.world.block_or_air(x, y, z) == ssc_world::CHEST
                        && self.can_access_chest(id, pos)
                    {
                        self.chests.entry(pos).or_default();
                        let session = self.sessions.get_mut(&id).unwrap();
                        session.open_chest = Some(pos);
                        session.open_furnace = None;
                        session.eating = None;
                        session.survival.digging = None;
                        if let Some(packet) = self.chest_packet(pos, true) {
                            let _ = self.direct(id, &packet);
                        }
                        return;
                    }
                    if ssc_world::is_furnace(self.world.block_or_air(x, y, z))
                        && self.can_access_furnace(id, pos)
                    {
                        self.furnaces.entry(pos).or_default();
                        let session = self.sessions.get_mut(&id).unwrap();
                        session.open_furnace = Some(pos);
                        session.open_chest = None;
                        session.eating = None;
                        session.survival.digging = None;
                        if let Some(packet) = self.furnace_packet(pos, true) {
                            let _ = self.direct(id, &packet);
                        }
                        return;
                    }
                }
            }
        }
        let Some((item, count)) = self.selected_item(id) else {
            return;
        };
        if item == ssc_world::MILK_BUCKET {
            self.drink_milk(id);
            return;
        }
        if item == ssc_world::BUCKET && self.milk_cow(id) {
            return;
        }
        if matches!(item, ssc_world::BUCKET | ssc_world::WATER_BUCKET) {
            self.use_bucket(id, item);
            return;
        }
        if ssc_world::items::is_hoe(item) {
            self.use_hoe(id);
            return;
        }
        if item == ssc_world::BOW {
            self.fire_bow(id);
            return;
        }
        if item == ssc_world::SHEARS {
            self.use_shears(id);
            return;
        }
        if ssc_world::items::armor_slot(item).is_some() {
            self.equip_armor(id, item);
            return;
        }
        let item_info = ssc_world::item_info(item);

        // Eating (survival only — hunger doesn't exist in creative).
        if item_info.food.is_some() {
            if self.cfg.game_mode == GameMode::Survival {
                let session = self.sessions.get_mut(&id).unwrap();
                if session.eating.is_none()
                    && !session.survival.dead
                    && (session.survival.hunger < crate::survival::MAX_HUNGER
                        || item == ssc_world::APPLE)
                {
                    session.eating = Some(EatingState {
                        slot: session.player.selected_slot,
                        item,
                        remaining: crate::survival::EAT_TICKS,
                    });
                    session.survival.digging = None;
                }
            }
            return;
        }

        // Axe use transforms a targeted log in place; it is not a break or
        // placement and never creates loot or charges mining exhaustion.
        if matches!(
            item,
            ssc_world::WOODEN_AXE | ssc_world::STONE_AXE | ssc_world::IRON_AXE
        ) {
            let (yaw, pitch) = self.look_of(id);
            let Some(hit) = raycast(&self.world, self.eye_of(id), look_dir(yaw, pitch), REACH)
            else {
                return;
            };
            let [x, y, z] = hit.block;
            if self.world.block_or_air(x, y, z) == ssc_world::OAK_LOG {
                let block = ssc_world::STRIPPED_OAK_LOG;
                self.world.set_block(x, y, z, block);
                self.record_block_edit(x, y, z, block);
                let _ = self.broadcast(&ServerPacket::BlockUpdate { x, y, z, block });
                self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
                self.send_sound(SoundId::Place, x, y, z);
                if let Some(session) = self.sessions.get_mut(&id) {
                    session.survival.digging = None;
                    if self.cfg.game_mode == GameMode::Survival {
                        damage_held_tool(session, 1);
                    }
                }
            }
            return;
        }

        // Placing.
        let Some(block) = item_info.places else {
            return;
        };
        let (yaw, pitch) = self.look_of(id);
        let dir = look_dir(yaw, pitch);
        let Some(hit) = raycast(&self.world, self.eye_of(id), dir, REACH) else {
            return;
        };
        let [hx, hy, hz] = hit.block;
        let [nx, ny, nz] = hit.normal;
        let (x, y, z) = (hx + nx, hy + ny, hz + nz);

        if y < ssc_world::MIN_Y || y >= ssc_world::MAX_Y {
            return;
        }
        if is_solid(self.world.block_or_air(x, y, z)) {
            return;
        }
        if ssc_world::is_oak_sapling(block)
            && !crate::block_ticks::can_plant_sapling(&self.world, x, y, z)
        {
            return;
        }
        if ssc_world::is_wheat(block)
            && (block != ssc_world::WHEAT_0
                || ny != 1
                || !ssc_world::is_farmland(self.world.block_or_air(x, y - 1, z)))
        {
            return;
        }
        if block == ssc_world::LADDER {
            let supported = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|(dx, dz)| is_solid(self.world.block_or_air(x + dx, y, z + dz)));
            if !supported {
                return;
            }
        }
        if block == ssc_world::DOOR {
            if y + 1 >= ssc_world::MAX_Y
                || !is_solid(self.world.block_or_air(x, y - 1, z))
                || self.world.block_or_air(x, y, z) != AIR
                || self.world.block_or_air(x, y + 1, z) != AIR
            {
                return;
            }
            for session in self.sessions.values() {
                if session.player.joined && !session.survival.dead {
                    let body = player_aabb(session.player.pos);
                    if body.intersects(&ssc_math::Aabb::block(x, y, z))
                        || body.intersects(&ssc_math::Aabb::block(x, y + 1, z))
                    {
                        return;
                    }
                }
            }
            let (lower, upper) = ssc_world::door_pair(false);
            self.world.set_block(x, y, z, lower);
            self.world.set_block(x, y + 1, z, upper);
            self.record_block_edit(x, y, z, lower);
            self.record_block_edit(x, y + 1, z, upper);
            let _ = self.broadcast(&ServerPacket::BlockUpdate {
                x,
                y,
                z,
                block: lower,
            });
            let _ = self.broadcast(&ServerPacket::BlockUpdate {
                x,
                y: y + 1,
                z,
                block: upper,
            });
            self.send_sound(SoundId::Place, x, y, z);
            self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
            if self.cfg.game_mode == GameMode::Survival && count > 0 {
                let slot = self.sessions[&id].player.selected_slot as usize % 9;
                if let Some(session) = self.sessions.get_mut(&id) {
                    if let Some((i, c)) = session.inventory[slot] {
                        session.inventory[slot] = if c <= 1 { None } else { Some((i, c - 1)) };
                    }
                    session.inv_dirty = true;
                }
            }
            return;
        }
        // Never place inside any player.
        for session in self.sessions.values() {
            if is_solid(block)
                && session.player.joined
                && !session.survival.dead
                && player_aabb(session.player.pos).intersects(&ssc_math::Aabb::block(x, y, z))
            {
                return;
            }
        }

        if self.world.set_block(x, y, z, block) {
            self.record_block_edit(x, y, z, block);
            if ssc_world::is_furnace(block) {
                self.furnaces.entry((x, y, z)).or_default();
            }
            if block == ssc_world::CHEST {
                self.chests.entry((x, y, z)).or_default();
            }
            if block == ssc_world::WATER {
                self.update_farmland_moisture(x, y, z);
            }
            let _ = self.broadcast(&ServerPacket::BlockUpdate { x, y, z, block });
            self.send_sound(SoundId::Place, x, y, z);
            self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
            if matches!(block, ssc_world::SAND | ssc_world::GRAVEL) {
                self.settle_gravity_stack(x, y, z);
            }
            // Survival: consume one item.
            if self.cfg.game_mode == GameMode::Survival && count > 0 {
                let slot = self.sessions[&id].player.selected_slot as usize % 9;
                if let Some(session) = self.sessions.get_mut(&id) {
                    if let Some((i, c)) = session.inventory[slot] {
                        session.inventory[slot] = if c <= 1 { None } else { Some((i, c - 1)) };
                    }
                    session.inv_dirty = true;
                }
            }
        }
    }

    /// Server time, not repeated use requests, determines when food is
    /// consumed. Repeated Place packets never reset or accelerate the timer.
    fn tick_eating(&mut self) {
        let mut completed = Vec::new();
        for (id, session) in &mut self.sessions {
            let Some(eating) = &mut session.eating else {
                continue;
            };
            let valid = self.cfg.game_mode == GameMode::Survival
                && session.player.joined
                && !session.survival.dead
                && session.player.selected_slot == eating.slot
                && session
                    .inventory
                    .get(eating.slot as usize)
                    .is_some_and(|slot| {
                        slot.is_some_and(|(item, count)| item == eating.item && count > 0)
                    })
                && (session.survival.hunger < crate::survival::MAX_HUNGER
                    || eating.item == ssc_world::APPLE);
            if !valid {
                session.eating = None;
                continue;
            }
            eating.remaining -= 1;
            if eating.remaining == 0 {
                completed.push((*id, eating.item, eating.slot));
                session.eating = None;
            }
        }
        for (id, item, slot) in completed {
            let session = self.sessions.get_mut(&id).unwrap();
            let food = ssc_world::item_info(item)
                .food
                .expect("eating state stores food");
            if !session.survival.eat(
                food,
                ssc_world::items::food_saturation(item),
                item == ssc_world::APPLE,
            ) {
                continue;
            }
            if item == ssc_world::ROTTEN_FLESH
                && hash_tick(self.tick, id as i32, slot as i32) % 5 != 0
            {
                session.survival.apply_rotten_flesh_effect();
            }
            if let Some((item, count)) = session.inventory[slot as usize] {
                session.inventory[slot as usize] = if count == 1 {
                    None
                } else {
                    Some((item, count - 1))
                };
            }
            session.inv_dirty = true;
            session.status_dirty = true;
            let pos = session.player.pos;
            self.send_sound(
                SoundId::Eat,
                pos.x.floor() as i32,
                pos.y.floor() as i32,
                pos.z.floor() as i32,
            );
        }
        let mut sync = Vec::new();
        for (id, session) in &mut self.sessions {
            let remaining = session.eating.as_ref().map_or(0, |eating| eating.remaining);
            if remaining != session.last_eating_sync
                && (remaining % 4 == 0 || session.last_eating_sync == 0)
            {
                session.last_eating_sync = remaining;
                sync.push((*id, ServerPacket::EatingProgress { remaining }));
            }
        }
        for (id, packet) in sync {
            let _ = self.direct(id, &packet);
        }
    }

    /// Sound triggers are server-driven (`specs.md` §2): clients receive
    /// these packets and play/log them — they never decide sounds locally.
    fn send_sound(&mut self, sound: SoundId, x: i32, y: i32, z: i32) {
        let _ = self.broadcast(&ServerPacket::Sound {
            sound: sound.to_u16(),
            x: x as f32 + 0.5,
            y: y as f32 + 0.5,
            z: z as f32 + 0.5,
        });
    }

    /// Re-send the LOD summary of an edited column to everyone.
    fn refresh_lod_for(&mut self, key: ColumnKey) {
        let cells = self
            .gen
            .lod_summary(key.0, key.1, &self.world, &self.edited);
        let _ = self.broadcast(&ServerPacket::LodColumn {
            cx: key.0,
            cz: key.1,
            cells,
        });
        for session in self.sessions.values_mut() {
            session.lod_streamed.insert(key);
        }
    }

    // -----------------------------------------------------------------------
    // Wire helpers
    // -----------------------------------------------------------------------

    fn direct(&mut self, id: EntityId, pkt: &ServerPacket) -> Result<(), String> {
        let Some(session) = self.sessions.get_mut(&id) else {
            return Ok(());
        };
        session.wire.send(&pkt.encode())
    }

    fn broadcast(&mut self, pkt: &ServerPacket) -> Result<(), String> {
        let encoded = pkt.encode();
        for session in self.sessions.values_mut() {
            if let Err(e) = session.wire.send(&encoded) {
                eprintln!("[server] broadcast failed: {e}");
                return Err(e);
            }
        }
        Ok(())
    }
}

/// Spawn an in-process server for local play (`specs.md` §2).
///
/// Returns the client end of the wire and the server thread handle. The
/// server thread runs until the client drops the wire.
pub fn spawn_local(cfg: ServerConfig) -> (MemoryWire, std::thread::JoinHandle<()>) {
    let (client_wire, server_wire) = memory_pair();
    let handle = std::thread::Builder::new()
        .name("ssc-server".into())
        .spawn(move || {
            let mut cfg = cfg;
            cfg.local = true;
            Server::new(cfg, Box::new(server_wire)).run();
        })
        .expect("spawn server thread");
    (client_wire, handle)
}

/// Deterministic tick-positioned hash for drop randomness.
fn hash_tick(tick: u64, x: i32, z: i32) -> u64 {
    let mut h = tick.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (x as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (z as u64).wrapping_mul(0x165667B19E3779F4);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_protocol::wire::Wire;

    /// A test rig: server over in-memory wires, players joined and
    /// standing on a hand-built flat stone floor (worldgen-independent).
    struct Rig {
        server: Server,
        clients: Vec<MemoryWire>,
    }

    impl Rig {
        fn new(n: usize, pitch: f32) -> Self {
            Self::new_mode(n, pitch, GameMode::Creative)
        }

        /// `n` players spaced three blocks apart, looking with `pitch`.
        fn new_mode(n: usize, pitch: f32, mode: GameMode) -> Self {
            let mut cfg = ServerConfig::default();
            cfg.game_mode = mode;
            let mut server = Server::empty(cfg);
            let mut clients = Vec::new();
            for i in 0..n {
                let (client, server_wire) = memory_pair();
                let id = server.attach(Box::new(server_wire));
                assert_eq!(id, (i + 1) as EntityId);
                let session = server.sessions.get_mut(&id).unwrap();
                session.player.joined = true;
                session.player.pos = Vec3::new(8.5 + i as f32 * 3.0, 55.02, 8.5);
                session.player.vel = Vec3::ZERO;
                session.player.yaw = 0.0;
                session.player.pitch = pitch;
                clients.push(client);
            }
            // Flat floor: stone up to world y=54 (sub-chunk 7 local
            // 0..6), air above — players stand on the top face.
            let mut col = ssc_world::Column::default();
            let mut sc = Box::new(ssc_world::SubChunk::default());
            for y in 0..7i32 {
                for z in 0..16 {
                    for x in 0..16 {
                        sc.set(x, y, z, ssc_world::STONE);
                    }
                }
            }
            col.subchunks[7] = Some(sc);
            server.world.columns.insert((0, 0), col);
            Self { server, clients }
        }

        /// Decode all packets queued on one client end.
        fn drain(&mut self, client: usize) -> Vec<ServerPacket> {
            let mut out = Vec::new();
            while let Some(bytes) = self.clients[client].try_recv() {
                if let Ok(p) = ServerPacket::decode(&bytes) {
                    out.push(p);
                }
            }
            out
        }
    }

    #[test]
    fn grass_random_ticks_sync_and_persist_only_near_online_players() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.tick = 77;
        let ((x, y, z), _) = crate::block_ticks::sample(77, rig.server.gen.seed(), 0, 0, 7);
        rig.server.world.set_block(x, y, z, ssc_world::GRASS_BLOCK);
        rig.server.world.set_block(x, y + 1, z, ssc_world::STONE);
        rig.server.sessions.get_mut(&1).unwrap().player.joined = false;
        rig.server.tick_blocks();
        assert_eq!(
            rig.server.world.block_or_air(x, y, z),
            ssc_world::GRASS_BLOCK
        );
        rig.server.sessions.get_mut(&1).unwrap().player.joined = true;
        rig.server.sessions.get_mut(&1).unwrap().player.pos.x = 1024.0;
        rig.server.tick_blocks();
        assert_eq!(
            rig.server.world.block_or_air(x, y, z),
            ssc_world::GRASS_BLOCK
        );
        rig.server.sessions.get_mut(&1).unwrap().player.pos.x = 8.5;
        rig.server.tick_blocks();
        assert_eq!(rig.server.world.block_or_air(x, y, z), ssc_world::DIRT);
        assert_eq!(rig.server.block_edits[&(0, 0)][&(x, y, z)], ssc_world::DIRT);
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet, ServerPacket::BlockUpdate { x: px, y: py, z: pz, block: ssc_world::DIRT }
                if (*px, *py, *pz) == (x, y, z)
        )));
        let path =
            std::env::temp_dir().join(format!("ssc-grass-tick-save-{}.bin", std::process::id()));
        rig.server.cfg.world_path = Some(path.clone());
        rig.server.save_world();
        let (_, _, edits) = crate::persistence::load(&path).unwrap().unwrap();
        assert_eq!(edits[&(0, 0)][&(x, y, z)], ssc_world::DIRT);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn natural_leaf_decay_drops_world_items_but_placed_leaves_persist() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let (tick, (x, y, z)) = (1..10000)
            .find_map(|tick| {
                let ((x, y, z), roll) =
                    crate::block_ticks::sample(tick, rig.server.gen.seed(), 0, 0, 8);
                ((4..=11).contains(&x)
                    && (4..=11).contains(&z)
                    && (roll & 0xffff) as f32 / 65535.0 < ssc_world::LEAF_APPLE_CHANCE)
                    .then_some((tick, (x, y, z)))
            })
            .expect("a reproducible apple-drop leaf sample");
        rig.server.tick = tick;
        rig.server.world.set_block(x, y, z, ssc_world::OAK_LEAVES);
        rig.server.tick_blocks();
        assert_eq!(rig.server.world.block_or_air(x, y, z), AIR);
        assert_eq!(rig.server.block_edits[&(0, 0)][&(x, y, z)], AIR);
        assert!(rig
            .server
            .items
            .values()
            .any(|item| item.item == ssc_world::APPLE && item.count == 1));

        rig.server.world.set_block(x, y, z, ssc_world::OAK_LEAVES);
        rig.server.record_block_edit(x, y, z, ssc_world::OAK_LEAVES);
        let item_count = rig.server.items.len();
        rig.server.tick_blocks();
        assert_eq!(
            rig.server.world.block_or_air(x, y, z),
            ssc_world::OAK_LEAVES
        );
        assert_eq!(rig.server.items.len(), item_count);
    }

    #[test]
    fn sapling_growth_states_and_natural_canopies_are_persisted_as_world_edits() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let (tick, (x, y, z)) = (1..10000)
            .find_map(|tick| {
                let ((x, y, z), roll) =
                    crate::block_ticks::sample(tick, rig.server.gen.seed(), 0, 0, 8);
                ((4..=11).contains(&x) && (4..=11).contains(&z) && roll % 7 == 0)
                    .then_some((tick, (x, y, z)))
            })
            .unwrap();
        rig.server.tick = tick;
        rig.server.time_of_day = 0.5;
        rig.server.world.set_block(x, y - 1, z, ssc_world::DIRT);
        rig.server.world.set_block(x, y, z, ssc_world::OAK_SAPLING);
        rig.server
            .record_block_edit(x, y, z, ssc_world::OAK_SAPLING);
        rig.server.tick_blocks();
        assert_eq!(
            rig.server.world.block_or_air(x, y, z),
            ssc_world::OAK_SAPLING_READY
        );
        assert_eq!(
            rig.server.block_edits[&(0, 0)][&(x, y, z)],
            ssc_world::OAK_SAPLING_READY
        );
        let path = std::env::temp_dir().join(format!(
            "ssc-sapling-growth-save-{}.bin",
            std::process::id()
        ));
        rig.server.cfg.world_path = Some(path.clone());
        rig.server.save_world();
        let (_, _, saved) = crate::persistence::load(&path).unwrap().unwrap();
        assert_eq!(saved[&(0, 0)][&(x, y, z)], ssc_world::OAK_SAPLING_READY);
        rig.server.tick_blocks();
        assert_eq!(rig.server.world.block_or_air(x, y, z), ssc_world::OAK_LOG);
        assert!(rig.server.block_edits[&(0, 0)]
            .values()
            .any(|block| *block == ssc_world::GROWN_OAK_LEAVES));
        rig.server.save_world();
        let (_, _, saved) = crate::persistence::load(&path).unwrap().unwrap();
        assert!(saved[&(0, 0)]
            .values()
            .any(|block| *block == ssc_world::GROWN_OAK_LEAVES));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn saplings_can_be_planted_on_dirt_but_not_stone_and_drop_when_uprooted() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::OAK_SAPLING, 2));
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), AIR);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::OAK_SAPLING, 2))
        );
        rig.server.world.set_block(8, 54, 8, ssc_world::DIRT);
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.world.block_or_air(8, 55, 8),
            ssc_world::OAK_SAPLING
        );
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::OAK_SAPLING, 1))
        );
        rig.server.break_block(1, 8, 54, 8);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), AIR);
        assert!(rig
            .server
            .items
            .values()
            .any(|item| item.item == ssc_world::OAK_SAPLING && item.count == 1));
    }

    #[test]
    fn furnace_inventory_cooking_sync_and_break_drops_are_authoritative() {
        let mut rig = Rig::new_mode(2, -1.5, GameMode::Survival);
        let pos = (8, 54, 8);
        rig.server
            .world
            .set_block(pos.0, pos.1, pos.2, ssc_world::FURNACE);
        rig.server
            .record_block_edit(pos.0, pos.1, pos.2, ssc_world::FURNACE);
        rig.server.sessions.get_mut(&2).unwrap().player.pos.x = 8.5;
        rig.server.handle_use(1);
        rig.server.handle_use(2);
        assert_eq!(rig.server.sessions[&1].open_furnace, Some(pos));
        assert!(rig
            .drain(0)
            .iter()
            .any(|p| matches!(p, ServerPacket::Furnace { open: true, .. })));
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::RAW_IRON, 2));
        rig.server.sessions.get_mut(&1).unwrap().inventory[1] = Some((ssc_world::COAL, 1));
        rig.server.transfer_furnace(1, 0, 2, false).unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::RAW_IRON, 2))
        );
        rig.server.transfer_furnace(1, 0, 0, false).unwrap();
        rig.server.transfer_furnace(1, 1, 1, false).unwrap();
        assert_eq!(rig.server.sessions[&1].inventory[0], None);
        for tick in 1..=200 {
            rig.server.tick = tick;
            rig.server.tick_furnaces();
        }
        assert_eq!(
            rig.server.furnaces[&pos].slots[2],
            Some((ssc_world::IRON_INGOT, 1))
        );
        assert_eq!(
            rig.server.world.block_or_air(8, 54, 8),
            ssc_world::LIT_FURNACE
        );
        assert!(rig.drain(1).iter().any(|p| matches!(p, ServerPacket::Furnace { slots, .. } if slots[2] == (ssc_world::IRON_INGOT, 1))));
        rig.server.transfer_furnace(2, 0, 2, true).unwrap();
        assert_eq!(
            rig.server.sessions[&2].inventory[0],
            Some((ssc_world::IRON_INGOT, 1))
        );
        assert_eq!(rig.server.furnaces[&pos].slots[2], None);
        rig.server.sessions.get_mut(&2).unwrap().player.pos.x = 1000.0;
        rig.server.transfer_furnace(2, 1, 0, true).unwrap();
        assert_eq!(
            rig.server.furnaces[&pos].slots[0],
            Some((ssc_world::RAW_IRON, 1))
        );
        rig.server.tick_furnaces();
        assert_eq!(rig.server.sessions[&2].open_furnace, None);
        rig.server.sessions.get_mut(&1).unwrap().player.joined = false;
        rig.server.sessions.get_mut(&2).unwrap().player.joined = false;
        let before = rig.server.furnaces[&pos].clone();
        rig.server.tick_furnaces();
        assert_eq!(rig.server.furnaces[&pos], before);
        rig.server.break_block(1, 8, 54, 8);
        assert!(!rig.server.furnaces.contains_key(&pos));
        assert!(rig
            .server
            .items
            .values()
            .any(|item| item.item == ssc_world::RAW_IRON && item.count == 1));
    }

    #[test]
    fn furnaces_resume_saved_cooking_and_preserve_corrupt_sidecars() {
        let path =
            std::env::temp_dir().join(format!("ssc-furnace-resume-{}.bin", std::process::id()));
        let sidecar = crate::persistence::furnace_save_path(&path);
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.cfg.world_path = Some(path.clone());
        rig.server.furnace_persistence_ready = true;
        rig.server.world.set_block(8, 54, 8, ssc_world::LIT_FURNACE);
        rig.server
            .record_block_edit(8, 54, 8, ssc_world::LIT_FURNACE);
        rig.server.furnaces.insert(
            (8, 54, 8),
            crate::furnace::Furnace {
                slots: [Some((ssc_world::PORKCHOP, 1)), None, None],
                burn_remaining: 12,
                burn_total: 300,
                cook_ticks: 199,
            },
        );
        rig.server.save_world();
        rig.server.save_furnaces();
        let mut restored = Server::empty(rig.server.cfg.clone());
        assert!(restored.furnace_persistence_ready);
        assert_eq!(restored.furnaces, rig.server.furnaces);
        restored.furnaces.get_mut(&(8, 54, 8)).unwrap().tick();
        assert_eq!(
            restored.furnaces[&(8, 54, 8)].slots[2],
            Some((ssc_world::COOKED_PORKCHOP, 1))
        );
        std::fs::write(&sidecar, b"corrupt furnace data").unwrap();
        let restored = Server::empty(rig.server.cfg.clone());
        assert!(!restored.furnace_persistence_ready);
        restored.save_furnaces();
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"corrupt furnace data");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(sidecar).unwrap();
    }

    #[test]
    fn building_material_smelting_transfers_and_persists_output() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        let pos = (8, 54, 8);
        rig.server.world.set_block(8, 54, 8, ssc_world::FURNACE);
        rig.server.handle_use(1);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::COBBLESTONE, 1));
        rig.server.sessions.get_mut(&1).unwrap().inventory[1] = Some((ssc_world::COAL, 1));
        rig.server.transfer_furnace(1, 0, 0, false).unwrap();
        rig.server.transfer_furnace(1, 1, 1, false).unwrap();
        for _ in 0..200 {
            rig.server.tick_furnaces();
        }
        rig.server.transfer_furnace(1, 0, 2, true).unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::STONE, 1))
        );
        rig.server.transfer_furnace(1, 0, 0, false).unwrap();
        for _ in 0..200 {
            rig.server.tick_furnaces();
        }
        assert_eq!(
            rig.server.furnaces[&pos].slots[2],
            Some((ssc_world::SMOOTH_STONE, 1))
        );
        let path = std::env::temp_dir().join(format!(
            "ssc-building-material-furnace-{}.bin",
            std::process::id()
        ));
        crate::persistence::save_furnaces(&path, &rig.server.furnaces).unwrap();
        assert_eq!(
            crate::persistence::load_furnaces(&path).unwrap(),
            Some(rig.server.furnaces.clone())
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pig_groups_require_grass_and_respect_mob_cap() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        for cx in -2..=2 {
            for cz in -2..=2 {
                for x in cx * 16..cx * 16 + 16 {
                    for z in cz * 16..cz * 16 + 16 {
                        rig.server.world.set_block(x, 54, z, ssc_world::GRASS_BLOCK);
                    }
                }
            }
        }
        rig.server.time_of_day = 0.5;
        let mut found_group = false;
        for tick in 1..=128 {
            rig.server.mobs.clear();
            rig.server.tick = tick;
            rig.server.spawn_mobs();
            let count = rig.server.mobs.len();
            assert!(count <= 3 && count <= MOB_CAP_PER_PLAYER);
            for mob in rig.server.mobs.values() {
                assert!(matches!(
                    mob.kind,
                    crate::mobs::MobKind::Pig
                        | crate::mobs::MobKind::Sheep
                        | crate::mobs::MobKind::Cow
                ));
                let x = mob.pos.x.floor() as i32;
                let z = mob.pos.z.floor() as i32;
                assert_eq!(
                    rig.server
                        .world
                        .block_or_air(x, mob.pos.y.floor() as i32 - 1, z),
                    ssc_world::GRASS_BLOCK
                );
                let distance = (mob.pos - rig.server.sessions[&1].player.pos).length();
                assert!((MIN_MOB_SPAWN_DISTANCE..=MAX_MOB_DISTANCE).contains(&distance));
            }
            if count >= 2 {
                found_group = true;
                break;
            }
        }
        assert!(found_group, "some ticks should generate multi-pig groups");

        // An existing cap of five leaves room for only one group member.
        let sample_pos = rig.server.mobs.values().next().unwrap().pos;
        rig.server.mobs.clear();
        for id in FIRST_MOB_ID..FIRST_MOB_ID + 5 {
            rig.server.mobs.insert(
                id,
                crate::mobs::Mob::new(crate::mobs::MobKind::Pig, sample_pos),
            );
        }
        rig.server.tick += 1;
        rig.server.spawn_mobs();
        assert!(rig.server.mobs.len() <= MOB_CAP_PER_PLAYER);

        for cx in -2..=2 {
            for cz in -2..=2 {
                for x in cx * 16..cx * 16 + 16 {
                    for z in cz * 16..cz * 16 + 16 {
                        rig.server.world.set_block(x, 54, z, ssc_world::SAND);
                    }
                }
            }
        }
        rig.server.mobs.clear();
        for tick in 1..=32 {
            rig.server.tick = tick;
            rig.server.spawn_mobs();
            assert!(rig.server.mobs.is_empty(), "desert sand cannot host pigs");
        }
        rig.server.time_of_day = 0.8;
        let mut found_hostile = false;
        for tick in 1..=64 {
            rig.server.tick = tick;
            rig.server.spawn_mobs();
            assert!(
                rig.server.mobs.len() <= 1,
                "hostile mobs remain singleton spawns"
            );
            if let Some(mob) = rig.server.mobs.values().next() {
                assert!(matches!(
                    mob.kind,
                    crate::mobs::MobKind::Zombie | crate::mobs::MobKind::Spider
                ));
                found_hostile = true;
                break;
            }
        }
        assert!(
            found_hostile,
            "sand remains valid ground for night hostiles"
        );
    }

    #[test]
    fn spiders_spawn_at_night_drop_string_and_chase() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        for x in -48..48 {
            for z in -48..48 {
                rig.server.world.set_block(x, 54, z, ssc_world::STONE);
            }
        }
        rig.server.time_of_day = 0.8;
        let mut found = None;
        for tick in 1..=512 {
            rig.server.mobs.clear();
            rig.server.tick = tick;
            rig.server.spawn_mobs();
            if let Some(mob) = rig
                .server
                .mobs
                .values()
                .find(|mob| mob.kind == crate::mobs::MobKind::Spider)
            {
                found = Some(mob.pos);
                break;
            }
        }
        let pos = found.expect("spiders should join night spawns");
        assert_eq!(crate::mobs::MobKind::Spider.attack_damage(), 2.0);
        assert_eq!(crate::mobs::MobKind::Spider.max_health(), 16.0);

        rig.server.mobs.clear();
        let mut spider = crate::mobs::Mob::new(crate::mobs::MobKind::Spider, pos);
        spider.health = 1.0;
        assert!(spider.hurt(5.0, Vec3::new(8.5, 55.0, 8.5)));
        rig.server.mobs.insert(FIRST_MOB_ID, spider);
        rig.server.items.clear();
        rig.server.tick();
        assert!(!rig.server.mobs.contains_key(&FIRST_MOB_ID));
        assert!(rig
            .server
            .items
            .values()
            .all(|item| item.item == ssc_world::STRING));
    }

    #[test]
    fn sheep_spawn_on_grass_and_drop_wool_and_mutton_once() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        for x in -48..48 {
            for z in -48..48 {
                rig.server.world.set_block(x, 54, z, ssc_world::GRASS_BLOCK);
            }
        }
        rig.server.time_of_day = 0.5;
        let mut sheep = None;
        for tick in 1..=512 {
            rig.server.mobs.clear();
            rig.server.tick = tick;
            rig.server.spawn_mobs();
            if let Some(mob) = rig
                .server
                .mobs
                .values()
                .find(|mob| mob.kind == crate::mobs::MobKind::Sheep)
            {
                assert_eq!(
                    rig.server.world.block_or_air(
                        mob.pos.x.floor() as i32,
                        54,
                        mob.pos.z.floor() as i32
                    ),
                    ssc_world::GRASS_BLOCK
                );
                let distance = (mob.pos - rig.server.sessions[&1].player.pos).length();
                assert!((MIN_MOB_SPAWN_DISTANCE..=MAX_MOB_DISTANCE).contains(&distance));
                sheep = Some((tick, mob.pos));
                break;
            }
        }
        let (tick, pos) = sheep.expect("sheep should join day grass spawns");
        rig.server.tick = tick;
        rig.server.mobs.clear();
        let id = rig.server.allocate_mob_id().unwrap();
        let mut mob = crate::mobs::Mob::new(crate::mobs::MobKind::Sheep, pos);
        assert!(mob.hurt(10.0, pos - Vec3::new(1.0, 0.0, 0.0)));
        rig.server.mobs.insert(id, mob);
        rig.server.tick();
        assert!(!rig.server.mobs.contains_key(&id));
        let items: Vec<_> = rig
            .server
            .items
            .values()
            .map(|item| (item.item, item.count))
            .collect();
        assert!(items.contains(&(ssc_world::WOOL, 1)));
        assert!(items
            .iter()
            .any(|(item, count)| *item == ssc_world::MUTTON && (1..=2).contains(count)));
        let removals = rig
            .drain(0)
            .iter()
            .filter(|packet| {
                matches!(packet,
            ServerPacket::EntityRemove { id: removed } if *removed == id)
            })
            .count();
        assert_eq!(removals, 1, "sheep is removed exactly once");
    }

    #[test]
    fn doors_place_two_blocks_toggle_and_break_together() {
        let mut rig = Rig::new_mode(1, -0.6, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::DOOR, 3));
        rig.server.world.set_block(10, 55, 8, ssc_world::STONE);
        rig.server.world.set_block(10, 56, 8, ssc_world::STONE);
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(9, 55, 8), ssc_world::DOOR);
        assert_eq!(
            rig.server.world.block_or_air(9, 56, 8),
            ssc_world::DOOR_UPPER
        );
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::DOOR, 2))
        );
        assert_eq!(
            rig.server.block_edits[&(0, 0)][&(9, 55, 8)],
            ssc_world::DOOR
        );
        assert_eq!(
            rig.server.block_edits[&(0, 0)][&(9, 56, 8)],
            ssc_world::DOOR_UPPER
        );
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet,
            ServerPacket::BlockUpdate {
                x: 9,
                y: 56,
                z: 8,
                block: ssc_world::DOOR_UPPER
            }
        )));

        // The upper half blocks placement when occupied.
        rig.server.world.set_block(9, 55, 8, AIR);
        rig.server.world.set_block(9, 56, 8, ssc_world::STONE);
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(9, 55, 8), AIR);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::DOOR, 2))
        );

        rig.server.world.set_block(9, 56, 8, AIR);
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(9, 55, 8), ssc_world::DOOR);
        assert_eq!(
            rig.server.world.block_or_air(9, 56, 8),
            ssc_world::DOOR_UPPER
        );
        rig.server.sessions.get_mut(&1).unwrap().player.pitch = 0.0;
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.world.block_or_air(9, 55, 8),
            ssc_world::DOOR_OPEN
        );
        assert_eq!(
            rig.server.world.block_or_air(9, 56, 8),
            ssc_world::DOOR_OPEN_UPPER
        );
        assert!(!ssc_world::is_solid(ssc_world::DOOR_OPEN));
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(9, 55, 8), ssc_world::DOOR);
        assert_eq!(
            rig.server.world.block_or_air(9, 56, 8),
            ssc_world::DOOR_UPPER
        );

        rig.server.break_block(1, 9, 55, 8);
        assert_eq!(rig.server.world.block_or_air(9, 55, 8), AIR);
        assert_eq!(rig.server.world.block_or_air(9, 56, 8), AIR);
        assert_eq!(rig.server.block_edits[&(0, 0)][&(9, 56, 8)], AIR);
        let doors = rig.server.sessions[&1]
            .inventory
            .iter()
            .flatten()
            .filter(|(item, _)| *item == ssc_world::DOOR)
            .map(|(_, count)| *count as u16)
            .sum::<u16>();
        assert_eq!(doors, 2, "one door item drops from the pair");
    }

    #[test]
    fn doors_need_support_and_room_for_both_halves() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::DOOR, 1));
        rig.server.world.set_block(9, 55, 8, ssc_world::STONE);
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::DOOR, 1))
        );
        assert_eq!(
            rig.server.world.block_or_air(8, 55, 8),
            AIR,
            "no room inside the player"
        );

        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::DOOR, 1));
        rig.server.sessions.get_mut(&1).unwrap().player.pitch = 0.0;
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::DOOR, 1))
        );
    }

    #[test]
    fn ladders_need_wall_support_and_climb_with_jump() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::LADDER, 3));
        rig.server.world.set_block(9, 56, 8, ssc_world::STONE);
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(8, 56, 8), ssc_world::LADDER);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::LADDER, 2))
        );

        // Looking down at the floor has no horizontal support.
        rig.server.sessions.get_mut(&1).unwrap().player.pitch = -1.5;
        rig.server.world.set_block(9, 55, 8, AIR);
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.world.block_or_air(8, 55, 8),
            AIR,
            "no wall support"
        );
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::LADDER, 2))
        );

        for y in 55..59 {
            rig.server.world.set_block(8, y, 8, ssc_world::LADDER);
            rig.server.world.set_block(9, y, 8, ssc_world::STONE);
        }
        let world = &rig.server.world;
        let player = &mut rig.server.sessions.get_mut(&1).unwrap().player;
        player.pos.y = 55.02;
        player.on_ground = false;
        player.keys = keys::JUMP;
        for _ in 0..20 {
            step_player(world, player, 0.05, false, false, false);
        }
        assert!(
            player.pos.y > 56.0,
            "jump climbs the ladder: {}",
            player.pos.y
        );
        player.keys = 0;
        for _ in 0..40 {
            step_player(world, player, 0.05, false, false, false);
        }
        assert!(player.vel.y >= -1.5 - 0.001, "ladder slide is capped");
    }

    #[test]
    fn cows_spawn_on_grass_can_be_milked_and_drop_beef() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        for x in -48..48 {
            for z in -48..48 {
                rig.server.world.set_block(x, 54, z, ssc_world::GRASS_BLOCK);
            }
        }
        rig.server.time_of_day = 0.5;
        let mut cow = None;
        for tick in 1..=512 {
            rig.server.mobs.clear();
            rig.server.tick = tick;
            rig.server.spawn_mobs();
            if let Some(mob) = rig
                .server
                .mobs
                .values()
                .find(|mob| mob.kind == crate::mobs::MobKind::Cow)
            {
                assert_eq!(
                    rig.server.world.block_or_air(
                        mob.pos.x.floor() as i32,
                        54,
                        mob.pos.z.floor() as i32
                    ),
                    ssc_world::GRASS_BLOCK
                );
                cow = Some(mob.pos);
                break;
            }
        }
        cow.expect("cows join day grass spawns");

        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::BUCKET, 1));
        rig.server.sessions.get_mut(&1).unwrap().player.pitch = -0.4;
        rig.server.mobs.insert(
            FIRST_MOB_ID,
            crate::mobs::Mob::new(crate::mobs::MobKind::Cow, Vec3::new(10.5, 55.02, 8.5)),
        );
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::MILK_BUCKET, 1))
        );
        rig.server
            .sessions
            .get_mut(&1)
            .unwrap()
            .survival
            .hunger_effect_ticks = 200;
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::BUCKET, 1))
        );
        assert_eq!(rig.server.sessions[&1].survival.hunger_effect_ticks, 0);

        rig.server.items.clear();
        let cow = rig.server.mobs.get_mut(&FIRST_MOB_ID).unwrap();
        assert!(cow.hurt(10.0, Vec3::new(8.5, 55.0, 8.5)));
        rig.server.tick();
        assert!(!rig.server.mobs.contains_key(&FIRST_MOB_ID));
        assert!(rig
            .server
            .items
            .values()
            .any(|item| item.item == ssc_world::RAW_BEEF));
        assert!(rig
            .server
            .items
            .values()
            .all(|item| item.item == ssc_world::RAW_BEEF || item.item == ssc_world::LEATHER));
    }

    #[test]
    fn bows_fire_arrows_with_cooldown_and_damage_mobs() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::BOW, 1));
            session.durability[0] = 384;
            session.inventory[1] = Some((ssc_world::ARROW, 2));
        }
        rig.server.mobs.insert(
            FIRST_MOB_ID,
            crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, Vec3::new(11.5, 55.02, 8.5)),
        );
        rig.server.handle_use(1);
        assert_eq!(rig.server.arrows.len(), 1);
        assert_eq!(
            rig.server.sessions[&1].inventory[1],
            Some((ssc_world::ARROW, 1))
        );
        assert_eq!(rig.server.sessions[&1].durability[0], 383);
        assert!(rig.server.sessions[&1].player.bow_cooldown > 0);
        // The cooldown prevents an immediate second shot.
        rig.server.handle_use(1);
        assert_eq!(rig.server.arrows.len(), 1);
        assert_eq!(
            rig.server.sessions[&1].inventory[1],
            Some((ssc_world::ARROW, 1))
        );
        for _ in 0..6 {
            rig.server.tick();
        }
        assert!(rig.server.arrows.is_empty());
        assert_eq!(rig.server.mobs[&FIRST_MOB_ID].health, 16.0);
        assert!(rig
            .drain(0)
            .iter()
            .any(|packet| matches!(packet, ServerPacket::EntityState { kind: 7, .. })));
    }

    #[test]
    fn arrow_kills_award_experience_and_missing_ammo_does_not_fire() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::BOW, 1));
            session.inventory[1] = Some((ssc_world::ARROW, 1));
        }
        let mut zombie =
            crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, Vec3::new(11.5, 55.02, 8.5));
        zombie.health = 1.0;
        rig.server.mobs.insert(FIRST_MOB_ID, zombie);
        rig.server.handle_use(1);
        for _ in 0..5 {
            rig.server.tick();
        }
        assert_eq!(rig.server.sessions[&1].xp_level, 0);
        assert_eq!(rig.server.sessions[&1].xp_progress, 5);
        assert_eq!(rig.server.sessions[&1].inventory[1], None);

        rig.server.sessions.get_mut(&1).unwrap().inventory[1] = None;
        rig.server.sessions.get_mut(&1).unwrap().player.bow_cooldown = 0;
        let before = rig.server.arrows.len();
        rig.server.handle_use(1);
        assert_eq!(rig.server.arrows.len(), before, "no arrows means no shot");

        let mut creative = Rig::new_mode(1, 0.0, GameMode::Creative);
        creative.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::BOW, 1));
        creative.server.handle_use(1);
        assert_eq!(
            creative.server.arrows.len(),
            1,
            "creative fires without arrows"
        );
        assert_eq!(
            creative.server.sessions[&1].inventory[0],
            Some((ssc_world::BOW, 1))
        );
    }

    #[test]
    fn shears_shear_wool_once_and_wear_out() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::SHEARS, 1));
            session.durability[0] = 238;
        }
        rig.server.mobs.insert(
            FIRST_MOB_ID,
            crate::mobs::Mob::new(crate::mobs::MobKind::Sheep, Vec3::new(10.5, 55.02, 8.5)),
        );
        rig.server.sessions.get_mut(&1).unwrap().player.pitch = -0.4;
        rig.server.handle_use(1);
        assert!(rig.server.mobs[&FIRST_MOB_ID].sheared);
        assert_eq!(rig.server.sessions[&1].durability[0], 237);
        let wool = rig
            .server
            .items
            .values()
            .filter(|item| item.item == ssc_world::WOOL)
            .map(|item| item.count)
            .sum::<u8>();
        assert!((1..=3).contains(&wool));
        rig.server.handle_use(1);
        assert_eq!(
            rig.server
                .items
                .values()
                .filter(|item| item.item == ssc_world::WOOL)
                .map(|item| item.count)
                .sum::<u8>(),
            wool
        );
        assert_eq!(rig.server.sessions[&1].durability[0], 237);
    }

    #[test]
    fn sheared_sheep_drop_only_mutton_and_regrow_by_eating_grass() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let mut sheep =
            crate::mobs::Mob::new(crate::mobs::MobKind::Sheep, Vec3::new(10.5, 55.02, 8.5));
        sheep.sheared = true;
        assert!(sheep.hurt(10.0, Vec3::new(8.5, 55.0, 8.5)));
        rig.server.mobs.insert(FIRST_MOB_ID, sheep);
        rig.server.tick();
        assert!(!rig.server.mobs.contains_key(&FIRST_MOB_ID));
        assert!(rig
            .server
            .items
            .values()
            .any(|item| item.item == ssc_world::MUTTON));
        assert!(rig
            .server
            .items
            .values()
            .all(|item| item.item != ssc_world::WOOL));

        rig.server.items.clear();
        rig.server
            .world
            .set_block(10, 54, 8, ssc_world::GRASS_BLOCK);
        let mut sheep =
            crate::mobs::Mob::new(crate::mobs::MobKind::Sheep, Vec3::new(10.5, 55.0, 8.5));
        sheep.sheared = true;
        sheep.regrow_timer = 1199;
        sheep.wander_timer = 60;
        sheep.wander_dir = None;
        rig.server.mobs.insert(FIRST_MOB_ID, sheep);
        rig.server.tick();
        assert!(!rig.server.mobs[&FIRST_MOB_ID].sheared);
        assert_eq!(rig.server.world.block_or_air(10, 54, 8), ssc_world::DIRT);
        assert_eq!(
            rig.server.block_edits[&(0, 0)][&(10, 54, 8)],
            ssc_world::DIRT
        );
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet,
            ServerPacket::BlockUpdate {
                x: 10,
                y: 54,
                z: 8,
                block: ssc_world::DIRT
            }
        )));
    }

    #[test]
    fn natural_spawns_stay_outside_every_players_exclusion_sphere() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&2).unwrap().player.pos.x = 32.5;
        for x in -48..96 {
            for z in -48..64 {
                rig.server.world.set_block(x, 54, z, ssc_world::GRASS_BLOCK);
            }
        }
        rig.server.time_of_day = 0.5;
        let mut spawned = 0;
        for tick in 1..=64 {
            rig.server.mobs.clear();
            rig.server.tick = tick;
            rig.server.spawn_mobs();
            spawned += rig.server.mobs.len();
            for mob in rig.server.mobs.values() {
                let distances: Vec<f32> = rig
                    .server
                    .sessions
                    .values()
                    .map(|session| (mob.pos - session.player.pos).length())
                    .collect();
                assert!(distances
                    .iter()
                    .all(|distance| *distance >= MIN_MOB_SPAWN_DISTANCE));
                assert!(distances
                    .iter()
                    .any(|distance| *distance <= MAX_MOB_DISTANCE));
            }
        }
        assert!(spawned > 0, "fixture must exercise valid spawn attempts");

        // Feet 45 blocks above the floor are outside the shell even when
        // horizontally close: the range is spherical, not a 2D ring.
        rig.server.sessions.get_mut(&2).unwrap().player.joined = false;
        rig.server.sessions.get_mut(&1).unwrap().player.pos.y = 100.1;
        rig.server.mobs.clear();
        for tick in 1..=32 {
            rig.server.tick = tick;
            rig.server.spawn_mobs();
        }
        assert!(rig.server.mobs.is_empty());
    }

    #[test]
    fn low_hunger_prevents_sprinting_and_sprint_exhaustion() {
        let mut walk = Rig::new_mode(1, 0.0, GameMode::Survival);
        let mut sprint = Rig::new_mode(1, 0.0, GameMode::Survival);
        let mut creative = Rig::new_mode(1, 0.0, GameMode::Creative);
        for (rig, hunger) in [(&mut walk, 6), (&mut sprint, 7), (&mut creative, 0)] {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.player.keys = keys::FORWARD | keys::SPRINT;
            session.player.on_ground = true;
            session.survival.hunger = hunger;
            rig.server.tick();
        }
        let start = Vec3::new(8.5, 55.02, 8.5);
        let distance = |rig: &Rig| {
            let delta = rig.server.sessions[&1].player.pos - start;
            (delta.x * delta.x + delta.z * delta.z).sqrt()
        };
        assert!((distance(&walk) - WALK_SPEED / TICK_RATE).abs() < 0.001);
        assert!((distance(&sprint) - SPRINT_SPEED / TICK_RATE).abs() < 0.001);
        assert!((distance(&creative) - SPRINT_SPEED / TICK_RATE).abs() < 0.001);
        assert_eq!(walk.server.sessions[&1].survival.exhaustion, 0.0);
        assert!(
            (sprint.server.sessions[&1].survival.exhaustion - distance(&sprint) * 0.1).abs()
                < 0.0001
        );

        // Holding sprint while strafing or sneaking is ordinary movement.
        for keys in [
            keys::RIGHT | keys::SPRINT,
            keys::FORWARD | keys::SNEAK | keys::SPRINT,
        ] {
            let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.player.keys = keys;
            session.player.on_ground = true;
            rig.server.tick();
            assert_eq!(rig.server.sessions[&1].survival.exhaustion, 0.0);
        }
    }

    #[test]
    fn sprinting_into_a_wall_does_not_drain_hunger() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.world.set_block(9, 55, 8, ssc_world::STONE);
        rig.server.world.set_block(9, 56, 8, ssc_world::STONE);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.player.pos.x = 8.7;
        session.player.on_ground = true;
        session.player.keys = keys::FORWARD | keys::SPRINT;
        for _ in 0..200 {
            rig.server.tick();
        }
        assert!(rig.server.sessions[&1].survival.exhaustion < 0.001);
        assert_eq!(
            rig.server.sessions[&1].survival.hunger,
            crate::survival::MAX_HUNGER
        );
    }

    #[test]
    fn sneak_holds_the_ledge_until_released_or_jumped() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.player.pos = Vec3::new(15.25, 55.0001, 8.5);
        session.player.on_ground = true;
        session.player.keys = keys::FORWARD | keys::SNEAK;
        for _ in 0..20 {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            step_player(
                &rig.server.world,
                &mut session.player,
                0.05,
                false,
                false,
                false,
            );
        }
        assert!(rig.server.sessions[&1].player.pos.x < 16.3);
        assert!(rig.server.sessions[&1].player.on_ground);

        // Jumping still lets the player leave the ledge.
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.player.keys |= keys::JUMP;
        step_player(
            &rig.server.world,
            &mut session.player,
            0.05,
            false,
            false,
            false,
        );
        assert!(session.player.pos.y > 55.1);
        assert!(!session.player.on_ground);
        session.player.keys = keys::FORWARD;
        for _ in 0..25 {
            step_player(
                &rig.server.world,
                &mut session.player,
                0.05,
                false,
                false,
                false,
            );
        }
        assert!(session.player.pos.x > 16.3);
        assert!(session.player.pos.y < 55.0);
    }

    #[test]
    fn releasing_sneak_walks_off_the_ledge() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.player.pos = Vec3::new(16.25, 55.0001, 8.5);
        session.player.on_ground = true;
        session.player.keys = keys::FORWARD;
        for _ in 0..10 {
            step_player(
                &rig.server.world,
                &mut session.player,
                0.05,
                false,
                false,
                false,
            );
        }
        assert!(session.player.pos.x > 16.3);
        assert!(session.player.pos.y < 55.0);
    }

    #[test]
    fn dig_removes_block_and_emits_update_and_sound() {
        // Creative: straight down = instant break.
        let mut rig = Rig::new(1, -1.5);
        rig.server.handle_dig_start(1);

        let packets = rig.drain(0);
        let updates = packets
            .iter()
            .filter(|p| matches!(p, ServerPacket::BlockUpdate { block: 0, .. }))
            .count();
        let sounds = packets
            .iter()
            .filter(|p| matches!(p, ServerPacket::Sound { .. }))
            .count();
        assert_eq!(
            updates, 1,
            "expected exactly one BlockUpdate(AIR), got {packets:?}"
        );
        assert_eq!(sounds, 1, "expected exactly one dig Sound, got {packets:?}");
    }

    #[test]
    fn place_puts_selected_block_with_sound() {
        // Creative default hotbar slot 0 = bricks.
        let mut rig = Rig::new(1, -1.2);
        rig.server.handle_use(1);

        let packets = rig.drain(0);
        let updates = packets
            .iter()
            .filter(|p| {
                matches!(
                    p,
                    ServerPacket::BlockUpdate {
                        block: ssc_world::BRICKS,
                        ..
                    }
                )
            })
            .count();
        let sounds = packets
            .iter()
            .filter(|p| matches!(p, ServerPacket::Sound { .. }))
            .count();
        assert_eq!(
            updates, 1,
            "expected exactly one BlockUpdate(BRICKS), got {packets:?}"
        );
        assert_eq!(
            sounds, 1,
            "expected exactly one place Sound, got {packets:?}"
        );
    }

    #[test]
    fn gravel_and_sand_stack_settles_and_records_both_edits() {
        let mut rig = Rig::new(1, 0.0);
        let (x, z) = (8, 8);
        rig.server.world.set_block(x, 50, z, ssc_world::STONE);
        for y in 51..=56 {
            rig.server.world.set_block(x, y, z, AIR);
        }
        rig.server.world.set_block(x, 55, z, ssc_world::GRAVEL);
        rig.server.world.set_block(x, 56, z, ssc_world::SAND);
        rig.server.settle_gravity_stack(x, 55, z);
        assert_eq!(rig.server.world.block_or_air(x, 51, z), ssc_world::GRAVEL);
        assert_eq!(rig.server.world.block_or_air(x, 52, z), ssc_world::SAND);
        assert_eq!(rig.server.world.block_or_air(x, 55, z), AIR);
        assert_eq!(rig.server.world.block_or_air(x, 56, z), AIR);
        let edits = &rig.server.block_edits[&(0, 0)];
        assert_eq!(edits[&(x, 51, z)], ssc_world::GRAVEL);
        assert_eq!(edits[&(x, 52, z)], ssc_world::SAND);
        assert_eq!(edits[&(x, 55, z)], AIR);
        assert_eq!(edits[&(x, 56, z)], AIR);
        let packets = rig.drain(0);
        assert_eq!(
            packets
                .iter()
                .filter(|packet| matches!(packet, ServerPacket::BlockUpdate { .. }))
                .count(),
            4
        );
    }

    #[test]
    fn breaking_support_settles_gravel_above_it() {
        let mut rig = Rig::new(1, 0.0);
        rig.server.world.set_block(8, 53, 8, ssc_world::STONE);
        rig.server.world.set_block(8, 54, 8, ssc_world::STONE);
        rig.server.world.set_block(8, 55, 8, ssc_world::GRAVEL);
        rig.server.break_block(1, 8, 54, 8);
        assert_eq!(rig.server.world.block_or_air(8, 54, 8), ssc_world::GRAVEL);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), AIR);
    }

    #[test]
    fn dig_at_sky_hits_nothing() {
        let mut rig = Rig::new(1, 1.5); // looking straight up
        rig.server.handle_dig_start(1);
        let packets = rig.drain(0);
        assert!(
            packets.is_empty(),
            "dig at the sky must change nothing: {packets:?}"
        );
    }

    #[test]
    fn survival_dig_grass_drops_dirt() {
        // Same rig but a grass floor: drop = dirt (not cobble/grass).
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        // Replace the floor surface with grass.
        rig.server.world.set_block(8, 54, 8, ssc_world::GRASS_BLOCK);
        rig.server.handle_dig_start(1);
        for _ in 0..160 {
            rig.server.tick();
        }
        let inv = &rig.server.sessions[&1].inventory;
        assert!(
            inv.iter().flatten().any(|(i, _)| *i == ssc_world::DIRT),
            "grass must drop dirt, inventory: {inv:?}"
        );
    }

    #[test]
    fn survival_dig_by_hand_breaks_stone_without_a_drop() {
        // Stone by hand: 7.5 s = 150 ticks.
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.handle_dig_start(1);
        let packets = rig.drain(0);
        assert!(
            packets
                .iter()
                .all(|p| !matches!(p, ServerPacket::BlockUpdate { .. })),
            "no instant break in survival: {packets:?}"
        );
        assert!(rig.server.sessions[&1].survival.digging.is_some());

        // 149 ticks: almost done, still not broken.
        for _ in 0..149 {
            rig.server.tick();
        }
        let packets = rig.drain(0);
        assert!(
            packets
                .iter()
                .all(|p| !matches!(p, ServerPacket::BlockUpdate { .. })),
            "stone must not break early: {packets:?}"
        );
        // A few extra ticks finish it (floating-point accumulation can
        // need one tick of slack).
        for _ in 0..5 {
            rig.server.tick();
        }
        let packets = rig.drain(0);
        assert!(packets
            .iter()
            .any(|p| matches!(p, ServerPacket::BlockUpdate { block: 0, .. })));
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .all(Option::is_none));
        assert!((rig.server.sessions[&1].survival.exhaustion - 0.005).abs() < 0.0001);
    }

    #[test]
    fn submerged_dig_progress_is_slower_than_on_land() {
        let mut dry = Rig::new_mode(1, -1.5, GameMode::Survival);
        let mut wet = Rig::new_mode(1, -1.5, GameMode::Survival);
        for rig in [&mut dry, &mut wet] {
            rig.server.sessions.get_mut(&1).unwrap().inventory[0] =
                Some((ssc_world::STONE_PICKAXE, 1));
            rig.server.sessions.get_mut(&1).unwrap().player.on_ground = true;
        }
        wet.server.world.set_block(8, 56, 8, ssc_world::WATER);
        dry.server.handle_dig_start(1);
        wet.server.handle_dig_start(1);
        dry.server.tick();
        wet.server.tick();
        let dry_progress = dry.server.sessions[&1]
            .survival
            .digging
            .as_ref()
            .unwrap()
            .progress;
        let wet_progress = wet.server.sessions[&1]
            .survival
            .digging
            .as_ref()
            .unwrap()
            .progress;
        assert!(wet_progress > 0.0);
        assert!(wet_progress <= dry_progress / 5.0 + f32::EPSILON);
    }

    #[test]
    fn crafted_pickaxe_mines_stone_and_drops_cobblestone() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::OAK_LOG, 2));
        for recipe in [0, 0, 1, 2] {
            rig.server
                .handle_packet(1, ClientPacket::Craft { recipe })
                .unwrap();
        }
        let inv = &rig.server.sessions[&1].inventory;
        let slot = inv
            .iter()
            .position(|entry| *entry == Some((ssc_world::WOODEN_PICKAXE, 1)))
            .unwrap();
        rig.server
            .handle_packet(1, ClientPacket::SelectSlot { slot: slot as u8 })
            .unwrap();
        rig.server.handle_dig_start(1);
        for _ in 0..25 {
            rig.server.tick();
        }
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .flatten()
            .any(|(item, _)| *item == ssc_world::COBBLESTONE));
    }

    #[test]
    fn chests_open_transfer_tool_wear_and_sync_contents() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.world.set_block(8, 54, 8, ssc_world::CHEST);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::IRON_SWORD, 1));
        rig.server.sessions.get_mut(&1).unwrap().durability[0] = 17;
        rig.server.handle_use(1);
        assert_eq!(rig.server.sessions[&1].open_chest, Some((8, 54, 8)));
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet,
            ServerPacket::Chest {
                open: true,
                x: 8,
                y: 54,
                z: 8,
                ..
            }
        )));
        rig.server.transfer_chest(1, 0, 0, false).unwrap();
        {
            let chest = &rig.server.chests[&(8, 54, 8)];
            assert_eq!(chest.slots[0], Some((ssc_world::IRON_SWORD, 1)));
            assert_eq!(chest.durability[0], 17);
        }
        assert_eq!(rig.server.sessions[&1].inventory[0], None);
        assert!(rig.drain(0).iter().any(|packet| matches!(packet,
            ServerPacket::Chest { slots, .. } if slots[0] == (ssc_world::IRON_SWORD, 1))));
        rig.server.transfer_chest(1, 0, 0, true).unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::IRON_SWORD, 1))
        );
        assert_eq!(rig.server.sessions[&1].durability[0], 17);
        assert_eq!(rig.server.chests[&(8, 54, 8)].slots[0], None);

        // Breaking the chest drops the chest and its stored items once.
        rig.server.transfer_chest(1, 0, 0, false).unwrap();
        rig.server.break_block(1, 8, 54, 8);
        assert!(!rig.server.chests.contains_key(&(8, 54, 8)));
        assert_eq!(rig.server.sessions[&1].open_chest, None);
        assert!(rig
            .server
            .items
            .values()
            .any(|item| item.item == ssc_world::IRON_SWORD && item.durability == 17));
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .flatten()
            .any(|(item, _)| *item == ssc_world::CHEST));
    }

    #[test]
    fn chest_transfers_merge_stacks_and_close_out_of_range() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.world.set_block(8, 54, 8, ssc_world::CHEST);
        rig.server.handle_use(1);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::APPLE, 10));
        rig.server.transfer_chest(1, 0, 0, false).unwrap();
        rig.server.sessions.get_mut(&1).unwrap().inventory[1] = Some((ssc_world::APPLE, 5));
        rig.server.transfer_chest(1, 1, 0, false).unwrap();
        assert_eq!(
            rig.server.chests[&(8, 54, 8)].slots[0],
            Some((ssc_world::APPLE, 15))
        );
        assert_eq!(rig.server.sessions[&1].inventory[1], None);
        rig.server.transfer_chest(1, 0, 0, true).unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 15))
        );
        rig.server.transfer_chest(1, 0, 0, false).unwrap();
        rig.server.sessions.get_mut(&1).unwrap().player.pos.x = 100.0;
        rig.server.tick_chests();
        assert_eq!(rig.server.sessions[&1].open_chest, None);
        assert!(rig
            .drain(0)
            .iter()
            .any(|packet| matches!(packet, ServerPacket::ContainerClosed)));
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::APPLE, 1));
        rig.server.transfer_chest(1, 0, 0, false).unwrap();
        assert_eq!(
            rig.server.chests[&(8, 54, 8)].slots[0],
            Some((ssc_world::APPLE, 15))
        );
    }

    #[test]
    fn chests_and_contents_persist_with_tool_wear_and_guard_corruption() {
        let world_path =
            std::env::temp_dir().join(format!("ssc-chest-world-{}.bin", std::process::id()));
        let sidecar = crate::persistence::chest_save_path(&world_path);
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.cfg.world_path = Some(world_path.clone());
        rig.server.chest_persistence_ready = true;
        rig.server.world.set_block(8, 54, 8, ssc_world::CHEST);
        rig.server.record_block_edit(8, 54, 8, ssc_world::CHEST);
        let chest = rig.server.chests.entry((8, 54, 8)).or_default();
        chest.slots[0] = Some((ssc_world::IRON_PICKAXE, 1));
        chest.durability[0] = 44;
        chest.slots[5] = Some((ssc_world::APPLE, 12));
        rig.server.save_world();
        rig.server.save_chests();

        let restored = Server::empty(rig.server.cfg.clone());
        assert!(restored.chest_persistence_ready);
        let chest = &restored.chests[&(8, 54, 8)];
        assert_eq!(chest.slots[0], Some((ssc_world::IRON_PICKAXE, 1)));
        assert_eq!(chest.durability[0], 44);
        assert_eq!(chest.slots[5], Some((ssc_world::APPLE, 12)));

        std::fs::write(&sidecar, b"corrupt chest save").unwrap();
        let restored = Server::empty(rig.server.cfg.clone());
        assert!(!restored.chest_persistence_ready);
        restored.save_chests();
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"corrupt chest save");
        std::fs::remove_file(&world_path).unwrap();
        std::fs::remove_file(&sidecar).unwrap();
    }

    #[test]
    fn iron_armor_equips_swaps_and_syncs_with_durability() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] =
            Some((ssc_world::IRON_CHESTPLATE, 1));
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.sessions[&1].armor[1],
            Some((ssc_world::IRON_CHESTPLATE, 1))
        );
        assert_eq!(rig.server.sessions[&1].armor_durability[1], 240);
        assert_eq!(rig.server.sessions[&1].inventory[0], None);
        rig.server.tick();
        assert!(rig.drain(0).iter().any(|packet| matches!(packet,
            ServerPacket::Armor { slots } if slots[1] == (ssc_world::IRON_CHESTPLATE, 1, 240))));

        // Equipping a second pair of boots returns the old pair to the hand
        // with its wear intact.
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.armor[3] = Some((ssc_world::IRON_BOOTS, 1));
            session.armor_durability[3] = 120;
            session.inventory[0] = Some((ssc_world::IRON_BOOTS, 1));
            session.durability[0] = 77;
        }
        rig.server.handle_use(1);
        assert_eq!(rig.server.sessions[&1].armor_durability[3], 77);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::IRON_BOOTS, 1))
        );
        assert_eq!(rig.server.sessions[&1].durability[0], 120);

        // Creative keeps its palette item.
        let mut creative = Rig::new_mode(1, 0.0, GameMode::Creative);
        creative.server.sessions.get_mut(&1).unwrap().inventory[0] =
            Some((ssc_world::IRON_HELMET, 1));
        creative.server.handle_use(1);
        assert_eq!(
            creative.server.sessions[&1].armor[0],
            Some((ssc_world::IRON_HELMET, 1))
        );
        assert_eq!(
            creative.server.sessions[&1].inventory[0],
            Some((ssc_world::IRON_HELMET, 1))
        );
    }

    fn full_iron_armor() -> [crate::survival::Slot; crate::survival::ARMOR_SIZE] {
        [
            Some((ssc_world::IRON_HELMET, 1)),
            Some((ssc_world::IRON_CHESTPLATE, 1)),
            Some((ssc_world::IRON_LEGGINGS, 1)),
            Some((ssc_world::IRON_BOOTS, 1)),
        ]
    }

    #[test]
    fn leather_armor_equips_and_reduces_damage_by_seven_points() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] =
            Some((ssc_world::LEATHER_HELMET, 1));
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.sessions[&1].armor[0],
            Some((ssc_world::LEATHER_HELMET, 1))
        );
        assert_eq!(rig.server.sessions[&1].armor_durability[0], 55);

        rig.server.time_of_day = 0.8;
        rig.server.sessions.get_mut(&1).unwrap().armor = [
            Some((ssc_world::LEATHER_HELMET, 1)),
            Some((ssc_world::LEATHER_CHESTPLATE, 1)),
            Some((ssc_world::LEATHER_LEGGINGS, 1)),
            Some((ssc_world::LEATHER_BOOTS, 1)),
        ];
        rig.server.mobs.insert(
            FIRST_MOB_ID,
            crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, Vec3::new(8.5, 55.02, 9.5)),
        );
        rig.server.tick();
        let damage = 20.0 - rig.server.sessions[&1].survival.health;
        assert!((damage - 3.0 * 0.72).abs() < 0.001, "{damage}");
    }

    #[test]
    fn armor_reduces_zombie_damage_and_wears_per_hit() {
        for (armor, expected, remaining) in [([None; 4], 3.0, 0), (full_iron_armor(), 1.2, 164)] {
            let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
            rig.server.time_of_day = 0.8;
            rig.server.mobs.insert(
                FIRST_MOB_ID,
                crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, Vec3::new(8.5, 55.02, 9.5)),
            );
            rig.server.sessions.get_mut(&1).unwrap().armor = armor;
            for index in 0..crate::survival::ARMOR_SIZE {
                rig.server.sessions.get_mut(&1).unwrap().armor_durability[index] =
                    if armor[index].is_some() { 165 } else { 0 };
            }
            rig.server.tick();
            let damage = 20.0 - rig.server.sessions[&1].survival.health;
            assert!((damage - expected).abs() < 0.001, "{damage}");
            if remaining > 0 {
                assert_eq!(rig.server.sessions[&1].armor_durability[0], remaining);
            }
        }

        // A piece at its last use breaks after absorbing a hit.
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.time_of_day = 0.8;
        rig.server.mobs.insert(
            FIRST_MOB_ID,
            crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, Vec3::new(8.5, 55.02, 9.5)),
        );
        rig.server.sessions.get_mut(&1).unwrap().armor[0] = Some((ssc_world::IRON_HELMET, 1));
        rig.server.sessions.get_mut(&1).unwrap().armor_durability[0] = 1;
        rig.server.tick();
        assert_eq!(rig.server.sessions[&1].armor[0], None);
        assert!(rig.drain(0).iter().any(|packet| matches!(packet,
            ServerPacket::Armor { slots } if slots[0] == (0, 0, 0))));
    }

    #[test]
    fn mob_kills_award_experience_levels_and_status_sync() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        for id in [FIRST_MOB_ID, FIRST_MOB_ID + 1] {
            let mut zombie =
                crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, Vec3::new(10.5, 55.02, 8.5));
            zombie.health = 1.0;
            rig.server.mobs.insert(id, zombie);
            rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
            rig.server.tick(); // loot/death processing and status sync
        }
        assert_eq!(rig.server.sessions[&1].xp_level, 1);
        assert_eq!(rig.server.sessions[&1].xp_progress, 3);
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet,
            ServerPacket::Status {
                xp_level: 1,
                xp_progress: 3,
                ..
            }
        )));
    }

    #[test]
    fn furnace_output_collection_awards_experience_once() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        let pos = (8, 54, 8);
        rig.server.world.set_block(8, 54, 8, ssc_world::FURNACE);
        rig.server.handle_use(1);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::RAW_IRON, 1));
        rig.server.sessions.get_mut(&1).unwrap().inventory[1] = Some((ssc_world::COAL, 1));
        rig.server.transfer_furnace(1, 0, 0, false).unwrap();
        rig.server.transfer_furnace(1, 1, 1, false).unwrap();
        for _ in 0..201 {
            rig.server.tick_furnaces();
        }
        assert_eq!(
            rig.server.furnaces[&pos].slots[2],
            Some((ssc_world::IRON_INGOT, 1))
        );
        assert_eq!(
            rig.server.sessions[&1].xp_progress, 0,
            "XP waits for collection"
        );
        rig.server.transfer_furnace(1, 0, 2, true).unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::IRON_INGOT, 1))
        );
        assert_eq!(rig.server.sessions[&1].xp_progress, 1);
        rig.server.transfer_furnace(1, 0, 2, true).unwrap();
        assert_eq!(
            rig.server.sessions[&1].xp_progress, 1,
            "empty output grants nothing"
        );
    }

    #[test]
    fn death_resets_experience_with_the_inventory_drop() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.xp_level = 4;
            session.xp_progress = 5;
        }
        let mut fx = crate::survival::TickEffects::default();
        rig.server
            .sessions
            .get_mut(&1)
            .unwrap()
            .survival
            .apply_combat_damage(20.0, &mut fx);
        assert!(fx.hurt);
        rig.server.tick();
        assert_eq!(rig.server.sessions[&1].xp_level, 0);
        assert_eq!(rig.server.sessions[&1].xp_progress, 0);
    }

    #[test]
    fn armor_reduces_pvp_sword_damage() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        let victim = rig.server.sessions[&1].player.pos;
        rig.server.sessions.get_mut(&2).unwrap().player.pos = victim + Vec3::new(-1.0, 0.0, 0.0);
        rig.server.sessions.get_mut(&2).unwrap().inventory[0] = Some((ssc_world::IRON_SWORD, 1));
        rig.server.sessions.get_mut(&1).unwrap().armor = full_iron_armor();
        for index in 0..crate::survival::ARMOR_SIZE {
            rig.server.sessions.get_mut(&1).unwrap().armor_durability[index] = 165;
        }
        rig.server.handle_packet(2, ClientPacket::DigStart).unwrap();
        let damage = 20.0 - rig.server.sessions[&1].survival.health;
        assert!((damage - 2.8).abs() < 0.001, "{damage}");
        assert_eq!(rig.server.sessions[&1].armor_durability[1], 164);
        assert_eq!(rig.server.sessions[&1].armor_durability[0], 164);
    }

    #[test]
    fn crafted_iron_tools_start_full_and_apply_combat_wear() {
        for (recipe, item, ingots, damage) in [
            (10, ssc_world::IRON_PICKAXE, 3, 5.0),
            (11, ssc_world::IRON_AXE, 3, 6.0),
            (12, ssc_world::IRON_SHOVEL, 1, 4.0),
        ] {
            let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::IRON_INGOT, ingots));
            session.inventory[1] = Some((ssc_world::STICK, 2));
            rig.server
                .handle_packet(1, ClientPacket::Craft { recipe })
                .unwrap();
            assert_eq!(rig.server.sessions[&1].inventory[0], Some((item, 1)));
            assert_eq!(rig.server.sessions[&1].durability[0], 251);
            rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
            assert_eq!(rig.server.sessions[&2].survival.health, 20.0 - damage);
            assert_eq!(rig.server.sessions[&1].durability[0], 249);
        }
    }

    #[test]
    fn crafted_swords_use_bedrock_damage_and_one_use_per_entity_hit() {
        for (recipe, material, item, durability, damage) in [
            (13, ssc_world::OAK_PLANKS, ssc_world::WOODEN_SWORD, 60, 5.0),
            (14, ssc_world::COBBLESTONE, ssc_world::STONE_SWORD, 132, 6.0),
            (15, ssc_world::IRON_INGOT, ssc_world::IRON_SWORD, 251, 7.0),
        ] {
            let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((material, 2));
            session.inventory[1] = Some((ssc_world::STICK, 1));
            rig.server
                .handle_packet(1, ClientPacket::Craft { recipe })
                .unwrap();
            assert_eq!(rig.server.sessions[&1].inventory[0], Some((item, 1)));
            assert_eq!(rig.server.sessions[&1].durability[0], durability);
            rig.server.handle_dig_start(1);
            assert_eq!(rig.server.sessions[&2].survival.health, 20.0 - damage);
            assert_eq!(rig.server.sessions[&1].durability[0], durability - 1);
            rig.server.handle_dig_start(1);
            assert_eq!(
                rig.server.sessions[&1].durability[0],
                durability - 1,
                "immune hits cost nothing"
            );

            let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
            rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((item, 1));
            rig.server.sessions.get_mut(&1).unwrap().durability[0] = 1;
            rig.server.mobs.insert(
                FIRST_MOB_ID,
                crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, Vec3::new(10.5, 55.02, 8.5)),
            );
            rig.server.handle_dig_start(1);
            assert_eq!(rig.server.mobs[&FIRST_MOB_ID].health, 20.0 - damage);
            assert_eq!(rig.server.sessions[&1].inventory[0], None);
        }
    }

    #[test]
    fn swords_cost_two_uses_per_block_but_not_zero_hardness_plants() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::IRON_SWORD, 1));
        session.durability[0] = 10;
        rig.server.break_block(1, 8, 54, 8);
        assert_eq!(rig.server.sessions[&1].durability[0], 8);
        rig.server.world.set_block(8, 54, 8, ssc_world::OAK_SAPLING);
        rig.server.break_block(1, 8, 54, 8);
        assert_eq!(rig.server.sessions[&1].durability[0], 8);
        let mut creative = Rig::new_mode(1, -1.5, GameMode::Creative);
        creative.server.sessions.get_mut(&1).unwrap().inventory[0] =
            Some((ssc_world::IRON_SWORD, 1));
        creative.server.handle_dig_start(1);
        assert_eq!(
            creative.server.world.block_or_air(8, 54, 8),
            ssc_world::STONE
        );
    }

    #[test]
    fn dead_and_creative_players_cannot_craft_survival_tools() {
        for mode in [GameMode::Survival, GameMode::Creative] {
            let mut rig = Rig::new_mode(1, 0.0, mode);
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::OAK_LOG, 1));
            if mode == GameMode::Survival {
                session.survival.dead = true;
            }
            let before = session.inventory.clone();
            rig.server
                .handle_packet(1, ClientPacket::Craft { recipe: 0 })
                .unwrap();
            assert_eq!(rig.server.sessions[&1].inventory, before);
        }
    }

    #[test]
    fn dropping_selected_item_is_server_authoritative_and_delayed() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::FLINT, 2));
        rig.server
            .handle_packet(1, ClientPacket::DropSelected)
            .unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::FLINT, 1))
        );
        let entity = rig.server.items.values().next().unwrap();
        assert_eq!((entity.item, entity.count), (ssc_world::FLINT, 1));
        assert_eq!(entity.pickup_delay, 40);
        rig.server
            .handle_packet(1, ClientPacket::DropSelected)
            .unwrap();
        rig.server
            .handle_packet(1, ClientPacket::DropSelected)
            .unwrap();
        assert_eq!(rig.server.sessions[&1].inventory[0], None);
        assert_eq!(
            rig.server
                .items
                .values()
                .map(|item| item.count as u16)
                .sum::<u16>(),
            2
        );
        rig.server.tick();
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet,
            ServerPacket::Inventory { slots } if slots[0] == (0, 0)
        )));
        assert_eq!(
            rig.server
                .items
                .values()
                .map(|item| item.count as u16)
                .sum::<u16>(),
            2
        );
    }

    #[test]
    fn dead_and_creative_players_cannot_drop_selected_item() {
        for mode in [GameMode::Survival, GameMode::Creative] {
            let mut rig = Rig::new_mode(1, 0.0, mode);
            rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::OAK_LOG, 2));
            if mode == GameMode::Survival {
                rig.server.sessions.get_mut(&1).unwrap().survival.dead = true;
            }
            rig.server
                .handle_packet(1, ClientPacket::DropSelected)
                .unwrap();
            rig.server
                .handle_packet(1, ClientPacket::DropStack)
                .unwrap();
            assert_eq!(
                rig.server.sessions[&1].inventory[0],
                Some((ssc_world::OAK_LOG, 2))
            );
            assert!(rig.server.items.is_empty());
        }
    }

    #[test]
    fn dropping_full_stacks_preserves_counts_wear_and_prevents_repeated_duplication() {
        for (item, count, wear) in [
            (ssc_world::APPLE, 64, 0),
            (ssc_world::SNOWBALL, 16, 0),
            (ssc_world::IRON_SWORD, 1, 17),
        ] {
            let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.player.selected_slot = 3;
            session.inventory[3] = Some((item, count));
            session.durability[3] = wear;
            for _ in 0..3 {
                rig.server
                    .handle_packet(1, ClientPacket::DropStack)
                    .unwrap();
            }
            assert_eq!(rig.server.sessions[&1].inventory[3], None);
            assert_eq!(rig.server.sessions[&1].durability[3], 0);
            assert_eq!(rig.server.items.len(), 1);
            let entity = rig.server.items.values().next().unwrap();
            assert_eq!(
                (entity.item, entity.count, entity.durability),
                (item, count, wear)
            );
            assert_eq!(entity.pickup_delay, 40);
            rig.server.tick();
            assert!(rig.drain(0).iter().any(|packet| matches!(
                packet, ServerPacket::Inventory { slots } if slots[3] == (0, 0)
            )));
        }
    }

    #[test]
    fn dropping_full_food_stack_cancels_eating_and_unjoined_requests_do_nothing() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::APPLE, 5));
        rig.server.handle_use(1);
        assert!(rig.server.sessions[&1].eating.is_some());
        rig.server
            .handle_packet(1, ClientPacket::DropStack)
            .unwrap();
        assert!(rig.server.sessions[&1].eating.is_none());
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::APPLE, 5));
        rig.server.sessions.get_mut(&1).unwrap().player.joined = false;
        rig.server
            .handle_packet(1, ClientPacket::DropStack)
            .unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 5))
        );
        assert_eq!(rig.server.items.len(), 1);
    }

    #[test]
    fn buckets_collect_and_place_static_water_with_inventory_and_world_sync() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::BUCKET, 1));
        rig.server.world.set_block(8, 55, 8, ssc_world::WATER);
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), AIR);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::WATER_BUCKET, 1))
        );
        assert_eq!(rig.server.block_edits[&(0, 0)][&(8, 55, 8)], AIR);
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet,
            ServerPacket::BlockUpdate {
                x: 8,
                y: 55,
                z: 8,
                block: AIR
            }
        )));
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), ssc_world::WATER);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::BUCKET, 1))
        );
        assert_eq!(
            rig.server.block_edits[&(0, 0)][&(8, 55, 8)],
            ssc_world::WATER
        );
        let path =
            std::env::temp_dir().join(format!("ssc-bucket-water-world-{}.bin", std::process::id()));
        rig.server.cfg.world_path = Some(path.clone());
        rig.server.save_world();
        let (_, _, edits) = crate::persistence::load(&path).unwrap().unwrap();
        assert_eq!(edits[&(0, 0)][&(8, 55, 8)], ssc_world::WATER);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stacked_buckets_keep_empty_buckets_and_drop_filled_overflow_once() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server
            .sessions
            .get_mut(&1)
            .unwrap()
            .inventory
            .fill(Some((ssc_world::COBBLESTONE, 64)));
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::BUCKET, 2));
        rig.server.world.set_block(8, 55, 8, ssc_world::WATER);
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::BUCKET, 1))
        );
        assert_eq!(rig.server.items.len(), 1);
        let item = rig.server.items.values().next().unwrap();
        assert_eq!((item.item, item.count), (ssc_world::WATER_BUCKET, 1));
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.items.len(),
            1,
            "repeat requests cannot refill from air"
        );
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::BUCKET, 1))
        );
    }

    #[test]
    fn exhausted_item_ids_do_not_delete_dropped_stacks_or_bucket_overflow() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        for id in FIRST_ITEM_ID..=EntityId::MAX {
            rig.server.items.insert(
                id,
                ItemEntity {
                    item: ssc_world::APPLE,
                    count: 1,
                    durability: 0,
                    pos: Vec3::ZERO,
                    vel: Vec3::ZERO,
                    age: 0,
                    pickup_delay: 10,
                },
            );
        }
        rig.server
            .sessions
            .get_mut(&1)
            .unwrap()
            .inventory
            .fill(Some((ssc_world::COBBLESTONE, 64)));
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::BUCKET, 2));
        rig.server
            .handle_packet(1, ClientPacket::DropStack)
            .unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::BUCKET, 2))
        );
        rig.server.world.set_block(8, 55, 8, ssc_world::WATER);
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), ssc_world::WATER);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::BUCKET, 2))
        );
        assert!(rig.server.block_edits.is_empty());
    }

    #[test]
    fn buckets_cannot_collect_through_walls_or_place_over_existing_water() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::BUCKET, 1));
        rig.server.world.set_block(9, 56, 8, ssc_world::GLASS);
        rig.server.world.set_block(10, 56, 8, ssc_world::WATER);
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(10, 56, 8), ssc_world::WATER);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::BUCKET, 1))
        );
        rig.server.world.set_block(9, 56, 8, AIR);
        rig.server.world.set_block(10, 56, 8, AIR);
        rig.server.world.set_block(14, 56, 8, ssc_world::WATER);
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.world.block_or_air(14, 56, 8),
            ssc_world::WATER,
            "out of reach"
        );
        rig.server.sessions.get_mut(&1).unwrap().player.pitch = -1.5;
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::WATER_BUCKET, 1));
        rig.server.world.set_block(8, 55, 8, ssc_world::WATER);
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::WATER_BUCKET, 1))
        );
    }

    #[test]
    fn hoe_tills_grass_into_farmland_wears_and_requires_clear_space() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::WOODEN_HOE, 1));
        session.durability[0] = 5;
        rig.server.world.set_block(8, 54, 8, ssc_world::GRASS_BLOCK);
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(8, 54, 8), ssc_world::FARMLAND);
        assert_eq!(
            rig.server.block_edits[&(0, 0)][&(8, 54, 8)],
            ssc_world::FARMLAND
        );
        assert_eq!(rig.server.sessions[&1].durability[0], 4);
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet,
            ServerPacket::BlockUpdate {
                x: 8,
                y: 54,
                z: 8,
                block: ssc_world::FARMLAND
            }
        )));

        rig.server.world.set_block(10, 54, 8, ssc_world::DIRT);
        rig.server.world.set_block(10, 55, 8, ssc_world::STONE);
        rig.server.sessions.get_mut(&1).unwrap().player.pos.x = 10.5;
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(10, 54, 8), ssc_world::DIRT);
        assert_eq!(rig.server.sessions[&1].durability[0], 4);
        rig.server.world.set_block(10, 55, 8, AIR);
        rig.server.handle_use(1);
        assert_eq!(
            rig.server.world.block_or_air(10, 54, 8),
            ssc_world::FARMLAND
        );
        assert_eq!(rig.server.sessions[&1].durability[0], 3);
    }

    #[test]
    fn water_updates_farmland_moisture_and_wheat_can_only_plant_on_farmland() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.world.set_block(8, 54, 8, ssc_world::FARMLAND);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::WHEAT_SEEDS, 2));
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), ssc_world::WHEAT_0);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::WHEAT_SEEDS, 1))
        );
        assert_eq!(
            rig.server.block_edits[&(0, 0)][&(8, 55, 8)],
            ssc_world::WHEAT_0
        );

        rig.server.world.set_block(11, 55, 8, ssc_world::WATER);
        rig.server.update_farmland_moisture(11, 55, 8);
        assert_eq!(
            rig.server.world.block_or_air(8, 54, 8),
            ssc_world::FARMLAND_WET
        );
        assert_eq!(
            rig.server.block_edits[&(0, 0)][&(8, 54, 8)],
            ssc_world::FARMLAND_WET
        );
        rig.server.world.set_block(11, 55, 8, AIR);
        rig.server.update_farmland_moisture(11, 55, 8);
        assert_eq!(rig.server.world.block_or_air(8, 54, 8), ssc_world::FARMLAND);

        rig.server.world.set_block(8, 55, 8, AIR);
        rig.server.world.set_block(8, 54, 8, ssc_world::STONE);
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), AIR);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::WHEAT_SEEDS, 1))
        );
    }

    #[test]
    fn wheat_random_ticks_advance_hydrated_crops_and_persist_growth() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.time_of_day = 0.5;
        let seed = rig.server.gen.seed();
        let (tick, (x, y, z)) = (1..20_000)
            .find_map(|tick| {
                crate::block_ticks::samples(tick, seed, 0, 0, 8)
                    .into_iter()
                    .find(|((x, _, z), roll)| *x <= 11 && *z <= 15 && roll % 7 == 0)
                    .map(|((x, y, z), _)| (tick, (x, y, z)))
            })
            .expect("a deterministic growth sample");
        rig.server.tick = tick;
        rig.server.world.set_block(x, y - 1, z, ssc_world::FARMLAND);
        rig.server.world.set_block(x, y, z, ssc_world::WHEAT_2);
        rig.server.world.set_block(x + 2, y, z, ssc_world::WATER);
        rig.server.tick_blocks();
        let grown = rig.server.world.block_or_air(x, y, z);
        assert!(
            matches!(grown, ssc_world::WHEAT_3 | ssc_world::WHEAT_4),
            "age advanced from 2: {grown}"
        );
        assert_eq!(
            rig.server.world.block_or_air(x, y - 1, z),
            ssc_world::FARMLAND_WET
        );
        assert_eq!(rig.server.block_edits[&(0, 0)][&(x, y, z)], grown);

        rig.server.sessions.get_mut(&1).unwrap().player.joined = false;
        rig.server.tick += 1;
        rig.server.tick_blocks();
        assert_eq!(rig.server.world.block_or_air(x, y, z), grown);

        let path =
            std::env::temp_dir().join(format!("ssc-wheat-growth-save-{}.bin", std::process::id()));
        rig.server.cfg.world_path = Some(path.clone());
        rig.server.save_world();
        let (_, _, edits) = crate::persistence::load(&path).unwrap().unwrap();
        assert_eq!(edits[&(0, 0)][&(x, y, z)], grown);
        assert_eq!(edits[&(0, 0)][&(x, y - 1, z)], ssc_world::FARMLAND_WET);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn harvesting_and_breaking_farmland_drop_wheat_and_seeds() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server
            .world
            .set_block(8, 54, 8, ssc_world::FARMLAND_WET);
        rig.server.world.set_block(8, 55, 8, ssc_world::WHEAT_7);
        rig.server.break_block(1, 8, 55, 8);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), AIR);
        let inv = &rig.server.sessions[&1].inventory;
        assert!(inv.iter().any(|slot| *slot == Some((ssc_world::WHEAT, 1))));
        assert_eq!(
            rig.server.world.block_or_air(8, 54, 8),
            ssc_world::FARMLAND_WET,
            "crop harvest leaves soil"
        );

        rig.server.world.set_block(8, 55, 8, ssc_world::WHEAT_4);
        rig.server.break_block(1, 8, 54, 8);
        assert_eq!(rig.server.world.block_or_air(8, 54, 8), AIR);
        assert_eq!(
            rig.server.world.block_or_air(8, 55, 8),
            AIR,
            "unsupported crop pops"
        );
        assert_eq!(rig.server.block_edits[&(0, 0)][&(8, 55, 8)], AIR);
        assert!(rig
            .server
            .items
            .values()
            .any(|item| item.item == ssc_world::WHEAT_SEEDS && item.count == 1));
    }

    #[test]
    fn beds_craft_and_sleeping_skips_night_and_persists_respawn() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::WOOL, 3));
            session.inventory[1] = Some((ssc_world::OAK_PLANKS, 3));
        }
        rig.server
            .handle_packet(1, ClientPacket::Craft { recipe: 21 })
            .unwrap();
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .flatten()
            .any(|(item, _)| *item == ssc_world::BED));
        rig.server.world.set_block(8, 54, 8, ssc_world::BED);
        rig.server.time_of_day = 0.8;
        rig.server.handle_use(1);
        assert_eq!(rig.server.sessions[&1].spawn, Vec3::new(8.5, 55.0, 8.5));
        assert!((rig.server.time_of_day - 0.26).abs() < 0.0001);
        assert!(rig.drain(0).iter().any(|packet| matches!(packet,
            ServerPacket::TimeOfDay { t } if (*t - 0.26).abs() < 0.0001)));

        let path =
            std::env::temp_dir().join(format!("ssc-bed-sleep-world-{}.bin", std::process::id()));
        let player_id = [0x51; 16];
        rig.server.cfg.world_path = Some(path.clone());
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.player_id = Some(player_id);
            session.player_data_writable = true;
        }
        rig.server.save_player_data(1);
        let data = crate::persistence::load_player(
            &crate::persistence::player_path(&path, &player_id),
            &player_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(data.respawn, Some([8.5, 55.0, 8.5]));

        // Daytime does nothing.
        rig.server.time_of_day = 0.5;
        rig.server.sessions.get_mut(&1).unwrap().spawn = Vec3::new(1.5, 60.0, 1.5);
        rig.server.handle_use(1);
        assert_eq!(rig.server.sessions[&1].spawn, Vec3::new(1.5, 60.0, 1.5));
        // Nearby zombies prevent sleeping.
        rig.server.time_of_day = 0.8;
        rig.server.mobs.insert(
            FIRST_MOB_ID,
            crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, Vec3::new(10.5, 55.02, 8.5)),
        );
        rig.server.handle_use(1);
        assert_eq!(rig.server.sessions[&1].spawn, Vec3::new(1.5, 60.0, 1.5));
        rig.server.mobs.clear();
        // A block over the bed prevents sleeping.
        rig.server.world.set_block(8, 55, 8, ssc_world::STONE);
        assert!(!rig.server.use_bed(1, (8, 54, 8)));
        assert_eq!(rig.server.sessions[&1].spawn, Vec3::new(1.5, 60.0, 1.5));
        std::fs::remove_file(crate::persistence::player_path(&path, &player_id)).unwrap();
    }

    #[test]
    fn bread_crafts_from_three_wheat_in_the_inventory() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::WHEAT, 3));
        rig.server.sessions.get_mut(&1).unwrap().survival.hunger = 10;
        rig.server
            .handle_packet(1, ClientPacket::Craft { recipe: 20 })
            .unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::BREAD, 1))
        );
        rig.server.handle_use(1);
        assert!(rig.server.sessions[&1].eating.is_some());
    }

    #[test]
    fn creative_water_placement_keeps_bucket_and_dead_players_cannot_use_buckets() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Creative);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::WATER_BUCKET, 1));
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), ssc_world::WATER);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::WATER_BUCKET, 1))
        );
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::BUCKET, 1));
        rig.server.sessions.get_mut(&1).unwrap().survival.dead = true;
        rig.server.world.set_block(8, 55, 8, ssc_world::WATER);
        rig.server.handle_use(1);
        assert_eq!(rig.server.world.block_or_air(8, 55, 8), ssc_world::WATER);
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::BUCKET, 1))
        );
    }

    #[test]
    fn iron_ore_needs_stone_pickaxe_to_drop_raw_iron() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.world.set_block(8, 54, 8, ssc_world::IRON_ORE);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] =
            Some((ssc_world::WOODEN_PICKAXE, 1));
        rig.server.handle_dig_start(1);
        for _ in 0..50 {
            rig.server.tick();
        }
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .flatten()
            .all(|(item, _)| *item != ssc_world::RAW_IRON));

        rig.server.world.set_block(8, 54, 8, ssc_world::IRON_ORE);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::STONE_PICKAXE, 1));
        rig.server.handle_dig_start(1);
        for _ in 0..28 {
            rig.server.tick();
        }
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .flatten()
            .any(|(item, _)| *item == ssc_world::RAW_IRON));
    }

    #[test]
    fn deepslate_coal_requires_pickaxe_and_yields_coal() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server
            .world
            .set_block(8, 54, 8, ssc_world::DEEPSLATE_COAL_ORE);
        rig.server.handle_dig_start(1);
        for _ in 0..75 {
            rig.server.tick();
        }
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .flatten()
            .all(|(item, _)| *item != ssc_world::COAL));

        rig.server
            .world
            .set_block(8, 54, 8, ssc_world::DEEPSLATE_COAL_ORE);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::STONE_PICKAXE, 1));
        rig.server.handle_dig_start(1);
        for _ in 0..50 {
            rig.server.tick();
        }
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .flatten()
            .any(|(item, _)| *item == ssc_world::COAL));
    }

    #[test]
    fn pickaxe_wear_follows_slot_swaps_and_breaks_after_last_use() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::WOODEN_PICKAXE, 1));
        session.durability[0] = 2;
        rig.server
            .handle_packet(1, ClientPacket::SwapSlots { a: 0, b: 3 })
            .unwrap();
        assert_eq!(rig.server.sessions[&1].durability[3], 2);
        rig.server
            .handle_packet(1, ClientPacket::SelectSlot { slot: 3 })
            .unwrap();
        rig.server.handle_dig_start(1);
        for _ in 0..25 {
            rig.server.tick();
        }
        assert_eq!(rig.server.sessions[&1].durability[3], 1);
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet, ServerPacket::ToolDurability { slots } if slots[3] == 1
        )));
        rig.server.world.set_block(8, 54, 8, ssc_world::STONE);
        rig.server.handle_dig_start(1);
        for _ in 0..25 {
            rig.server.tick();
        }
        assert_eq!(rig.server.sessions[&1].inventory[3], None);
        assert_eq!(rig.server.sessions[&1].durability[3], 0);
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .flatten()
            .any(|(item, _)| *item == ssc_world::COBBLESTONE));
    }

    #[test]
    fn crafted_axe_mines_logs_and_same_type_swaps_preserve_wear() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::OAK_PLANKS, 3));
        session.inventory[1] = Some((ssc_world::STICK, 2));
        rig.server
            .handle_packet(1, ClientPacket::Craft { recipe: 4 })
            .unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::WOODEN_AXE, 1))
        );
        assert_eq!(rig.server.sessions[&1].durability[0], 60);
        rig.server.world.set_block(8, 54, 8, ssc_world::OAK_LOG);
        rig.server.handle_dig_start(1);
        for _ in 0..32 {
            rig.server.tick();
        }
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .flatten()
            .any(|(item, _)| *item == ssc_world::OAK_LOG));
        assert_eq!(rig.server.sessions[&1].durability[0], 59);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[4] = Some((ssc_world::WOODEN_AXE, 1));
        session.durability[4] = 7;
        rig.server
            .handle_packet(1, ClientPacket::SwapSlots { a: 0, b: 4 })
            .unwrap();
        assert_eq!(rig.server.sessions[&1].durability[0], 7);
        assert_eq!(rig.server.sessions[&1].durability[4], 59);
    }

    #[test]
    fn repair_request_preserves_wear_and_cannot_run_dead_or_in_creative() {
        for (mode, dead, allowed) in [
            (GameMode::Survival, false, true),
            (GameMode::Survival, true, false),
            (GameMode::Creative, false, false),
        ] {
            let mut rig = Rig::new_mode(1, 0.0, mode);
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::STONE_PICKAXE, 1));
            session.inventory[35] = Some((ssc_world::STONE_PICKAXE, 1));
            session.durability[0] = 11;
            session.durability[35] = 17;
            session.inv_dirty = false;
            if dead {
                session.survival.die();
            }
            rig.server
                .handle_packet(1, ClientPacket::Craft { recipe: 8 })
                .unwrap();
            let session = &rig.server.sessions[&1];
            if allowed {
                assert_eq!(session.durability[0], 34);
                assert_eq!(session.inventory[35], None);
                assert_eq!(session.durability[35], 0);
                assert!(session.inv_dirty);
            } else {
                assert_eq!(session.durability[0], 11);
                assert_eq!(session.durability[35], 17);
                assert_eq!(session.inventory[35], Some((ssc_world::STONE_PICKAXE, 1)));
                assert!(!session.inv_dirty);
            }
        }
    }

    #[test]
    fn axe_strips_logs_in_place_once_and_records_the_edit() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.world.set_block(10, 56, 8, ssc_world::OAK_LOG);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::WOODEN_AXE, 1));
        session.durability[0] = 2;
        rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        assert_eq!(
            rig.server.world.block_or_air(10, 56, 8),
            ssc_world::STRIPPED_OAK_LOG
        );
        assert_eq!(
            rig.server.block_edits[&(0, 0)][&(10, 56, 8)],
            ssc_world::STRIPPED_OAK_LOG
        );
        assert_eq!(rig.server.sessions[&1].durability[0], 1);
        assert_eq!(rig.server.sessions[&1].survival.exhaustion, 0.0);
        assert!(rig.server.items.is_empty());
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet,
            ServerPacket::BlockUpdate {
                block: ssc_world::STRIPPED_OAK_LOG,
                ..
            }
        )));

        rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        assert_eq!(
            rig.server.sessions[&1].durability[0], 1,
            "already stripped is a no-op"
        );
        rig.server.world.set_block(10, 56, 8, ssc_world::OAK_LOG);
        rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0], None,
            "last use breaks the axe"
        );
        assert_eq!(
            rig.server.world.block_or_air(10, 56, 8),
            ssc_world::STRIPPED_OAK_LOG
        );
    }

    #[test]
    fn stripping_requires_an_axe_reach_and_clear_line_of_sight() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.world.set_block(10, 56, 8, ssc_world::OAK_LOG);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::STONE_SHOVEL, 1));
        rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        assert_eq!(rig.server.world.block_or_air(10, 56, 8), ssc_world::OAK_LOG);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::STONE_AXE, 1));
        rig.server.world.set_block(9, 56, 8, ssc_world::STONE);
        rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        assert_eq!(rig.server.world.block_or_air(10, 56, 8), ssc_world::OAK_LOG);
        rig.server.world.set_block(9, 56, 8, ssc_world::AIR);
        rig.server.world.set_block(10, 56, 8, ssc_world::AIR);
        rig.server.world.set_block(14, 56, 8, ssc_world::OAK_LOG);
        rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        assert_eq!(rig.server.world.block_or_air(14, 56, 8), ssc_world::OAK_LOG);
    }

    #[test]
    fn creative_stripping_preserves_axe_durability() {
        let mut rig = Rig::new(1, 0.0);
        rig.server.world.set_block(10, 56, 8, ssc_world::OAK_LOG);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::STONE_AXE, 1));
        session.durability[0] = 7;
        rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        assert_eq!(
            rig.server.world.block_or_air(10, 56, 8),
            ssc_world::STRIPPED_OAK_LOG
        );
        assert_eq!(rig.server.sessions[&1].durability[0], 7);
    }

    #[test]
    fn shovel_mines_snow_into_stack_limited_snowballs_and_wears() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::STONE_SHOVEL, 1));
        session.durability[0] = 1;
        session.inventory[1] = Some((ssc_world::SNOWBALL, 15));
        rig.server.world.set_block(8, 54, 8, ssc_world::SNOW_BLOCK);
        rig.server.handle_dig_start(1);
        for _ in 0..4 {
            rig.server.tick();
        }
        let session = &rig.server.sessions[&1];
        assert_eq!(session.inventory[0], None, "last use breaks the shovel");
        assert_eq!(session.durability[0], 0);
        let snowballs: u16 = session
            .inventory
            .iter()
            .flatten()
            .filter(|(item, _)| *item == ssc_world::SNOWBALL)
            .map(|(_, count)| *count as u16)
            .sum();
        assert_eq!(snowballs, 19);
        assert_eq!(session.inventory[1], Some((ssc_world::SNOWBALL, 16)));
    }

    #[test]
    fn dropped_pickaxe_keeps_wear_on_pickup() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::STONE_PICKAXE, 1));
        session.durability[0] = 17;
        rig.server
            .handle_packet(1, ClientPacket::DropSelected)
            .unwrap();
        let item_id = *rig.server.items.keys().next().unwrap();
        assert_eq!(rig.server.items[&item_id].durability, 17);
        let pos = rig.server.sessions[&1].player.pos;
        let item = rig.server.items.get_mut(&item_id).unwrap();
        item.pos = pos;
        item.pickup_delay = 0;
        rig.server.tick_items(0.05);
        let slot = rig.server.sessions[&1]
            .inventory
            .iter()
            .position(|slot| *slot == Some((ssc_world::STONE_PICKAXE, 1)))
            .unwrap();
        assert_eq!(rig.server.sessions[&1].durability[slot], 17);
    }

    #[test]
    fn death_drop_keeps_pickaxe_wear() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[1] = Some((ssc_world::WOODEN_PICKAXE, 1));
        session.durability[1] = 5;
        session.survival.die();
        rig.server.tick();
        assert_eq!(rig.server.sessions[&1].durability[1], 0);
        assert!(rig
            .server
            .items
            .values()
            .any(|item| item.item == ssc_world::WOODEN_PICKAXE && item.durability == 5));
    }

    #[test]
    fn survival_dig_cancel_resets_progress() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.handle_dig_start(1);
        for _ in 0..60 {
            rig.server.tick();
        }
        let _ = rig.server.handle_packet(1, ClientPacket::DigStop).unwrap();
        assert!(rig.server.sessions[&1].survival.digging.is_none());
        for _ in 0..200 {
            rig.server.tick();
        }
        // Cancelled: the block was never broken, no drop.
        assert!(rig.server.sessions[&1].inventory.iter().flatten().count() == 0);
        assert!(rig.server.world.get_block(8, 54, 8) == Some(ssc_world::STONE));
    }

    #[test]
    fn survival_place_consumes_and_empty_rejects() {
        let mut rig = Rig::new_mode(1, -1.2, GameMode::Survival);
        // Empty inventory: nothing to place.
        rig.server.handle_use(1);
        assert!(rig.server.world.get_block(9, 55, 8) != Some(ssc_world::BRICKS));

        // Give 2 bricks and place twice: the second must fail (1 left is
        // consumed by the first, then 0).
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::BRICKS, 2));
        }
        rig.server.handle_use(1);
        assert!(rig.server.world.get_block(9, 55, 8) == Some(ssc_world::BRICKS));
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::BRICKS, 1))
        );
        // A different target so the second placement isn't blocked by the
        // first block: aim straight down is the player's own cell, so
        // instead clear the world block and reuse the same cell.
        rig.server.world.set_block(9, 55, 8, AIR);
        rig.server.handle_use(1);
        assert!(rig.server.world.get_block(9, 55, 8) == Some(ssc_world::BRICKS));
        assert_eq!(rig.server.sessions[&1].inventory[0], None);
        // Inventory empty now: no more placements.
        rig.server.world.set_block(9, 55, 8, AIR);
        rig.server.handle_use(1);
        assert!(rig.server.world.get_block(9, 55, 8) != Some(ssc_world::BRICKS));
    }

    #[test]
    fn eating_apple_restores_hunger_and_consumes() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::APPLE, 3));
            session.survival.hunger = 10;
        }
        rig.server.handle_use(1);
        for _ in 0..crate::survival::EAT_TICKS {
            rig.server.tick_eating();
        }
        let session = &rig.server.sessions[&1];
        assert_eq!(session.survival.hunger, 14, "apple restores 4 hunger");
        assert!((session.survival.saturation - 7.4).abs() < 0.0001);
        assert_eq!(session.inventory[0], Some((ssc_world::APPLE, 2)));
        // Unlike other food, Bedrock apples are edible at full hunger.
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.survival.hunger = 20;
        }
        rig.server.handle_use(1);
        for _ in 0..crate::survival::EAT_TICKS {
            rig.server.tick_eating();
        }
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 1))
        );
        assert!((rig.server.sessions[&1].survival.saturation - 9.8).abs() < 0.0001);
    }

    #[test]
    fn food_requires_32_server_ticks_and_packet_spam_does_not_speed_it_up() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::APPLE, 3));
        rig.server.sessions.get_mut(&1).unwrap().survival.hunger = 10;
        for _ in 0..100 {
            rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        }
        assert_eq!(rig.server.sessions[&1].survival.hunger, 10);
        for _ in 0..31 {
            rig.server.handle_packet(1, ClientPacket::Place).unwrap();
            rig.server.tick();
        }
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 3))
        );
        rig.server.tick();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 2))
        );
        assert_eq!(rig.server.sessions[&1].survival.hunger, 14);
        assert!(rig.server.sessions[&1].eating.is_none());
        assert!(rig.drain(0).iter().any(|packet| matches!(
            packet, ServerPacket::Sound { sound, .. } if *sound == SoundId::Eat.to_u16()
        )));
    }

    #[test]
    fn releasing_use_or_changing_held_food_cancels_eating() {
        for cancel in [
            ClientPacket::UseStop,
            ClientPacket::SelectSlot { slot: 1 },
            ClientPacket::SwapSlots { a: 0, b: 1 },
            ClientPacket::DropSelected,
            ClientPacket::DigStart,
        ] {
            let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
            rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::APPLE, 3));
            rig.server.sessions.get_mut(&1).unwrap().survival.hunger = 10;
            rig.server.handle_packet(1, ClientPacket::Place).unwrap();
            for _ in 0..31 {
                rig.server.tick_eating();
            }
            rig.server.handle_packet(1, cancel.clone()).unwrap();
            rig.server.tick_eating();
            assert!(rig.server.sessions[&1].eating.is_none(), "{cancel:?}");
            assert_eq!(rig.server.sessions[&1].survival.hunger, 10, "{cancel:?}");
        }
    }

    #[test]
    fn canceled_eating_restarts_from_zero_and_dead_players_do_not_finish() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::APPLE, 3));
        rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        for _ in 0..31 {
            rig.server.tick_eating();
        }
        rig.server.handle_packet(1, ClientPacket::UseStop).unwrap();
        rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        rig.server.tick_eating();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 3))
        );
        assert_eq!(
            rig.server.sessions[&1].eating.as_ref().unwrap().remaining,
            31
        );
        rig.server.sessions.get_mut(&1).unwrap().survival.die();
        for _ in 0..32 {
            rig.server.tick_eating();
        }
        assert!(rig.server.sessions[&1].eating.is_none());
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 3))
        );
    }

    #[test]
    fn eating_slows_movement_and_prevents_sprinting() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::APPLE, 3));
        session.player.keys = keys::FORWARD | keys::SPRINT;
        session.player.on_ground = true;
        let before = session.player.pos.x;
        rig.server.handle_packet(1, ClientPacket::Place).unwrap();
        rig.server.tick();
        let session = &rig.server.sessions[&1];
        assert!((session.player.pos.x - before - WALK_SPEED * 0.2 / TICK_RATE).abs() < 0.001);
        assert_eq!(session.survival.exhaustion, 0.0);
        assert!(rig
            .drain(0)
            .iter()
            .any(|packet| matches!(packet, ServerPacket::EatingProgress { remaining: 31 })));
        rig.server.handle_packet(1, ClientPacket::UseStop).unwrap();
        rig.server.tick();
        assert!(rig
            .drain(0)
            .iter()
            .any(|packet| matches!(packet, ServerPacket::EatingProgress { remaining: 0 })));
    }

    #[test]
    fn eating_raw_porkchop_restores_three_hunger() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::PORKCHOP, 1));
            session.survival.hunger = 10;
        }
        rig.server.handle_use(1);
        for _ in 0..crate::survival::EAT_TICKS {
            rig.server.tick_eating();
        }
        let session = &rig.server.sessions[&1];
        assert_eq!(session.survival.hunger, 13);
        assert!((session.survival.saturation - 6.8).abs() < 0.0001);
        assert_eq!(session.inventory[0], None);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::PORKCHOP, 1));
        rig.server.sessions.get_mut(&1).unwrap().survival.hunger = 20;
        rig.server.handle_use(1);
        for _ in 0..crate::survival::EAT_TICKS {
            rig.server.tick_eating();
        }
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::PORKCHOP, 1))
        );
    }

    #[test]
    fn dropped_items_are_picked_up_into_inventory() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let pos = rig.server.sessions[&1].player.pos;
        rig.server.spawn_item_entity(ssc_world::APPLE, 3, pos);

        for _ in 0..10 {
            rig.server.tick_items(0.05);
        }

        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 3))
        );
        assert!(rig.server.items.is_empty());
    }

    #[test]
    fn dropped_items_do_not_home_toward_nearby_players() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let player_pos = rig.server.sessions[&1].player.pos;
        rig.server
            .spawn_item_entity(ssc_world::APPLE, 1, player_pos + Vec3::new(4.0, 0.0, 0.0));
        let item = rig.server.items.values_mut().next().unwrap();
        item.vel = Vec3::ZERO;
        item.pickup_delay = 100;

        rig.server.tick_items(0.05);

        assert_eq!(rig.server.items.values().next().unwrap().vel.x, 0.0);
    }

    #[test]
    fn item_pickup_reaches_above_the_head_and_diagonally_but_respects_delay() {
        for offset in [Vec3::new(0.0, 2.0, 0.0), Vec3::new(1.4, 0.0, 1.4)] {
            let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
            let pos = rig.server.sessions[&1].player.pos + offset;
            rig.server.spawn_item_entity(ssc_world::APPLE, 1, pos);
            let item = rig.server.items.values_mut().next().unwrap();
            item.vel = Vec3::ZERO;
            item.pickup_delay = 2;
            rig.server.tick_items(0.05);
            assert_eq!(rig.server.items.len(), 1);
            rig.server.tick_items(0.05);
            assert!(rig.server.items.is_empty());
            assert_eq!(
                rig.server.sessions[&1].inventory[0],
                Some((ssc_world::APPLE, 1))
            );
        }
    }

    #[test]
    fn floating_loot_keeps_stack_metadata_pauses_and_can_be_picked_up() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        for x in 20..25 {
            for z in 6..11 {
                for y in 60..64 {
                    rig.server.world.set_block(x, y, z, ssc_world::WATER);
                }
            }
        }
        rig.server.spawn_item_entity_with_durability(
            ssc_world::IRON_SWORD,
            1,
            Vec3::new(22.5, 60.1, 8.5),
            17,
        );
        let id = *rig.server.items.keys().next().unwrap();
        for _ in 0..400 {
            rig.server.tick_items(0.05);
        }
        let item = &rig.server.items[&id];
        assert!((63.8..64.0).contains(&item.pos.y));
        assert_eq!(item.age, 400);
        assert_eq!(item.durability, 17);
        let packet = item_state(id, item);
        assert_eq!(ServerPacket::decode(&packet.encode()).unwrap(), packet);
        let before = item.pos;
        rig.server.sessions.get_mut(&1).unwrap().player.joined = false;
        rig.server.tick_items(0.05);
        assert_eq!(rig.server.items[&id].pos, before);
        assert_eq!(rig.server.items[&id].age, 400);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.player.joined = true;
        session.player.pos = before;
        rig.server.tick_items(0.05);
        assert!(!rig.server.items.contains_key(&id));
        let slot = rig.server.sessions[&1]
            .inventory
            .iter()
            .position(|slot| *slot == Some((ssc_world::IRON_SWORD, 1)))
            .unwrap();
        assert_eq!(rig.server.sessions[&1].durability[slot], 17);
    }

    #[test]
    fn expired_item_cannot_be_picked_up_on_its_last_tick() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let pos = rig.server.sessions[&1].player.pos;
        rig.server.spawn_item_entity(ssc_world::APPLE, 1, pos);
        let id = rig.server.next_item_id - 1;
        let item = rig.server.items.get_mut(&id).unwrap();
        item.age = ITEM_LIFETIME_TICKS - 1;
        item.pickup_delay = 0;

        rig.server.tick_items(0.05);

        assert!(!rig.server.items.contains_key(&id));
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .all(Option::is_none));
    }

    #[test]
    fn items_fall_below_build_limit_and_despawn_only_at_item_void_depth() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().player.pos.y = -100.0;
        rig.server
            .spawn_item_entity(ssc_world::APPLE, 1, Vec3::new(12.5, -65.0, 8.5));
        let id = *rig.server.items.keys().next().unwrap();
        rig.server.tick_items(0.05);
        assert!(
            rig.server.items.contains_key(&id),
            "build minimum is not the item void depth"
        );
        rig.server.items.get_mut(&id).unwrap().pos.y = -127.9;
        rig.server.items.get_mut(&id).unwrap().vel.y = -10.0;
        rig.server.tick_items(0.05);
        assert!(
            !rig.server.items.contains_key(&id),
            "crossing Y=-128 removes loot before pickup/save"
        );
        assert!(rig.drain(0).iter().any(
            |packet| matches!(packet, ServerPacket::EntityRemove { id: removed } if *removed == id)
        ));
    }

    #[test]
    fn loot_in_the_void_can_save_reload_and_pause_offline() {
        let world_path =
            std::env::temp_dir().join(format!("ssc-item-void-save-{}.bin", std::process::id()));
        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(world_path.clone());
        let mut server = Server::empty(cfg.clone());
        server.spawn_item_entity(ssc_world::APPLE, 2, Vec3::new(8.5, -90.0, 8.5));
        server.items.values_mut().next().unwrap().vel.y = -3.0;
        server.save_item_entities();
        let mut restored = Server::empty(cfg);
        assert_eq!(restored.items.values().next().unwrap().pos.y, -90.0);
        restored.tick_items(0.05);
        assert_eq!(restored.items.values().next().unwrap().pos.y, -90.0);
        std::fs::remove_file(crate::persistence::item_save_path(&world_path)).unwrap();
    }

    #[test]
    fn block_break_overflow_becomes_a_world_item() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server
            .sessions
            .get_mut(&1)
            .unwrap()
            .inventory
            .fill(Some((ssc_world::STONE, 64)));
        rig.server.world.set_block(8, 54, 8, ssc_world::GRASS_BLOCK);

        rig.server.break_block(1, 8, 54, 8);

        assert!(rig
            .server
            .items
            .values()
            .any(|item| { item.item == ssc_world::DIRT && item.count == 1 }));
    }

    #[test]
    fn copper_ore_partial_drop_syncs_inventory_and_preserves_overflow() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory.fill(Some((ssc_world::STONE, 64)));
        session.inventory[0] = Some((ssc_world::STONE_PICKAXE, 1));
        session.inventory[1] = Some((ssc_world::RAW_COPPER, 63));
        session.inv_dirty = false;
        rig.server
            .world
            .set_block(8, 54, 8, ssc_world::DEEPSLATE_COPPER_ORE);
        let rng = (hash_tick(rig.server.tick, 8, 8) & 0xFFFF) as f32 / 65535.0;
        let count = crate::survival::drop_count(ssc_world::DEEPSLATE_COPPER_ORE, rng);

        rig.server.break_block(1, 8, 54, 8);

        let session = &rig.server.sessions[&1];
        assert_eq!(session.inventory[1], Some((ssc_world::RAW_COPPER, 64)));
        assert!(session.inv_dirty);
        assert!((session.survival.exhaustion - 0.005).abs() < 0.0001);
        assert!(rig
            .server
            .items
            .values()
            .any(|item| { item.item == ssc_world::RAW_COPPER && item.count == count - 1 }));
    }

    #[test]
    fn partial_item_pickup_keeps_remainder_and_marks_inventory_dirty() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let pos = rig.server.sessions[&1].player.pos;
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory.fill(Some((ssc_world::STONE, 64)));
            session.inventory[0] = Some((ssc_world::APPLE, 63));
            session.inv_dirty = false;
        }
        let id = rig.server.next_item_id;
        rig.server.spawn_item_entity(ssc_world::APPLE, 2, pos);

        for _ in 0..10 {
            rig.server.tick_items(0.05);
        }

        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 64))
        );
        assert!(rig.server.sessions[&1].inv_dirty);
        assert_eq!(rig.server.items[&id].count, 1);
    }

    #[test]
    fn partial_pickup_passes_remainder_to_next_nearby_player() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        let pos = rig.server.sessions[&1].player.pos;
        {
            let first = rig.server.sessions.get_mut(&1).unwrap();
            first.inventory.fill(Some((ssc_world::STONE, 64)));
            first.inventory[0] = Some((ssc_world::APPLE, 63));
            first.inv_dirty = false;
        }
        rig.server.sessions.get_mut(&2).unwrap().player.pos = pos + Vec3::new(0.5, 0.0, 0.0);
        rig.server.spawn_item_entity(ssc_world::APPLE, 2, pos);
        let item_id = rig.server.next_item_id - 1;
        rig.server.items.get_mut(&item_id).unwrap().pickup_delay = 0;

        rig.server.tick_items(0.05);

        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 64))
        );
        assert!(rig.server.sessions[&1].inv_dirty);
        assert_eq!(
            rig.server.sessions[&2].inventory[0],
            Some((ssc_world::APPLE, 1))
        );
        assert!(!rig.server.items.contains_key(&item_id));
    }

    #[test]
    fn nearby_item_stacks_merge_up_to_max_stack() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let pos = rig.server.sessions[&1].player.pos + Vec3::new(3.0, 0.0, 0.0);
        rig.server.spawn_item_entity(ssc_world::APPLE, 60, pos);
        let first = rig.server.next_item_id - 1;
        rig.server.spawn_item_entity(ssc_world::APPLE, 10, pos);
        let second = rig.server.next_item_id - 1;

        rig.server.tick_items(0.05);

        assert_eq!(rig.server.items[&first].count, 64);
        assert_eq!(rig.server.items[&second].count, 6);
    }

    #[test]
    fn merging_items_preserves_the_longer_pickup_delay() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let pos = rig.server.sessions[&1].player.pos + Vec3::new(3.0, 0.0, 0.0);
        rig.server.spawn_item_entity(ssc_world::APPLE, 1, pos);
        let first = rig.server.next_item_id - 1;
        rig.server.items.get_mut(&first).unwrap().pickup_delay = 0;
        rig.server.spawn_item_entity(ssc_world::APPLE, 1, pos);
        let second = rig.server.next_item_id - 1;
        rig.server.items.get_mut(&second).unwrap().pickup_delay = 9;

        rig.server.tick_items(0.05);

        assert_eq!(rig.server.items[&first].count, 2);
        assert_eq!(rig.server.items[&first].pickup_delay, 8);
        assert!(!rig.server.items.contains_key(&second));
    }

    #[test]
    fn full_nearest_inventory_does_not_block_another_players_pickup() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        let pos = rig.server.sessions[&1].player.pos;
        {
            let full_player = rig.server.sessions.get_mut(&1).unwrap();
            full_player.inventory.fill(Some((ssc_world::STONE, 64)));
        }
        rig.server.sessions.get_mut(&2).unwrap().player.pos = pos + Vec3::new(0.5, 0.0, 0.0);
        rig.server.spawn_item_entity(ssc_world::APPLE, 1, pos);

        for _ in 0..10 {
            rig.server.tick_items(0.05);
        }

        assert_eq!(
            rig.server.sessions[&2].inventory[0],
            Some((ssc_world::APPLE, 1))
        );
        assert!(rig.server.items.is_empty());
    }

    #[test]
    fn rotten_flesh_eating_applies_or_skips_hunger_effect_from_seeded_roll() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::ROTTEN_FLESH, 2));
            session.survival.hunger = 10;
        }
        let should_apply = hash_tick(rig.server.tick, 1, 0) % 5 != 0;
        rig.server.handle_use(1);
        for _ in 0..crate::survival::EAT_TICKS {
            rig.server.tick_eating();
        }
        let session = &rig.server.sessions[&1];
        assert_eq!(session.survival.hunger, 14);
        assert_eq!(session.inventory[0], Some((ssc_world::ROTTEN_FLESH, 1)));
        assert_eq!(session.survival.hunger_effect_ticks > 0, should_apply);
    }

    #[test]
    fn hunger_effect_status_syncs_and_clears_on_expiry() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.survival.apply_rotten_flesh_effect();
            session.status_dirty = true;
        }
        rig.server.tick();
        assert!(rig.drain(0).iter().any(|p| {
            matches!(
                p,
                ServerPacket::Status {
                    hunger_effect_ticks: 599,
                    ..
                }
            )
        }));
        rig.server
            .sessions
            .get_mut(&1)
            .unwrap()
            .survival
            .hunger_effect_ticks = 1;
        rig.server.tick();
        assert!(rig.drain(0).iter().any(|p| {
            matches!(
                p,
                ServerPacket::Status {
                    hunger_effect_ticks: 0,
                    ..
                }
            )
        }));
    }

    #[test]
    fn damage_flows_to_status_packet_and_death_respawns() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        // 30 damage: death.
        let session = rig.server.sessions.get_mut(&1).unwrap();
        let mut fx = crate::survival::TickEffects::default();
        session.survival.apply_damage(30.0, &mut fx);
        assert!(session.survival.dead);
        session.survival.respawn_at =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));

        // The tick loop respawns the player at the spawn point.
        rig.server.tick();
        let session = &rig.server.sessions[&1];
        assert!(!session.survival.dead);
        assert_eq!(session.survival.health, crate::survival::MAX_HEALTH);
        assert_eq!(session.player.pos, session.spawn);
        let packets = rig.drain(0);
        assert!(packets
            .iter()
            .any(|p| matches!(p, ServerPacket::Status { health, .. } if *health > 0.0)));
    }

    #[test]
    fn death_drops_inventory_once_and_respawn_does_not_duplicate_it() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::APPLE, 3));
            session.inventory[1] = Some((ssc_world::DIRT, 5));
            let mut fx = crate::survival::TickEffects::default();
            session.survival.apply_damage(30.0, &mut fx);
        }

        rig.server.tick();
        assert!(rig.server.sessions[&1]
            .inventory
            .iter()
            .all(Option::is_none));
        assert_eq!(
            rig.server
                .items
                .values()
                .map(|item| item.count as u16)
                .sum::<u16>(),
            8
        );

        rig.server.sessions.get_mut(&1).unwrap().survival.respawn_at =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
        rig.server.tick();
        assert_eq!(
            rig.server
                .items
                .values()
                .map(|item| item.count as u16)
                .sum::<u16>(),
            8
        );
        assert!(!rig.server.sessions[&1].survival.dead);
    }

    #[test]
    fn creative_players_are_immune_to_mob_attacks() {
        let mut rig = Rig::new(1, 0.0);
        let mob_id = FIRST_MOB_ID;
        rig.server.mobs.insert(
            mob_id,
            crate::mobs::Mob::new(
                crate::mobs::MobKind::Zombie,
                rig.server.sessions[&1].player.pos + Vec3::new(0.0, 0.0, 1.0),
            ),
        );

        for _ in 0..10 {
            rig.server.tick();
        }

        assert_eq!(
            rig.server.sessions[&1].survival.health,
            crate::survival::MAX_HEALTH,
            "creative mode must not take mob damage"
        );
    }

    #[test]
    fn zombie_hit_costs_defender_exhaustion() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let pos = rig.server.sessions[&1].player.pos;
        rig.server.mobs.insert(
            FIRST_MOB_ID,
            crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, pos + Vec3::new(1.0, 0.0, 0.0)),
        );
        for _ in 0..20 {
            rig.server.tick();
            if rig.server.sessions[&1].survival.health < crate::survival::MAX_HEALTH {
                break;
            }
        }
        assert!(rig.server.sessions[&1].survival.health < crate::survival::MAX_HEALTH);
        assert!((rig.server.sessions[&1].survival.exhaustion - 0.1).abs() < 0.0001);
        assert!(
            rig.server.sessions[&1].player.vel.x < 0.0,
            "zombie pushes the player away"
        );
        assert!(!rig.server.sessions[&1].player.on_ground);
    }

    #[test]
    fn creative_set_hotbar_slot_rejected_in_survival() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server
            .handle_packet(
                1,
                ClientPacket::SetHotbarSlot {
                    slot: 0,
                    block: ssc_world::BRICKS,
                },
            )
            .unwrap();
        assert_eq!(rig.server.sessions[&1].inventory[0], None);
    }

    #[test]
    fn swap_slots_moves_items() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::APPLE, 1));
            session.inventory[20] = Some((ssc_world::DIRT, 5));
        }
        rig.server
            .handle_packet(1, ClientPacket::SwapSlots { a: 0, b: 20 })
            .unwrap();
        let inv = &rig.server.sessions[&1].inventory;
        assert_eq!(inv[0], Some((ssc_world::DIRT, 5)));
        assert_eq!(inv[20], Some((ssc_world::APPLE, 1)));
    }

    #[test]
    fn swap_slots_merges_matching_stacks_and_preserves_remainder() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::APPLE, 50));
            session.inventory[1] = Some((ssc_world::APPLE, 30));
        }
        rig.server
            .handle_packet(1, ClientPacket::SwapSlots { a: 0, b: 1 })
            .unwrap();
        assert_eq!(
            rig.server.sessions[&1].inventory[0],
            Some((ssc_world::APPLE, 16))
        );
        assert_eq!(
            rig.server.sessions[&1].inventory[1],
            Some((ssc_world::APPLE, 64))
        );
    }

    #[test]
    fn dead_players_cannot_dig_or_move_inventory() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let target = (8, 54, 8);
        rig.server
            .world
            .set_block(target.0, target.1, target.2, ssc_world::DIRT);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::APPLE, 1));
            session.survival.die();
        }
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        rig.server
            .handle_packet(1, ClientPacket::SwapSlots { a: 0, b: 1 })
            .unwrap();

        let session = &rig.server.sessions[&1];
        assert!(session.survival.digging.is_none());
        assert_eq!(session.inventory[0], Some((ssc_world::APPLE, 1)));
        assert!(session.inventory[1].is_none());
        assert_eq!(
            rig.server.world.get_block(target.0, target.1, target.2),
            Some(ssc_world::DIRT)
        );
    }

    #[test]
    fn non_finite_input_cannot_corrupt_player_position() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let before = rig.server.sessions[&1].player.pos;
        for (yaw, pitch) in [(f32::NAN, 0.0), (0.0, f32::INFINITY)] {
            assert!(rig
                .server
                .handle_packet(
                    1,
                    ClientPacket::Input {
                        keys: keys::FORWARD,
                        yaw,
                        pitch,
                    },
                )
                .is_err());
        }
        rig.server.tick();
        let player = &rig.server.sessions[&1].player;
        assert_eq!(player.pos.x, before.x);
        assert_eq!(player.pos.z, before.z);
        assert!(player.pos.y.is_finite());
        assert!(player.yaw.is_finite() && player.pitch.is_finite());
    }

    #[test]
    fn survival_void_damage_kills_and_respawns_below_world() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let spawn = rig.server.sessions[&1].player.pos;
        rig.server.sessions.get_mut(&1).unwrap().spawn = spawn;
        rig.server.sessions.get_mut(&1).unwrap().player.pos.y = ssc_world::MIN_Y as f32 - 0.1;
        for _ in 0..10 {
            rig.server.tick();
        }
        assert_eq!(rig.server.sessions[&1].survival.health, 16.0);
        for _ in 0..40 {
            rig.server.tick();
        }
        assert!(rig.server.sessions[&1].survival.dead);
        rig.server.sessions.get_mut(&1).unwrap().survival.respawn_at =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
        rig.server.tick();
        assert_eq!(rig.server.sessions[&1].player.pos, spawn);
        assert_eq!(
            rig.server.sessions[&1].survival.health,
            crate::survival::MAX_HEALTH
        );
    }

    #[test]
    fn creative_players_survive_the_void_floor() {
        let mut rig = Rig::new(1, 0.0);
        rig.server.sessions.get_mut(&1).unwrap().player.pos.y = ssc_world::MIN_Y as f32 - 0.1;
        for _ in 0..50 {
            rig.server.tick();
        }
        let player = &rig.server.sessions[&1];
        assert!(!player.survival.dead);
        assert_eq!(player.survival.health, crate::survival::MAX_HEALTH);
        assert_eq!(player.player.pos.y, (ssc_world::MIN_Y - 41) as f32);
    }

    #[test]
    fn solid_wall_blocks_pvp_but_not_unobstructed_attacks() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        rig.server.world.set_block(10, 56, 8, ssc_world::STONE);
        let before = rig.server.sessions[&2].survival.health;

        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        assert_eq!(rig.server.sessions[&2].survival.health, before);
        assert_eq!(rig.server.sessions[&1].survival.exhaustion, 0.0);
        assert_eq!(rig.server.sessions[&2].survival.exhaustion, 0.0);

        rig.server.world.set_block(10, 56, 8, AIR);
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        assert_eq!(rig.server.sessions[&2].survival.health, before - 1.0);
        assert!((rig.server.sessions[&1].survival.exhaustion - 0.1).abs() < 0.0001);
        assert!((rig.server.sessions[&2].survival.exhaustion - 0.1).abs() < 0.0001);
    }

    #[test]
    fn melee_reach_is_three_blocks_to_the_nearest_hitbox_face() {
        for (offset, hit) in [(3.29, true), (3.31, false), (4.0, false)] {
            let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
            let pos = rig.server.sessions[&1].player.pos;
            rig.server.sessions.get_mut(&2).unwrap().player.pos = pos + Vec3::new(offset, 0.0, 0.0);
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::STONE_AXE, 1));
            session.durability[0] = 132;
            rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
            assert_eq!(
                rig.server.sessions[&2].survival.health,
                if hit { 15.0 } else { 20.0 }
            );
            assert_eq!(
                rig.server.sessions[&1].durability[0],
                if hit { 130 } else { 132 }
            );
        }
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.world.set_block(12, 56, 8, ssc_world::STONE);
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        let dig = rig.server.sessions[&1].survival.digging.as_ref().unwrap();
        assert_eq!(
            (dig.x, dig.y, dig.z),
            (12, 56, 8),
            "block reach remains 4.5"
        );
    }

    #[test]
    fn stone_pickaxe_hits_for_bedrock_damage_and_wears_twice() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::STONE_PICKAXE, 1));
        rig.server.sessions.get_mut(&1).unwrap().durability[0] = 2;
        let before = rig.server.sessions[&2].survival.health;
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        assert_eq!(rig.server.sessions[&2].survival.health, before - 4.0);
        assert_eq!(rig.server.sessions[&1].inventory[0], None);
        assert_eq!(rig.server.sessions[&1].durability[0], 0);
    }

    #[test]
    fn falling_critical_hits_use_authoritative_state_and_allow_bedrock_sprinting() {
        for (vy, grounded, wet, flying, expected) in [
            (-1.0, false, false, false, 7.5),
            (1.0, false, false, false, 5.0),
            (-1.0, true, false, false, 5.0),
            (-1.0, false, true, false, 5.0),
            (-1.0, false, false, true, 5.0),
        ] {
            let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::STONE_AXE, 1));
            session.durability[0] = 132;
            session.player.vel.y = vy;
            session.player.on_ground = grounded;
            session.player.in_water = wet;
            session.player.flying = flying;
            session.player.keys = keys::FORWARD | keys::SPRINT;
            rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
            assert_eq!(rig.server.sessions[&2].survival.health, 20.0 - expected);
            assert_eq!(rig.server.sessions[&1].durability[0], 130);
        }
    }

    #[test]
    fn a_stronger_critical_applies_only_excess_damage_during_immunity() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory[0] = Some((ssc_world::STONE_AXE, 1));
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        assert_eq!(rig.server.sessions[&2].survival.health, 15.0);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.player.vel.y = -1.0;
        session.player.on_ground = false;
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        assert_eq!(rig.server.sessions[&2].survival.health, 12.5);
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        assert_eq!(rig.server.sessions[&2].survival.health, 12.5);
    }

    #[test]
    fn immune_pvp_hits_do_not_cost_tool_wear_or_exhaustion() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::STONE_AXE, 1));
        session.durability[0] = 132;
        for _ in 0..30 {
            rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        }
        assert_eq!(rig.server.sessions[&2].survival.health, 15.0);
        assert_eq!(rig.server.sessions[&1].durability[0], 130);
        assert!((rig.server.sessions[&1].survival.exhaustion - 0.1).abs() < 0.0001);
        assert!((rig.server.sessions[&2].survival.exhaustion - 0.1).abs() < 0.0001);
        assert!(rig.server.sessions[&1].survival.digging.is_none());
    }

    #[test]
    fn grounded_pvp_knockback_survives_the_next_movement_step() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&2).unwrap().player.on_ground = true;
        let before = rig.server.sessions[&2].player.pos;
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        let victim = &mut rig.server.sessions.get_mut(&2).unwrap().player;
        assert!(!victim.on_ground);
        assert!(victim.vel.x > 0.0);
        step_player(&rig.server.world, victim, 0.05, false, false, false);
        assert!(victim.pos.x > before.x + 0.2);
        assert!(victim.pos.y > before.y);
        let velocity = victim.vel;
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        assert_eq!(
            rig.server.sessions[&2].player.vel, velocity,
            "immune hits add no extra impulse"
        );
    }

    #[test]
    fn immune_mob_hits_do_not_cost_tool_wear_or_exhaustion() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let session = rig.server.sessions.get_mut(&1).unwrap();
        session.inventory[0] = Some((ssc_world::STONE_AXE, 1));
        session.durability[0] = 132;
        rig.server.mobs.insert(
            FIRST_MOB_ID,
            crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, Vec3::new(10.5, 55.02, 8.5)),
        );
        for _ in 0..30 {
            rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        }
        assert_eq!(rig.server.mobs[&FIRST_MOB_ID].health, 15.0);
        assert_eq!(rig.server.sessions[&1].durability[0], 130);
        assert!((rig.server.sessions[&1].survival.exhaustion - 0.1).abs() < 0.0001);
        assert!(rig.server.sessions[&1].survival.digging.is_none());
    }

    #[test]
    fn solid_wall_blocks_mob_attacks() {
        let mut rig = Rig::new_mode(1, -0.25, GameMode::Survival);
        let mob_id = FIRST_MOB_ID;
        rig.server.mobs.insert(
            mob_id,
            crate::mobs::Mob::new(
                crate::mobs::MobKind::Pig,
                rig.server.sessions[&1].player.pos + Vec3::new(3.0, 0.0, 0.0),
            ),
        );
        let before = rig.server.mobs[&mob_id].health;
        rig.server.world.set_block(10, 56, 8, ssc_world::STONE);
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        assert_eq!(rig.server.mobs[&mob_id].health, before);

        rig.server.world.set_block(10, 56, 8, AIR);
        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();
        assert_eq!(rig.server.mobs[&mob_id].health, before - 1.0);
    }

    #[test]
    fn dead_players_do_not_intercept_attacks() {
        let mut rig = Rig::new_mode(3, 0.0, GameMode::Survival);
        let attacker = rig.server.sessions[&1].player.pos;
        rig.server.sessions.get_mut(&2).unwrap().player.pos = attacker + Vec3::new(2.0, 0.0, 0.0);
        rig.server.sessions.get_mut(&3).unwrap().player.pos = attacker + Vec3::new(3.0, 0.0, 0.0);
        rig.server.sessions.get_mut(&2).unwrap().survival.die();
        let health = rig.server.sessions[&3].survival.health;

        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();

        assert_eq!(rig.server.sessions[&3].survival.health, health - 1.0);
    }

    #[test]
    fn dead_mobs_do_not_intercept_attacks() {
        let mut rig = Rig::new_mode(1, -0.2, GameMode::Survival);
        let attacker = rig.server.sessions[&1].player.pos;
        let mut dead = crate::mobs::Mob::new(
            crate::mobs::MobKind::Zombie,
            attacker + Vec3::new(2.0, 0.0, 0.0),
        );
        dead.dead = true;
        rig.server.mobs.insert(FIRST_MOB_ID, dead);
        rig.server.mobs.insert(
            FIRST_MOB_ID + 1,
            crate::mobs::Mob::new(
                crate::mobs::MobKind::Zombie,
                attacker + Vec3::new(3.0, 0.0, 0.0),
            ),
        );
        let health = rig.server.mobs[&(FIRST_MOB_ID + 1)].health;

        rig.server.handle_packet(1, ClientPacket::DigStart).unwrap();

        assert_eq!(rig.server.mobs[&(FIRST_MOB_ID + 1)].health, health - 1.0);
    }

    #[test]
    fn entity_states_reach_every_client() {
        let mut rig = Rig::new(2, 0.0);
        // Simulate one broadcast tick's worth of state.
        let s1 = {
            let p = &rig.server.sessions[&1].player;
            entity_state(1, p)
        };
        let s2 = {
            let p = &rig.server.sessions[&2].player;
            entity_state(2, p)
        };
        let _ = rig.server.broadcast(&s1);
        let _ = rig.server.broadcast(&s2);

        for c in 0..2 {
            let packets = rig.drain(c);
            let ids: Vec<EntityId> = packets
                .iter()
                .filter_map(|p| match p {
                    ServerPacket::EntityState { id, .. } => Some(*id),
                    _ => None,
                })
                .collect();
            assert!(
                ids.contains(&1) && ids.contains(&2),
                "client {c} got {ids:?}"
            );
        }
    }

    #[test]
    fn player_column_uses_floor_at_negative_coordinates() {
        let mut rig = Rig::new(1, 0.0);
        rig.server.sessions.get_mut(&1).unwrap().player.pos = Vec3::new(-0.5, 55.0, -16.1);
        assert_eq!(rig.server.player_col(1), (-1, -2));
    }

    #[test]
    fn creative_double_jump_toggles_server_authoritative_flight() {
        let mut rig = Rig::new(1, 0.0);
        let send_keys = |rig: &mut Rig, keys| {
            rig.server
                .handle_packet(
                    1,
                    ClientPacket::Input {
                        keys,
                        yaw: 0.0,
                        pitch: 0.0,
                    },
                )
                .unwrap();
        };
        let start_y = rig.server.sessions[&1].player.pos.y;

        send_keys(&mut rig, keys::JUMP);
        send_keys(&mut rig, 0);
        for _ in 0..2 {
            rig.server.tick();
        }
        send_keys(&mut rig, keys::JUMP);
        assert!(rig.server.sessions[&1].player.flying);

        for _ in 0..3 {
            rig.server.tick();
        }
        assert!(rig.server.sessions[&1].player.pos.y > start_y);

        send_keys(&mut rig, 0);
        send_keys(&mut rig, keys::JUMP);
        assert!(!rig.server.sessions[&1].player.flying);
    }

    #[test]
    fn saved_world_restores_seed_and_edits_before_column_streaming() {
        let path =
            std::env::temp_dir().join(format!("ssc-server-world-{}.bin", std::process::id()));
        let mut cfg = ServerConfig::default();
        cfg.seed = 123456;
        cfg.world_path = Some(path.clone());

        let mut original = Server::empty(cfg.clone());
        original.world.set_block(-1, 80, -17, ssc_world::BRICKS);
        original.record_block_edit(-1, 80, -17, ssc_world::BRICKS);
        original.time_of_day = 0.625;
        original.save_world();

        let mut restored = Server::empty(cfg);
        assert_eq!(restored.gen.seed(), 123456);
        assert_eq!(restored.time_of_day, 0.625);
        restored.ensure_column((-1, -2));
        assert_eq!(
            restored.world.get_block(-1, 80, -17),
            Some(ssc_world::BRICKS)
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn anonymous_player_hello_restores_inventory_by_uuid() {
        let world_path =
            std::env::temp_dir().join(format!("ssc-anonymous-world-{}.bin", std::process::id()));
        let player_id = [0x31; 16];
        let player_path = crate::persistence::player_path(&world_path, &player_id);
        let mut inventory = vec![None; crate::survival::INVENTORY_SIZE];
        inventory[2] = Some((ssc_world::APPLE, 7));
        inventory[3] = Some((ssc_world::STONE_PICKAXE, 1));
        let mut durability = vec![0; crate::survival::INVENTORY_SIZE];
        durability[3] = 18;
        let data = crate::persistence::PlayerSave {
            inventory: inventory.clone(),
            durability,
            position: Some([12.5, 70.0, -9.25]),
            health: 13.0,
            hunger: 7,
            saturation: 2.5,
            air: 250,
            selected_slot: 4,
            hunger_effect_ticks: 410,
            exhaustion: 2.5,
            respawn: Some([-20.5, 72.0, 30.5]),
            armor: vec![None; crate::survival::ARMOR_SIZE],
            armor_durability: vec![0; crate::survival::ARMOR_SIZE],
            xp_level: 7,
            xp_progress: 4,
        };
        crate::persistence::save_player(&player_path, &player_id, &data).unwrap();

        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(world_path);
        cfg.game_mode = GameMode::Survival;
        let mut server = Server::empty(cfg);
        let (_client, server_wire) = memory_pair();
        let id = server.attach(Box::new(server_wire));
        server
            .handle_packet(
                id,
                ClientPacket::Hello {
                    version: PROTOCOL_VERSION,
                    player_id,
                    name: "anonymous".into(),
                },
            )
            .unwrap();

        assert_eq!(server.sessions[&id].inventory, inventory);
        assert_eq!(server.sessions[&id].durability[3], 18);
        assert_eq!(server.sessions[&id].player_id, Some(player_id));
        assert_eq!(
            server.sessions[&id].player.pos,
            Vec3::new(12.5, 70.0, -9.25)
        );
        assert!(
            server.world.column(0, -1).is_some(),
            "collision terrain must exist before the first player tick"
        );
        assert!(
            server.world.column(1, -1).is_some(),
            "neighbor terrain must exist at chunk boundaries"
        );
        assert_eq!(
            server.sessions[&id].spawn,
            Vec3::new(-20.5, 72.0, 30.5),
            "saved respawn point overrides the world spawn"
        );
        assert_eq!(server.sessions[&id].survival.health, 13.0);
        assert_eq!(server.sessions[&id].survival.hunger, 7);
        assert_eq!(server.sessions[&id].survival.saturation, 2.5);
        assert_eq!(server.sessions[&id].survival.air, 250);
        assert_eq!(server.sessions[&id].player.selected_slot, 4);
        assert_eq!(server.sessions[&id].survival.hunger_effect_ticks, 410);
        assert_eq!(server.sessions[&id].survival.exhaustion, 2.5);
        server.sessions.get_mut(&id).unwrap().inventory[2] = Some((ssc_world::APPLE, 9));
        server.save_player_data(id);
        let saved = crate::persistence::load_player(&player_path, &player_id)
            .unwrap()
            .unwrap();
        assert_eq!(saved.inventory[2], Some((ssc_world::APPLE, 9)));
        assert_eq!(saved.durability[3], 18);
        assert_eq!(saved.hunger_effect_ticks, 410);
        assert_eq!(saved.exhaustion, 2.5);
        assert_eq!(saved.saturation, 2.5);
        let respawn_point = server.sessions[&id].spawn;
        server.sessions.get_mut(&id).unwrap().survival.die();
        server.tick(); // drop inventory at the death location before respawning
        server.sessions.get_mut(&id).unwrap().survival.respawn_at =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
        server.tick();
        assert_eq!(server.sessions[&id].player.pos, respawn_point);
        std::fs::remove_dir_all(player_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn corrupt_anonymous_player_save_is_not_overwritten() {
        let world_path = std::env::temp_dir().join(format!(
            "ssc-corrupt-player-world-{}.bin",
            std::process::id()
        ));
        let player_id = [0xd4; 16];
        let player_path = crate::persistence::player_path(&world_path, &player_id);
        let original = b"invalid player data";
        std::fs::create_dir_all(player_path.parent().unwrap()).unwrap();
        std::fs::write(&player_path, original).unwrap();

        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(world_path);
        let mut server = Server::empty(cfg);
        let (_client, server_wire) = memory_pair();
        let id = server.attach(Box::new(server_wire));
        server
            .handle_packet(
                id,
                ClientPacket::Hello {
                    version: PROTOCOL_VERSION,
                    player_id,
                    name: "anonymous".into(),
                },
            )
            .unwrap();
        server.sessions.get_mut(&id).unwrap().inventory[0] = Some((ssc_world::APPLE, 1));
        server.save_player_data(id);

        assert_eq!(std::fs::read(&player_path).unwrap(), original);
        std::fs::remove_dir_all(player_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn corrupt_entity_saves_are_not_overwritten() {
        let world_path =
            std::env::temp_dir().join(format!("ssc-corrupt-entities-{}.bin", std::process::id()));
        let item_path = crate::persistence::item_save_path(&world_path);
        let mob_path = crate::persistence::mob_save_path(&world_path);
        let bad_items = b"invalid dropped items";
        let bad_mobs = b"invalid mobs";
        std::fs::write(&item_path, bad_items).unwrap();
        std::fs::write(&mob_path, bad_mobs).unwrap();

        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(world_path);
        let mut server = Server::empty(cfg);
        server.spawn_item_entity(ssc_world::APPLE, 1, Vec3::new(8.5, 65.0, 8.5));
        server.mobs.insert(
            FIRST_MOB_ID,
            crate::mobs::Mob::new(crate::mobs::MobKind::Pig, Vec3::new(8.5, 65.0, 8.5)),
        );
        server.save_item_entities();
        server.save_mobs();

        assert_eq!(std::fs::read(&item_path).unwrap(), bad_items);
        assert_eq!(std::fs::read(&mob_path).unwrap(), bad_mobs);
        std::fs::remove_file(item_path).unwrap();
        std::fs::remove_file(mob_path).unwrap();
    }

    #[test]
    fn dropped_items_pause_without_players() {
        let mut server = Server::empty(ServerConfig::default());
        server.spawn_item_entity(ssc_world::APPLE, 1, Vec3::new(8.5, 65.0, 8.5));
        let item_id = server.next_item_id - 1;
        {
            let item = server.items.get_mut(&item_id).unwrap();
            item.age = ITEM_LIFETIME_TICKS - 1;
            item.pickup_delay = 1;
        }
        let position = server.items[&item_id].pos;

        server.tick_items(0.05);

        let item = &server.items[&item_id];
        assert_eq!(item.age, ITEM_LIFETIME_TICKS - 1);
        assert_eq!(item.pickup_delay, 1);
        assert_eq!(item.pos, position);
    }

    #[test]
    fn distant_items_pause_until_a_player_approaches() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let far = Vec3::new(200.5, 65.0, 8.5);
        rig.server.spawn_item_entity(ssc_world::APPLE, 1, far);
        let id = rig.server.next_item_id - 1;
        {
            let item = rig.server.items.get_mut(&id).unwrap();
            item.age = 100;
            item.pickup_delay = 5;
        }
        rig.server.tick_items(0.05);
        assert_eq!(rig.server.items[&id].age, 100);
        assert_eq!(rig.server.items[&id].pickup_delay, 5);
        assert_eq!(rig.server.items[&id].pos, far);

        rig.server.sessions.get_mut(&1).unwrap().player.pos = far + Vec3::new(3.0, 0.0, 0.0);
        rig.server.tick_items(0.05);
        assert_eq!(rig.server.items[&id].age, 101);
        assert_eq!(rig.server.items[&id].pickup_delay, 4);
    }

    #[test]
    fn dead_player_save_reconnects_as_respawned() {
        let world_path =
            std::env::temp_dir().join(format!("ssc-dead-player-world-{}.bin", std::process::id()));
        let player_id = [0xa7; 16];
        let player_path = crate::persistence::player_path(&world_path, &player_id);
        crate::persistence::save_player(
            &player_path,
            &player_id,
            &crate::persistence::PlayerSave {
                inventory: vec![None; crate::survival::INVENTORY_SIZE],
                durability: vec![0; crate::survival::INVENTORY_SIZE],
                position: Some([4.5, 70.0, -2.5]),
                health: 0.0,
                hunger: 0,
                saturation: 0.0,
                air: 0,
                selected_slot: 0,
                hunger_effect_ticks: 0,
                exhaustion: 0.0,
                respawn: Some([3.5, 66.0, 4.5]),
                armor: vec![None; crate::survival::ARMOR_SIZE],
                armor_durability: vec![0; crate::survival::ARMOR_SIZE],
                xp_level: 0,
                xp_progress: 0,
            },
        )
        .unwrap();

        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(world_path);
        cfg.game_mode = GameMode::Survival;
        let mut server = Server::empty(cfg);
        let (_client, server_wire) = memory_pair();
        let id = server.attach(Box::new(server_wire));
        server
            .handle_packet(
                id,
                ClientPacket::Hello {
                    version: PROTOCOL_VERSION,
                    player_id,
                    name: "respawning".into(),
                },
            )
            .unwrap();

        let session = &server.sessions[&id];
        assert!(!session.survival.dead);
        assert_eq!(session.survival.health, crate::survival::MAX_HEALTH);
        assert_eq!(session.survival.hunger, crate::survival::MAX_HUNGER);
        assert_eq!(session.survival.air, crate::survival::MAX_AIR);
        assert_eq!(session.player.pos, Vec3::new(3.5, 66.0, 4.5));
        assert_eq!(session.player.pos, session.spawn);
        std::fs::remove_dir_all(player_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn disconnecting_in_the_void_saves_safe_respawn_position() {
        let world_path =
            std::env::temp_dir().join(format!("ssc-void-player-world-{}.bin", std::process::id()));
        let player_id = [0xb8; 16];
        let player_path = crate::persistence::player_path(&world_path, &player_id);
        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(world_path);
        cfg.game_mode = GameMode::Survival;
        let mut server = Server::empty(cfg);
        let (_client, server_wire) = memory_pair();
        let id = server.attach(Box::new(server_wire));
        server
            .handle_packet(
                id,
                ClientPacket::Hello {
                    version: PROTOCOL_VERSION,
                    player_id,
                    name: "falling".into(),
                },
            )
            .unwrap();
        let spawn = server.sessions[&id].spawn;
        server.sessions.get_mut(&id).unwrap().player.pos.y = ssc_world::MIN_Y as f32 - 1.0;
        server.sessions.get_mut(&id).unwrap().survival.health = 8.0;
        server.save_player_data(id);

        let saved = crate::persistence::load_player(&player_path, &player_id)
            .unwrap()
            .unwrap();
        assert_eq!(saved.position, Some([spawn.x, spawn.y, spawn.z]));
        assert_eq!(saved.health, 8.0);
        std::fs::remove_dir_all(player_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn dropped_items_restore_from_world_save() {
        let world_path =
            std::env::temp_dir().join(format!("ssc-item-world-{}.bin", std::process::id()));
        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(world_path.clone());
        let mut server = Server::empty(cfg.clone());
        server.spawn_item_entity(ssc_world::PORKCHOP, 2, Vec3::new(-12.5, 65.0, 7.25));
        server.items.values_mut().next().unwrap().vel = Vec3::new(0.5, -1.0, 0.25);
        server.items.values_mut().next().unwrap().age = 123;
        server.items.values_mut().next().unwrap().pickup_delay = 4;
        server.save_item_entities();

        let restored = Server::empty(cfg);
        let item = restored.items.values().next().unwrap();
        assert_eq!(restored.items.len(), 1);
        assert_eq!(item.item, ssc_world::PORKCHOP);
        assert_eq!(item.count, 2);
        assert_eq!(item.pos, Vec3::new(-12.5, 65.0, 7.25));
        assert_eq!(item.vel, Vec3::new(0.5, -1.0, 0.25));
        assert_eq!(item.age, 123);
        assert_eq!(item.pickup_delay, 4);
        std::fs::remove_file(crate::persistence::item_save_path(&world_path)).unwrap();
    }

    #[test]
    fn saved_mobs_load_with_terrain_and_pause_without_players() {
        let world_path =
            std::env::temp_dir().join(format!("ssc-mob-world-{}.bin", std::process::id()));
        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(world_path.clone());
        let mut server = Server::empty(cfg.clone());
        let mut mob =
            crate::mobs::Mob::new(crate::mobs::MobKind::Zombie, Vec3::new(8.5, 49.0, 8.5));
        mob.health = 12.0;
        mob.attack_cooldown = 7;
        let id = server.allocate_mob_id().unwrap();
        server.mobs.insert(id, mob);
        server.world.set_block(8, 48, 8, ssc_world::STONE);
        server.record_block_edit(8, 48, 8, ssc_world::STONE);
        server.save_world();
        server.save_mobs();

        let mut restored = Server::empty(cfg);
        let restored_mob = restored.mobs.values().next().unwrap();
        let position = restored_mob.pos;
        assert_eq!(restored_mob.kind, crate::mobs::MobKind::Zombie);
        assert_eq!(restored_mob.health, 12.0);
        assert_eq!(restored_mob.attack_cooldown, 7);
        assert_eq!(
            restored.world.block_or_air(8, 48, 8),
            ssc_world::STONE,
            "mob's surrounding terrain is available before simulation"
        );

        restored.tick();

        assert_eq!(restored.mobs.len(), 1);
        assert_eq!(restored.mobs.values().next().unwrap().pos, position);
        std::fs::remove_file(&world_path).unwrap();
        std::fs::remove_file(crate::persistence::mob_save_path(&world_path)).unwrap();
    }

    #[test]
    fn fleeing_pig_restores_its_timer_and_pauses_while_offline() {
        let world_path =
            std::env::temp_dir().join(format!("ssc-fleeing-pig-world-{}.bin", std::process::id()));
        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(world_path.clone());
        let mut server = Server::empty(cfg.clone());
        let pos = Vec3::new(8.5, 65.0, 8.5);
        let mut pig = crate::mobs::Mob::new(crate::mobs::MobKind::Pig, pos);
        pig.hurt(1.0, pos - Vec3::new(2.0, 0.0, 0.0));
        pig.panic_timer = 37;
        let id = server.allocate_mob_id().unwrap();
        server.mobs.insert(id, pig);
        server.save_mobs();
        let mut restored = Server::empty(cfg);
        restored.tick();
        let pig = restored.mobs.values().next().unwrap();
        assert_eq!(pig.panic_timer, 37);
        assert_eq!(pig.pos, pos);
        assert_eq!(pig.wander_dir, Some(ssc_math::Vec2::X));
        std::fs::remove_file(crate::persistence::mob_save_path(&world_path)).unwrap();
    }

    #[test]
    fn simultaneous_connections_cannot_claim_the_same_anonymous_id() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        let player_id = [0x42; 16];
        rig.server.sessions.get_mut(&1).unwrap().player.joined = false;
        rig.server.sessions.get_mut(&2).unwrap().player.joined = false;
        for id in [1, 2] {
            rig.server
                .handle_packet(
                    id,
                    ClientPacket::Hello {
                        version: PROTOCOL_VERSION,
                        player_id,
                        name: "duplicate".into(),
                    },
                )
                .unwrap();
        }
        assert!(rig.server.sessions[&1].player.joined);
        assert!(!rig.server.sessions[&2].player.joined);
        assert!(rig
            .drain(0)
            .iter()
            .all(|packet| !matches!(packet, ServerPacket::Goodbye { .. })));
        assert!(rig
            .drain(1)
            .iter()
            .any(|packet| matches!(packet, ServerPacket::Goodbye { .. })));
    }

    #[test]
    fn corrupt_world_save_is_not_overwritten() {
        let path = std::env::temp_dir().join(format!(
            "ssc-server-world-corrupt-{}.bin",
            std::process::id()
        ));
        let original = b"not a valid world save";
        std::fs::write(&path, original).unwrap();

        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(path.clone());
        let server = Server::empty(cfg);
        server.save_world();

        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn place_inside_any_player_is_rejected() {
        // Player 2 stands at (9.5, 55, 8.5); player 1's -1.2 aim places
        // into cell (9, 55, 8) — overlapping player 2 — so the placement
        // must be rejected with no update packets.
        let mut rig = Rig::new(2, -1.2);
        rig.server.sessions.get_mut(&2).unwrap().player.pos = Vec3::new(9.5, 55.02, 8.5);
        assert!(
            ssc_math::Aabb::block(9, 55, 8).intersects(&player_aabb(Vec3::new(9.5, 55.02, 8.5))),
            "test setup: player 2 must occupy the target cell"
        );

        rig.server.handle_use(1);
        let packets = rig.drain(0);
        assert!(
            packets.is_empty(),
            "placement inside a player must be rejected: {packets:?}"
        );
    }
}
