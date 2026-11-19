//! The authoritative game server.
//!
//! Owns the world, simulates every connected player at 20 TPS, streams
//! columns + LOD data per player, performs dig/place raycasts and emits
//! sound triggers — everything the clients know comes from here
//! (`specs.md` §2). Multi-player: one shared world, one session per
//! client wire.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use ssc_math::{
    look_dir, yaw_forward, yaw_right, GRAVITY, JUMP_VELOCITY, PLAYER_EYE_HEIGHT, REACH, SNEAK_SPEED,
    SPRINT_SPEED, TERMINAL_VELOCITY, WALK_SPEED, Vec3,
};
use ssc_protocol::packets::{keys, ClientPacket, ServerPacket, SoundId};
use ssc_protocol::wire::{memory_pair, MemoryWire, Wire};
use ssc_protocol::PROTOCOL_VERSION;
use ssc_world::{is_solid, BlockId, ColumnKey, World, AIR, SAND, SANDSTONE, SNOW_BLOCK, WATER};

use crate::physics::{move_player, player_aabb};
use crate::raycast::raycast;
use crate::worldgen::WorldGen;

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
    selected_slot: u8,
    on_ground: bool,
    in_water: bool,
    joined: bool,
}

/// A client connection: wire + player + its own streaming state.
struct Session {
    wire: Box<dyn Wire>,
    player: Player,
    /// The player's inventory (36 slots; 0..9 = hotbar).
    inventory: Vec<crate::survival::Slot>,
    /// Survival-mode state (ignored in creative).
    survival: crate::survival::SurvivalState,
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
            player: Player {
                name: String::new(),
                pos: Vec3::new(8.5, 128.0, 8.5),
                vel: Vec3::ZERO,
                yaw: 0.0,
                pitch: 0.0,
                keys: 0,
                selected_slot: 0,
                on_ground: false,
                in_water: false,
                joined: false,
            },
            inventory,
            survival: crate::survival::SurvivalState::default(),
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
    sessions: HashMap<EntityId, Session>,
    next_id: EntityId,
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
        Self {
            gen: WorldGen::new(cfg.seed),
            world: World::default(),
            edited: HashSet::new(),
            cfg,
            sessions: HashMap::new(),
            next_id: 1,
            accept_rx: None,
            tick: 0,
            time_of_day: 0.3, // start in the morning
            running: true,
        }
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
            step_player(&self.world, p, dt);
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
            .collect();
        for pkt in &states {
            let _ = self.broadcast(pkt);
        }

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
        }
    }
}

// ---------------------------------------------------------------------------
// Player simulation
// ---------------------------------------------------------------------------

/// Simulate one player for one tick (server-authoritative).
fn step_player(world: &World, player: &mut Player, dt: f32) {
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
    let speed = if in_water {
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

    if in_water {
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
    }
}

impl Server {
    fn player_col(&self, id: EntityId) -> ColumnKey {
        let p = &self.sessions[&id].player.pos;
        (
            (p.x as i32).div_euclid(16),
            (p.z as i32).div_euclid(16),
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
        if self.world.column(key.0, key.1).is_none() {
            let col = self.gen.generate_column(key.0, key.1);
            self.world.columns.insert(key, col);
        }
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

    /// Stream the LOD summary of one column to one player.
    fn stream_lod(&mut self, id: EntityId, key: ColumnKey) {
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
                    (p.x as i32).div_euclid(16),
                    (p.z as i32).div_euclid(16),
                )
            })
            .collect();
        self.world.columns.retain(|(cx, cz), _| {
            if self.edited.contains(&(*cx, *cz)) {
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
            ClientPacket::Hello { version, name } => self.on_hello(id, version, name),
            ClientPacket::Input { keys, yaw, pitch } => {
                let Some(session) = self.sessions.get_mut(&id) else { return Ok(()) };
                if !session.player.joined {
                    return Err("input before hello".into());
                }
                // Dead players give no input (server decides, specs.md §2).
                if !session.survival.dead {
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
                if !session.player.joined {
                    return Ok(());
                }
                if (a as usize) >= crate::survival::INVENTORY_SIZE
                    || (b as usize) >= crate::survival::INVENTORY_SIZE
                {
                    return Err(format!("slot out of range: {a}/{b}"));
                }
                session.inventory.swap(a as usize, b as usize);
                session.inv_dirty = true;
            }
            ClientPacket::DigStart => {
                if self
                    .sessions
                    .get(&id)
                    .is_some_and(|s| s.player.joined)
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

    fn on_hello(&mut self, id: EntityId, version: u32, name: String) {
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
        let Some(session) = self.sessions.get_mut(&id) else { return };
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
        session.player.pos = Vec3::new(spawn_x, spawn_y, spawn_z);
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
        session.spawn = session.player.pos;
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

    /// Start digging: creative breaks instantly, survival begins timed
    /// progress (advanced in `tick`).
    fn handle_dig_start(&mut self, id: EntityId) {
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
        self.edited.insert((x.div_euclid(16), z.div_euclid(16)));
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
                    eprintln!("[server] player {id} inventory full, drop lost");
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
                let ate = {
                    let Some(session) = self.sessions.get_mut(&id) else { return };
                    let s = &mut session.survival;
                    s.eat(food)
                };
                if ate {
                    // Consume one.
                    let slot = self.sessions[&id].player.selected_slot as usize % 9;
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
            self.edited.insert((x.div_euclid(16), z.div_euclid(16)));
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
            packets.iter().all(|p| !matches!(p, p @ ServerPacket::BlockUpdate { .. })),
            "no instant break in survival: {packets:?}"
        );
        assert!(rig.server.sessions[&1].survival.digging.is_some());

        // 149 ticks: almost done, still not broken.
        for _ in 0..149 {
            rig.server.tick();
        }
        let packets = rig.drain(0);
        assert!(
            packets.iter().all(|p| !matches!(p, p @ ServerPacket::BlockUpdate { .. })),
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
