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
    look_dir, yaw_forward, yaw_right, GRAVITY, JUMP_VELOCITY, PLAYER_EYE_HEIGHT, REACH, SNEAK_SPEED,
    SPRINT_SPEED, TERMINAL_VELOCITY, WALK_SPEED, Vec3,
};
use ssc_protocol::packets::{keys, ClientPacket, ServerPacket, SoundId};
use ssc_protocol::wire::{memory_pair, MemoryWire, Wire};
use ssc_protocol::PROTOCOL_VERSION;
use ssc_world::{is_solid, BlockId, ColumnKey, World, AIR, SAND, SANDSTONE, SNOW_BLOCK, WATER};

use crate::physics::{move_player, player_aabb, entity_aabb};
use crate::raycast::raycast;
use crate::worldgen::WorldGen;

/// Mobs occupy ids from here upward (players are 1..).
const FIRST_MOB_ID: EntityId = 10_000;
/// Dropped-item ids occupy a separate range when possible.
const FIRST_ITEM_ID: EntityId = 40_000;
/// Maximum mobs within a player's spawn range (per player).
const MOB_CAP_PER_PLAYER: usize = 6;
const ITEM_LIFETIME_TICKS: u32 = 6_000;

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
    /// Survival-mode state (ignored in creative).
    survival: crate::survival::SurvivalState,
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

/// One dropped stack. Gameplay and pickup are entirely server-owned.
struct ItemEntity {
    item: ssc_world::ItemId,
    count: u8,
    pos: Vec3,
    vel: Vec3,
    age: u32,
    pickup_delay: u8,
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
            },
            inventory,
            survival: crate::survival::SurvivalState::default(),
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
    sessions: HashMap<EntityId, Session>,
    /// Mobs (ids from `FIRST_MOB_ID`).
    mobs: HashMap<EntityId, crate::mobs::Mob>,
    items: HashMap<EntityId, ItemEntity>,
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
            Some(path) => match crate::persistence::load_items(
                &crate::persistence::item_save_path(path),
            ) {
                Ok(items) => (items.unwrap_or_default(), true),
                Err(err) => {
                    eprintln!(
                        "[server] dropped-item save could not be loaded: {err}; preserving it"
                    );
                    (Vec::new(), false)
                }
            },
            None => (Vec::new(), false),
        };
        let (saved_mobs, mob_persistence_ready) = match cfg.world_path.as_deref() {
            Some(path) => match crate::persistence::load_mobs(
                &crate::persistence::mob_save_path(path),
            ) {
                Ok(mobs) => (mobs.unwrap_or_default(), true),
                Err(err) => {
                    eprintln!("[server] mob save could not be loaded: {err}; preserving it");
                    (Vec::new(), false)
                }
            },
            None => (Vec::new(), false),
        };
        let edited = block_edits.keys().copied().collect();
        let mut server = Self {
            gen: WorldGen::new(seed),
            world: World::default(),
            edited,
            block_edits,
            persistence_ready,
            item_persistence_ready,
            mob_persistence_ready,
            cfg,
            sessions: HashMap::new(),
            mobs: HashMap::new(),
            items: HashMap::new(),
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
        self.sessions.insert(id, Session::new(wire, self.cfg.game_mode));
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
        let Some(path) = self.cfg.world_path.as_deref() else { return };
        if let Err(err) = crate::persistence::save(
            path,
            self.gen.seed(),
            self.time_of_day,
            &self.block_edits,
        ) {
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
                pos: [item.pos.x, item.pos.y, item.pos.z],
                vel: [item.vel.x, item.vel.y, item.vel.z],
                age: item.age,
                pickup_delay: item.pickup_delay,
            })
            .collect();
        let path = crate::persistence::item_save_path(world_path);
        if let Err(err) = crate::persistence::save_items(&path, &items) {
            eprintln!("[server] dropped-item save to {} failed: {err}", path.display());
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
                on_ground: mob.on_ground_hint,
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
            (
                Some([
                    session.player.pos.x,
                    session.player.pos.y,
                    session.player.pos.z,
                ]),
                session.survival.health,
                session.survival.hunger,
                session.survival.air,
            )
        };
        let data = crate::persistence::PlayerSave {
            inventory: session.inventory.clone(),
            position,
            health,
            hunger,
            air,
            selected_slot: session.player.selected_slot,
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

    fn record_block_edit(&mut self, x: i32, y: i32, z: i32, block: BlockId) {
        let key = (x.div_euclid(16), z.div_euclid(16));
        let local = (x.rem_euclid(16), y, z.rem_euclid(16));
        self.block_edits.entry(key).or_default().insert(local, block);
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
        self.save_world();
        let _ = self.broadcast(&ServerPacket::Goodbye { reason: "server closed".into() });
    }

    fn tick(&mut self) {
        self.tick += 1;
        let dt = 1.0 / TICK_RATE;

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

        for (id, session) in self.sessions.iter_mut() {
            let p = &mut session.player;
            if !p.joined {
                continue;
            }
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
            let old_y = p.pos.y;
            let jumped = p.keys & keys::JUMP != 0 && p.on_ground;
            let flying = p.flying;
            step_player(&self.world, p, dt, flying);
            let moving = p.keys & (keys::FORWARD | keys::BACKWARD | keys::LEFT | keys::RIGHT) != 0;
            let sprinting = p.keys & keys::SPRINT != 0 && moving;

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
                    landed: !was_on_ground && p.on_ground,
                    in_water: p.in_water,
                    eye_in_water,
                    moving,
                    sprinting,
                    jumped,
                    dy: p.pos.y - old_y,
                })
            } else {
                Default::default()
            };
            if fx_tick.health_changed || fx_tick.hunger_changed || fx_tick.air_changed {
                session.status_dirty = true;
            }

            // Dig progress (survival): the target must stay in reach and
            // remain the block we started on; raycast each tick.
            let mut dig_broadcast = None;
            let mut broke_block = None;
            if let Some(dig) = &mut session.survival.digging {
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
                            if crate::survival::dig_time(
                                self.world.block_or_air(hit.block[0], hit.block[1], hit.block[2]),
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
                    let time = crate::survival::dig_time(block);
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
                self.send_sound(SoundId::Hurt, x.floor() as i32, y.floor() as i32, z.floor() as i32);
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
                let moving = p.keys
                    & (keys::FORWARD | keys::BACKWARD | keys::LEFT | keys::RIGHT)
                    != 0;
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
                        (-1..=1).flat_map(move |dx| {
                            (-1..=1).map(move |dz| (cx + dx, cz + dz))
                        })
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
                let Some((player_id, _)) = players.get(idx) else { continue };
                if self.cfg.game_mode != GameMode::Survival {
                    continue;
                }
                let damage = self.mobs[&mob_id].kind.attack_damage();
                let mut fx = crate::survival::TickEffects::default();
                let session = self.sessions.get_mut(player_id).unwrap();
                session.survival.apply_damage(damage, &mut fx);
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
                let (kind, pos) = {
                    let m = &self.mobs[&mob_id];
                    (m.kind, m.pos)
                };
                let drop_roll = hash_tick(self.tick, mob_id as i32, pos.x.floor() as i32);
                let drop = crate::mobs::drop_for(kind, drop_roll);
                if let Some((item, count)) = drop {
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
                                .all(|p| (m.pos - *p).length() > 64.0))
                })
                .map(|(id, _)| *id)
                .collect();
            for id in despawn {
                self.mobs.remove(&id);
                let _ = self.broadcast(&ServerPacket::EntityRemove { id });
            }
        }

        if self.cfg.game_mode == GameMode::Survival {
            let dead_players: Vec<(EntityId, Vec3, Vec<(ssc_world::ItemId, u8)>)> = self
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
                    let drops = session.inventory.iter_mut().filter_map(Option::take).collect();
                    (*id, session.player.pos, drops)
                })
                .collect();
            for (_id, pos, drops) in dead_players {
                for (item, count) in drops {
                    self.spawn_item_entity(item, count, pos);
                }
            }
        }

        self.tick_items(dt);

        // --- Dirty syncs (inventory / status).
        let dirty: Vec<EntityId> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.player.joined && (s.inv_dirty || s.status_dirty))
            .map(|(id, _)| *id)
            .collect();
        for id in dirty {
            let (inv_pkt, status_pkt) = {
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
                let status_pkt = if session.status_dirty {
                    session.status_dirty = false;
                    Some(ServerPacket::Status {
                        health: session.survival.health,
                        hunger: session.survival.hunger,
                        air: session.survival.air,
                    })
                } else {
                    None
                };
                (inv_pkt, status_pkt)
            };
            if let Some(pkt) = inv_pkt {
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

        // Time advances every tick; broadcast once a second.
        self.time_of_day += 1.0 / 24000.0;
        if self.time_of_day >= 1.0 {
            self.time_of_day -= 1.0;
        }
        if self.tick % 20 == 0 {
            let _ = self.broadcast(&ServerPacket::TimeOfDay { t: self.time_of_day });
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
            self.save_world();
        }
    }

    fn spawn_item_entity(&mut self, item: ssc_world::ItemId, count: u8, pos: Vec3) {
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
            item.age += 1;
            item.pickup_delay = item.pickup_delay.saturating_sub(1);
            if item.age >= ITEM_LIFETIME_TICKS || item.pos.y <= ssc_world::MIN_Y as f32 {
                expired.push(*id);
                continue;
            }
            if let Some(target) = players
                .iter()
                .filter_map(|player| {
                    let delta = *player - item.pos;
                    let distance = delta.length();
                    (distance < 8.0 && distance > 0.001).then_some((delta, distance))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(delta, distance)| delta / distance)
            {
                // Gentle server-authoritative homing toward nearby players.
                item.vel += target * (8.0 * dt);
                let speed = item.vel.length();
                if speed > 12.0 {
                    item.vel = item.vel * (12.0 / speed);
                }
            }
            item.vel.y = (item.vel.y - GRAVITY * dt).max(-TERMINAL_VELOCITY);
            let result = crate::physics::move_entity(
                &self.world,
                &mut item.pos,
                &mut item.vel,
                dt,
                0.125,
                0.25,
            );
            if result.on_ground {
                item.vel.y = 0.0;
            }
            item.vel.x *= 0.8;
            item.vel.z *= 0.8;
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
            let Some(item) = self.items.get(&item_id) else { continue };
            let (item_value, mut count, pos) = (item.item, item.count, item.pos);
            if item.pickup_delay > 0 {
                continue;
            }
            let mut candidates: Vec<(EntityId, f32)> = self
                .sessions
                .iter()
                .filter(|(_, s)| s.player.joined && !s.survival.dead)
                .map(|(id, s)| (*id, (s.player.pos - pos).length()))
                .filter(|(_, distance)| *distance <= 1.25)
                .collect();
            candidates.sort_by(|a, b| a.1.total_cmp(&b.1));
            for (player_id, _) in candidates {
                let Some(session) = self.sessions.get_mut(&player_id) else { continue };
                let leftover =
                    crate::survival::add_item(&mut session.inventory, item_value, count);
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
            let Some(a) = self.items.get(&id) else { continue };
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

    /// Mob spawning: a few pigs near each player during the day, zombies
    /// at night. Simple, deterministic-ish, capped.
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

        // Pigs by day (and rarely at night), zombies at night.
        let want_zombie = night;
        let want_pig = !night;
        for (pcx, pcz) in player_cols {
            if self.mobs.len() >= mob_cap {
                break;
            }
            for attempt in 0..4 {
                // A random-ish column in a 3-radius ring (8..24 blocks).
                let ax = (pcx * 16 + 8 + (hash_tick(self.tick + attempt, pcx, pcz) % 32) as i32)
                    - 16;
                let az = (pcz * 16 + 8
                    + (hash_tick(self.tick + attempt * 7, pcz, pcx) % 32) as i32)
                    - 16;
                let Some(y) = crate::mobs::spawn_column_ok(&self.world, ax, az) else {
                    continue;
                };
                // Column must exist server-side (spawn only in streamed
                // areas so clients see the mob's ground).
                if self.world.column(ax.div_euclid(16), az.div_euclid(16)).is_none() {
                    continue;
                }
                let (kind, want) = if want_zombie {
                    (crate::mobs::MobKind::Zombie, want_zombie)
                } else {
                    (crate::mobs::MobKind::Pig, want_pig)
                };
                if !want {
                    continue;
                }
                // No spawning right on top of players.
                let pos = Vec3::new(ax as f32 + 0.5, y as f32 + 0.02, az as f32 + 0.5);
                let too_close = self.sessions.values().any(|s| {
                    s.player.joined && (s.player.pos - pos).length() < 12.0
                });
                if too_close {
                    continue;
                }
                let Some(id) = self.allocate_mob_id() else {
                    return;
                };
                self.mobs.insert(id, crate::mobs::Mob::new(kind, pos));
                break; // at most one mob per player per second
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
                if t <= REACH && best.map(|(_, bt, _)| t < bt).unwrap_or(true) {
                    best = Some((eid, t, is_mob));
                }
            }
        }
        let Some((target, t, is_mob)) = best else { return false };
        // The nearest entity only takes priority when no solid voxel is
        // between its hitbox and the attacker's eyes.
        if raycast(&self.world, eye, dir, (t - 0.0001).max(0.0)).is_some() {
            return false;
        }

        if is_mob {
            // Punch a mob: 2 damage by hand, knockback.
            let from = self.sessions[&id].player.pos;
            if let Some(mob) = self.mobs.get_mut(&target) {
                mob.hurt(2.0, from);
            }
            let pos = self.mobs.get(&target).map(|m| m.pos);
            if let Some(p) = pos {
                self.send_sound(SoundId::Hurt, p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32);
            }
            // Death and entity removal are handled together during the
            // authoritative mob tick so clients don't receive remove/state/
            // remove in consecutive packets.
            true
        } else {
            // PvP: 2 damage (hand) + knockback via velocity.
            let from = self.sessions[&id].player.pos;
            let session = self.sessions.get_mut(&target).unwrap();
            if self.cfg.game_mode == GameMode::Survival {
                let delta = (session.player.pos - from).normalize_or_zero();
                session.player.vel.x += delta.x * 6.0;
                session.player.vel.z += delta.z * 6.0;
                session.player.vel.y = (session.player.vel.y + 4.0).min(6.0);
                let mut fx = crate::survival::TickEffects::default();
                session.survival.apply_damage(2.0, &mut fx);
                session.status_dirty = true;
            }
            let p = self.sessions[&target].player.pos;
            self.send_sound(SoundId::Hurt, p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32);
            true
        }
    }
}

// ---------------------------------------------------------------------------
// Player simulation
// ---------------------------------------------------------------------------

/// Simulate one player for one tick (server-authoritative).
fn step_player(world: &World, player: &mut Player, dt: f32, flying: bool) {
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
        || world
            .block_or_air(eye.x.floor() as i32, eye.y.floor() as i32, eye.z.floor() as i32)
            == WATER;

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
    let sprint = k & keys::SPRINT != 0 && k & keys::FORWARD != 0 && k & keys::SNEAK == 0;
    let sneak = k & keys::SNEAK != 0;
    let speed = if flying {
        if sprint { 21.78 } else { 10.89 }
    } else if in_water {
        WALK_SPEED * 0.55 // swimming is slow
    } else if sprint {
        SPRINT_SPEED
    } else if sneak {
        SNEAK_SPEED
    } else {
        WALK_SPEED
    };
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
    let res = move_player(world, &mut pos, &mut vel, dt);
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
                let Some(session) = self.sessions.get_mut(&id) else { return };
                match session.stream_queue.pop_front() {
                    Some(k) => k,
                    None => break,
                }
            };
            self.stream_column(id, key);
        }
        for _ in 0..max_lods {
            let key = {
                let Some(session) = self.sessions.get_mut(&id) else { return };
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
        let Some(session) = self.sessions.get_mut(&id) else { return };
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
            let _ = session
                .wire
                .send(&ServerPacket::UnloadColumn { cx: key.0, cz: key.1 }.encode());
        }
        let lod_out: Vec<ColumnKey> = session
            .lod_streamed
            .iter()
            .filter(|(cx, cz)| (cx - pcx).abs() > ld + 1 || (cz - pcz).abs() > ld + 1)
            .copied()
            .collect();
        for key in lod_out {
            session.lod_streamed.remove(&key);
            let _ = session
                .wire
                .send(&ServerPacket::UnloadLod { cx: key.0, cz: key.1 }.encode());
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
        let Some(session) = self.sessions.get_mut(&id) else { return };
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
        let _ = session
            .wire
            .send(&ServerPacket::ColumnEnd { cx: key.0, cz: key.1 }.encode());
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
        let Some(session) = self.sessions.get_mut(&id) else { return };
        let _ = session
            .wire
            .send(&ServerPacket::LodColumn { cx: key.0, cz: key.1, cells }.encode());
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
                let Some(session) = self.sessions.get_mut(&id) else { return Ok(()) };
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
                let Some(session) = self.sessions.get_mut(&id) else { return Ok(()) };
                session.player.selected_slot = slot % 9;
            }
            ClientPacket::SetHotbarSlot { slot, block } => {
                // Creative only: validate against the registry, write the
                // item into the hotbar slot (infinite stack).
                if !ssc_world::valid_item_id(block) {
                    return Err(format!("invalid block id {block}"));
                }
                let Some(session) = self.sessions.get_mut(&id) else { return Ok(()) };
                if !session.player.joined || self.cfg.game_mode != GameMode::Creative {
                    return Ok(());
                }
                let idx = (slot as usize) % 9;
                session.inventory[idx] = Some((block, 64));
                session.inv_dirty = true;
            }
            ClientPacket::SwapSlots { a, b } => {
                // Own-inventory item movement (survival inventory UI).
                let Some(session) = self.sessions.get_mut(&id) else { return Ok(()) };
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
                    match (session.inventory[a], session.inventory[b]) {
                        (Some((item, source_count)), Some((target_item, target_count)))
                            if item == target_item =>
                        {
                            let capacity =
                                ssc_world::item_info(item).max_stack.saturating_sub(target_count);
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
                            session.inv_dirty = true;
                        }
                    }
                }
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
                let Some(session) = self.sessions.get_mut(&id) else { return Ok(()) };
                if let Some(dig) = session.survival.digging.take() {
                    let _ = self.broadcast(&ServerPacket::DigProgress {
                        x: dig.x,
                        y: dig.y,
                        z: dig.z,
                        progress: 101,
                    });
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
                    reason: format!("protocol mismatch: client {version}, server {PROTOCOL_VERSION}"),
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
        let Some(session) = self.sessions.get_mut(&id) else { return };
        // Death is an in-session timer, not a persistent state. If the player
        // quit after dying, reconnect them through the same fresh respawn path
        // rather than reviving at the saved death position with zero health.
        let saved_death = saved_player
            .as_ref()
            .is_some_and(|data| data.health <= 0.0);
        let restored_position = saved_player
            .as_ref()
            .filter(|_| !saved_death)
            .and_then(|data| data.position);
        if let Some(data) = saved_player {
            session.inventory = data.inventory;
            if saved_death {
                session.survival = crate::survival::SurvivalState::default();
            } else {
                session.survival.health = data.health;
                session.survival.hunger = data.hunger;
                session.survival.air = data.air;
            }
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
        // A saved logout position is not the world's respawn point.
        session.spawn = Vec3::new(spawn_x, spawn_y, spawn_z);
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
        let palette: Vec<BlockId> = (1..ssc_world::BLOCK_COUNT).collect();
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
            self.break_block(id, x, y, z);
        } else {
            let block = self.world.block_or_air(x, y, z);
            if crate::survival::dig_time(block).is_some() {
                if let Some(session) = self.sessions.get_mut(&id) {
                    session.survival.digging =
                        Some(crate::survival::DigState { x, y, z, progress: 0.0 });
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
        let _ = self.broadcast(&ServerPacket::BlockUpdate { x, y, z, block: AIR });
        self.send_sound(SoundId::Dig, x, y, z);
        // LOD must reflect the edit (server-driven LOD, specs.md §3).
        self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));

        // Survival: drop the item into the inventory (no item entities
        // yet — direct pickup, noted in implemented-features.md).
        if self.cfg.game_mode == GameMode::Survival {
            let rng = (hash_tick(self.tick, x, z) & 0xFFFF) as f32 / 65535.0;
            if let Some(item) = crate::survival::drop_for(block, rng) {
                let leftover = {
                    let Some(session) = self.sessions.get_mut(&id) else { return };
                    session.survival.exhaustion += 0.005; // mining costs hunger
                    let inv = &mut session.inventory;
                    crate::survival::add_item(inv, item, 1)
                };
                if leftover == 0 {
                    if let Some(session) = self.sessions.get_mut(&id) {
                        session.inv_dirty = true;
                    }
                } else {
                    self.spawn_item_entity(
                        item,
                        leftover,
                        Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5),
                    );
                }
            }
        }
    }

    /// Right-click use: eat the selected food, or place the selected
    /// block item (survival consumes, creative does not).
    fn handle_use(&mut self, id: EntityId) {
        let Some((item, count)) = self.selected_item(id) else { return };
        let item_info = ssc_world::item_info(item);

        // Eating (survival only — hunger doesn't exist in creative).
        if let Some(food) = item_info.food {
            if self.cfg.game_mode == GameMode::Survival {
                let slot = self.sessions[&id].player.selected_slot as usize % 9;
                let rotten_flesh_causes_hunger = item == ssc_world::ROTTEN_FLESH
                    && hash_tick(self.tick, id as i32, slot as i32) % 5 != 0;
                let ate = {
                    let Some(session) = self.sessions.get_mut(&id) else { return };
                    let s = &mut session.survival;
                    let ate = s.eat(food);
                    if ate && rotten_flesh_causes_hunger {
                        s.apply_rotten_flesh_effect();
                    }
                    ate
                };
                if ate {
                    // Consume one.
                    if let Some(session) = self.sessions.get_mut(&id) {
                        if let Some((i, c)) = session.inventory[slot] {
                            session.inventory[slot] =
                                if c <= 1 { None } else { Some((i, c - 1)) };
                        }
                        session.inv_dirty = true;
                        session.status_dirty = true;
                    }
                    let pos = self.sessions[&id].player.pos;
                    self.send_sound(
                        SoundId::Eat,
                        pos.x.floor() as i32,
                        pos.y.floor() as i32,
                        pos.z.floor() as i32,
                    );
                }
            }
            return;
        }

        // Placing.
        let Some(block) = item_info.places else { return };
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
        // Never place inside any player.
        for session in self.sessions.values() {
            if session.player.joined
                && !session.survival.dead
                && player_aabb(session.player.pos)
                    .intersects(&ssc_math::Aabb::block(x, y, z))
            {
                return;
            }
        }

        if self.world.set_block(x, y, z, block) {
            self.record_block_edit(x, y, z, block);
            let _ = self.broadcast(&ServerPacket::BlockUpdate { x, y, z, block });
            self.send_sound(SoundId::Place, x, y, z);
            self.refresh_lod_for((x.div_euclid(16), z.div_euclid(16)));
            // Survival: consume one item.
            if self.cfg.game_mode == GameMode::Survival && count > 0 {
                let slot = self.sessions[&id].player.selected_slot as usize % 9;
                if let Some(session) = self.sessions.get_mut(&id) {
                    if let Some((i, c)) = session.inventory[slot] {
                        session.inventory[slot] =
                            if c <= 1 { None } else { Some((i, c - 1)) };
                    }
                    session.inv_dirty = true;
                }
            }
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
        let _ = self.broadcast(&ServerPacket::LodColumn { cx: key.0, cz: key.1, cells });
        for session in self.sessions.values_mut() {
            session.lod_streamed.insert(key);
        }
    }

    // -----------------------------------------------------------------------
    // Wire helpers
    // -----------------------------------------------------------------------

    fn direct(&mut self, id: EntityId, pkt: &ServerPacket) -> Result<(), String> {
        let Some(session) = self.sessions.get_mut(&id) else { return Ok(()) };
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

        /// `n` players at `(8.5 + id*4, 55, 8.5)` looking with `pitch`.
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
                session.player.pos = Vec3::new(8.5 + i as f32 * 4.0, 55.02, 8.5);
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
        assert_eq!(updates, 1, "expected exactly one BlockUpdate(AIR), got {packets:?}");
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
                    ServerPacket::BlockUpdate { block: ssc_world::BRICKS, .. }
                )
            })
            .count();
        let sounds = packets
            .iter()
            .filter(|p| matches!(p, ServerPacket::Sound { .. }))
            .count();
        assert_eq!(updates, 1, "expected exactly one BlockUpdate(BRICKS), got {packets:?}");
        assert_eq!(sounds, 1, "expected exactly one place Sound, got {packets:?}");
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
    fn survival_dig_takes_time_and_drops_into_inventory() {
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
        // The cobblestone drop landed in the inventory (sync packet).
        assert!(packets
            .iter()
            .any(|p| matches!(p, ServerPacket::Inventory { slots } if slots.iter().any(|(i, c)| *i == ssc_world::COBBLESTONE && *c == 1))));
        assert_eq!(
            rig.server.sessions[&1].inventory.iter().flatten().next(),
            Some(&(ssc_world::COBBLESTONE, 1))
        );
    }

    #[test]
    fn survival_dig_cancel_resets_progress() {
        let mut rig = Rig::new_mode(1, -1.5, GameMode::Survival);
        rig.server.handle_dig_start(1);
        for _ in 0..60 {
            rig.server.tick();
        }
        let _ = rig
            .server
            .handle_packet(1, ClientPacket::DigStop)
            .unwrap();
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
        assert_eq!(rig.server.sessions[&1].inventory[0], Some((ssc_world::BRICKS, 1)));
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
        let session = &rig.server.sessions[&1];
        assert_eq!(session.survival.hunger, 14, "apple restores 4 hunger");
        assert_eq!(session.inventory[0], Some((ssc_world::APPLE, 2)));
        // Full hunger: no further eating.
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.survival.hunger = 20;
        }
        rig.server.handle_use(1);
        assert_eq!(rig.server.sessions[&1].inventory[0], Some((ssc_world::APPLE, 2)));
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
        let session = &rig.server.sessions[&1];
        assert_eq!(session.survival.hunger, 13);
        assert_eq!(session.inventory[0], None);
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
    fn dropped_items_are_attracted_toward_nearby_players() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        let player_pos = rig.server.sessions[&1].player.pos;
        rig.server.spawn_item_entity(
            ssc_world::APPLE,
            1,
            player_pos + Vec3::new(4.0, 0.0, 0.0),
        );
        let item = rig.server.items.values_mut().next().unwrap();
        item.vel = Vec3::ZERO;
        item.pickup_delay = 100;

        rig.server.tick_items(0.05);

        assert!(
            rig.server.items.values().next().unwrap().vel.x < 0.0,
            "item velocity should point toward the player"
        );
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
        assert!(rig.server.sessions[&1].inventory.iter().all(Option::is_none));
    }

    #[test]
    fn block_break_overflow_becomes_a_world_item() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server.sessions.get_mut(&1).unwrap().inventory.fill(Some((
            ssc_world::STONE,
            64,
        )));
        rig.server.world.set_block(8, 54, 8, ssc_world::GRASS_BLOCK);

        rig.server.break_block(1, 8, 54, 8);

        assert!(rig.server.items.values().any(|item| {
            item.item == ssc_world::DIRT && item.count == 1
        }));
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
        rig.server.sessions.get_mut(&2).unwrap().player.pos =
            pos + Vec3::new(0.5, 0.0, 0.0);
        rig.server.spawn_item_entity(ssc_world::APPLE, 2, pos);
        let item_id = rig.server.next_item_id - 1;
        rig.server.items.get_mut(&item_id).unwrap().pickup_delay = 0;

        rig.server.tick_items(0.05);

        assert_eq!(rig.server.sessions[&1].inventory[0], Some((ssc_world::APPLE, 64)));
        assert!(rig.server.sessions[&1].inv_dirty);
        assert_eq!(rig.server.sessions[&2].inventory[0], Some((ssc_world::APPLE, 1)));
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
        rig.server.sessions.get_mut(&2).unwrap().player.pos =
            pos + Vec3::new(0.5, 0.0, 0.0);
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
        let session = &rig.server.sessions[&1];
        assert_eq!(session.survival.hunger, 14);
        assert_eq!(session.inventory[0], Some((ssc_world::ROTTEN_FLESH, 1)));
        assert_eq!(session.survival.hunger_effect_ticks > 0, should_apply);
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
        assert_eq!(rig.server.items.values().map(|item| item.count as u16).sum::<u16>(), 8);

        rig.server.sessions.get_mut(&1).unwrap().survival.respawn_at =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
        rig.server.tick();
        assert_eq!(rig.server.items.values().map(|item| item.count as u16).sum::<u16>(), 8);
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
    fn creative_set_hotbar_slot_rejected_in_survival() {
        let mut rig = Rig::new_mode(1, 0.0, GameMode::Survival);
        rig.server
            .handle_packet(1, ClientPacket::SetHotbarSlot { slot: 0, block: ssc_world::BRICKS })
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
        rig.server.world.set_block(target.0, target.1, target.2, ssc_world::DIRT);
        {
            let session = rig.server.sessions.get_mut(&1).unwrap();
            session.inventory[0] = Some((ssc_world::APPLE, 1));
            session.survival.die();
        }
        rig.server
            .handle_packet(1, ClientPacket::DigStart)
            .unwrap();
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
    fn solid_wall_blocks_pvp_but_not_unobstructed_attacks() {
        let mut rig = Rig::new_mode(2, 0.0, GameMode::Survival);
        rig.server.world.set_block(10, 56, 8, ssc_world::STONE);
        let before = rig.server.sessions[&2].survival.health;

        rig.server
            .handle_packet(1, ClientPacket::DigStart)
            .unwrap();
        assert_eq!(rig.server.sessions[&2].survival.health, before);

        rig.server.world.set_block(10, 56, 8, AIR);
        rig.server
            .handle_packet(1, ClientPacket::DigStart)
            .unwrap();
        assert_eq!(rig.server.sessions[&2].survival.health, before - 2.0);
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
        rig.server
            .handle_packet(1, ClientPacket::DigStart)
            .unwrap();
        assert_eq!(rig.server.mobs[&mob_id].health, before);

        rig.server.world.set_block(10, 56, 8, AIR);
        rig.server
            .handle_packet(1, ClientPacket::DigStart)
            .unwrap();
        assert_eq!(rig.server.mobs[&mob_id].health, before - 2.0);
    }

    #[test]
    fn dead_players_do_not_intercept_attacks() {
        let mut rig = Rig::new_mode(3, 0.0, GameMode::Survival);
        let attacker = rig.server.sessions[&1].player.pos;
        rig.server.sessions.get_mut(&2).unwrap().player.pos =
            attacker + Vec3::new(2.0, 0.0, 0.0);
        rig.server.sessions.get_mut(&3).unwrap().player.pos =
            attacker + Vec3::new(4.0, 0.0, 0.0);
        rig.server.sessions.get_mut(&2).unwrap().survival.die();
        let health = rig.server.sessions[&3].survival.health;

        rig.server
            .handle_packet(1, ClientPacket::DigStart)
            .unwrap();

        assert_eq!(rig.server.sessions[&3].survival.health, health - 2.0);
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

        rig.server
            .handle_packet(1, ClientPacket::DigStart)
            .unwrap();

        assert_eq!(rig.server.mobs[&(FIRST_MOB_ID + 1)].health, health - 2.0);
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
            assert!(ids.contains(&1) && ids.contains(&2), "client {c} got {ids:?}");
        }
    }

    #[test]
    fn player_column_uses_floor_at_negative_coordinates() {
        let mut rig = Rig::new(1, 0.0);
        rig.server.sessions.get_mut(&1).unwrap().player.pos =
            Vec3::new(-0.5, 55.0, -16.1);
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
        let path = std::env::temp_dir().join(format!(
            "ssc-server-world-{}.bin",
            std::process::id()
        ));
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
        let world_path = std::env::temp_dir().join(format!(
            "ssc-anonymous-world-{}.bin",
            std::process::id()
        ));
        let player_id = [0x31; 16];
        let player_path = crate::persistence::player_path(&world_path, &player_id);
        let mut inventory = vec![None; crate::survival::INVENTORY_SIZE];
        inventory[2] = Some((ssc_world::APPLE, 7));
        let data = crate::persistence::PlayerSave {
            inventory: inventory.clone(),
            position: Some([12.5, 70.0, -9.25]),
            health: 13.0,
            hunger: 7,
            air: 250,
            selected_slot: 4,
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
        assert_ne!(server.sessions[&id].spawn, server.sessions[&id].player.pos);
        assert_eq!(server.sessions[&id].survival.health, 13.0);
        assert_eq!(server.sessions[&id].survival.hunger, 7);
        assert_eq!(server.sessions[&id].survival.air, 250);
        assert_eq!(server.sessions[&id].player.selected_slot, 4);
        server.sessions.get_mut(&id).unwrap().inventory[2] = Some((ssc_world::APPLE, 9));
        server.save_player_data(id);
        let saved = crate::persistence::load_player(&player_path, &player_id)
            .unwrap()
            .unwrap();
        assert_eq!(saved.inventory[2], Some((ssc_world::APPLE, 9)));
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
        let world_path =
            std::env::temp_dir().join(format!("ssc-corrupt-player-world-{}.bin", std::process::id()));
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
            crate::mobs::Mob::new(
                crate::mobs::MobKind::Pig,
                Vec3::new(8.5, 65.0, 8.5),
            ),
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
                position: Some([4.5, 70.0, -2.5]),
                health: 0.0,
                hunger: 0,
                air: 0,
                selected_slot: 0,
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
        assert_ne!(session.player.pos, Vec3::new(4.5, 70.0, -2.5));
        assert_eq!(session.player.pos, session.spawn);
        std::fs::remove_dir_all(player_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn dropped_items_restore_from_world_save() {
        let world_path = std::env::temp_dir().join(format!(
            "ssc-item-world-{}.bin",
            std::process::id()
        ));
        let mut cfg = ServerConfig::default();
        cfg.world_path = Some(world_path.clone());
        let mut server = Server::empty(cfg.clone());
        server.spawn_item_entity(
            ssc_world::PORKCHOP,
            2,
            Vec3::new(-12.5, 65.0, 7.25),
        );
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
        let mut mob = crate::mobs::Mob::new(
            crate::mobs::MobKind::Zombie,
            Vec3::new(8.5, 49.0, 8.5),
        );
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
        let path =
            std::env::temp_dir().join(format!("ssc-server-world-corrupt-{}.bin", std::process::id()));
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
