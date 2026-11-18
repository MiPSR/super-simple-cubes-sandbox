//! The authoritative game server.
//!
//! Owns the world, simulates players at 20 TPS, streams columns + LOD data
//! to the client, performs dig/place raycasts and emits sound triggers —
//! everything the client knows comes from here (`specs.md` §2).

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use ssc_math::{
    look_dir, yaw_forward, yaw_right, GRAVITY, JUMP_VELOCITY, PLAYER_EYE_HEIGHT, REACH, SNEAK_SPEED,
    SPRINT_SPEED, TERMINAL_VELOCITY, WALK_SPEED, Vec3,
};
use ssc_protocol::packets::{keys, ClientPacket, ServerPacket, SoundId};
use ssc_protocol::wire::{memory_pair, MemoryWire, Wire};
use ssc_protocol::PROTOCOL_VERSION;
use ssc_world::{is_solid, BlockId, ColumnKey, World, AIR, SAND, SANDSTONE, SNOW_BLOCK};

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
            local: false,
        }
    }
}

/// One connected player (v0: exactly one).
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
    joined: bool,
}

/// The game server. Drives one client `Wire`.
pub struct Server {
    cfg: ServerConfig,
    world: World,
    gen: WorldGen,
    edited: HashSet<ColumnKey>,
    player: Player,
    wire: Box<dyn Wire>,
    /// Full columns queued to stream, nearest first.
    stream_queue: VecDeque<ColumnKey>,
    /// Columns already sent (for unload bookkeeping).
    streamed: HashSet<ColumnKey>,
    /// LOD columns queued to stream.
    lod_queue: VecDeque<ColumnKey>,
    lod_streamed: HashSet<ColumnKey>,
    last_player_col: ColumnKey,
    tick: u64,
    running: bool,
}

impl Server {
    pub fn new(cfg: ServerConfig, wire: Box<dyn Wire>) -> Self {
        let spawn = Vec3::new(8.5, 128.0, 8.5); // corrected on join
        Self {
            gen: WorldGen::new(cfg.seed),
            world: World::default(),
            edited: HashSet::new(),
            player: Player {
                name: String::new(),
                pos: spawn,
                vel: Vec3::ZERO,
                yaw: 0.0,
                pitch: 0.0,
                keys: 0,
                selected_slot: 0,
                on_ground: false,
                joined: false,
            },
            cfg,
            wire,
            stream_queue: VecDeque::new(),
            streamed: HashSet::new(),
            lod_queue: VecDeque::new(),
            lod_streamed: HashSet::new(),
            last_player_col: (i32::MAX, i32::MAX),
            tick: 0,
            running: true,
        }
    }

    /// Run until the client disconnects. Call from a dedicated thread.
    pub fn run(mut self) {
        let mut next_tick = Instant::now();
        while self.running {
            // Drain everything the client sent this tick.
            while let Some(bytes) = self.wire.try_recv() {
                if let Err(e) = self.handle_frame(bytes) {
                    eprintln!("[server] dropping client: {e}");
                    self.running = false;
                    break;
                }
            }
            if !self.running {
                break;
            }

            self.tick();

            if !self.wire.is_alive() && self.player.joined {
                eprintln!("[server] client disconnected");
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
        let _ = self.send(&ServerPacket::Goodbye { reason: "server closed".into() });
    }

    fn tick(&mut self) {
        self.tick += 1;

        if self.player.joined {
            self.step_player(1.0 / TICK_RATE);

            let p = &self.player;
            let _ = self.send(&ServerPacket::PlayerState {
                x: p.pos.x,
                y: p.pos.y,
                z: p.pos.z,
                yaw: p.yaw,
                pitch: p.pitch,
            });
        }

        // Stream pending columns/LOD.
        let cols_this_tick = if self.cfg.local { usize::MAX } else { 4 };
        for _ in 0..cols_this_tick {
            match self.stream_queue.pop_front() {
                Some(c) => self.stream_column(c),
                None => break,
            }
        }
        let lods_this_tick = if self.cfg.local { usize::MAX } else { 48 };
        for _ in 0..lods_this_tick {
            match self.lod_queue.pop_front() {
                Some(c) => self.stream_lod(c),
                None => break,
            }
        }

        // Refresh streaming sets periodically (and right after joins).
        if self.tick % 20 == 0 {
            self.refresh_streaming();
        }
    }

    // -----------------------------------------------------------------------
    // Player simulation
    // -----------------------------------------------------------------------

    fn step_player(&mut self, dt: f32) {
        let k = self.player.keys;
        let yaw = self.player.yaw;

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
        let speed = if sprint { SPRINT_SPEED } else if sneak { SNEAK_SPEED } else { WALK_SPEED };
        let wish_len = (wish[0] * wish[0] + wish[1] * wish[1]).sqrt();
        let target = if wish_len > 0.0 {
            (wish[0] / wish_len * speed, wish[1] / wish_len * speed)
        } else {
            (0.0, 0.0)
        };

        // Ground: instant target velocity. Air: limited steering.
        if self.player.on_ground {
            self.player.vel.x = target.0;
            self.player.vel.z = target.1;
            if k & keys::JUMP != 0 {
                self.player.vel.y = JUMP_VELOCITY;
                self.player.on_ground = false;
            }
        } else {
            let blend = 0.15;
            self.player.vel.x += (target.0 - self.player.vel.x) * blend;
            self.player.vel.z += (target.1 - self.player.vel.z) * blend;
        }

        self.player.vel.y -= GRAVITY * dt;
        if self.player.vel.y < -TERMINAL_VELOCITY {
            self.player.vel.y = -TERMINAL_VELOCITY;
        }

        let mut pos = self.player.pos;
        let mut vel = self.player.vel;
        let res = move_player(&self.world, &mut pos, &mut vel, dt);
        self.player.pos = pos;
        self.player.vel = vel;
        self.player.on_ground = res.on_ground;
    }

    // -----------------------------------------------------------------------
    // Streaming
    // -----------------------------------------------------------------------

    fn refresh_streaming(&mut self) {
        let (pcx, pcz) = (
            (self.player.pos.x as i32).div_euclid(16),
            (self.player.pos.z as i32).div_euclid(16),
        );
        if (pcx, pcz) == self.last_player_col && self.stream_queue.is_empty() {
            // Only unload checks needed.
        } else {
            self.last_player_col = (pcx, pcz);
        }

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
        for key in wanted {
            if !self.streamed.contains(&key) && !self.stream_queue.contains(&key) {
                self.stream_queue.push_back(key);
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
            if !self.lod_streamed.contains(&key) && !self.lod_queue.contains(&key) {
                self.lod_queue.push_back(key);
            }
        }

        // Unload what fell out of range.
        let out_of_range: Vec<ColumnKey> = self
            .streamed
            .iter()
            .filter(|(cx, cz)| (cx - pcx).abs() > rd + 1 || (cz - pcz).abs() > rd + 1)
            .copied()
            .collect();
        for key in out_of_range {
            self.streamed.remove(&key);
            let _ = self.send(&ServerPacket::UnloadColumn { cx: key.0, cz: key.1 });
            // Evict server memory if far, unedited.
            if (key.0 - pcx).abs() > rd + 4
                && (key.1 - pcz).abs() > rd + 4
                && !self.edited.contains(&key)
            {
                self.world.columns.remove(&key);
            }
        }
        let lod_out: Vec<ColumnKey> = self
            .lod_streamed
            .iter()
            .filter(|(cx, cz)| (cx - pcx).abs() > ld + 1 || (cz - pcz).abs() > ld + 1)
            .copied()
            .collect();
        for key in lod_out {
            self.lod_streamed.remove(&key);
            let _ = self.send(&ServerPacket::UnloadLod { cx: key.0, cz: key.1 });
        }
    }

    /// Ensure a column exists server-side and stream it fully.
    fn stream_column(&mut self, key: ColumnKey) {
        if self.world.column(key.0, key.1).is_none() {
            let col = self.gen.generate_column(key.0, key.1);
            self.world.columns.insert(key, col);
        }
        // Collect sub-chunk payloads first (immutable borrow of the world),
        // then send (mutable borrow of self).
        let payloads: Vec<(u8, Vec<u16>)> = {
            let column = self.world.column(key.0, key.1).unwrap();
            let mut out = Vec::new();
            for (sub, slot) in column.subchunks.iter().enumerate() {
                if let Some(sc) = slot {
                    let mut blocks = Vec::with_capacity(ssc_world::SUBCHUNK_VOLUME);
                    for y in 0..16 {
                        for z in 0..16 {
                            for x in 0..16 {
                                blocks.push(sc.get(x, y, z));
                            }
                        }
                    }
                    out.push((sub as u8, blocks));
                }
            }
            out
        };
        for (sub, blocks) in payloads {
            let _ = self.send(&ServerPacket::Subchunk {
                cx: key.0,
                cz: key.1,
                sub,
                blocks,
            });
        }
        let _ = self.send(&ServerPacket::ColumnEnd { cx: key.0, cz: key.1 });
        self.streamed.insert(key);
    }

    /// Stream the LOD summary of one column.
    fn stream_lod(&mut self, key: ColumnKey) {
        let cells = self
            .gen
            .lod_summary(key.0, key.1, &self.world, &self.edited);
        let _ = self.send(&ServerPacket::LodColumn { cx: key.0, cz: key.1, cells });
        self.lod_streamed.insert(key);
    }

    // -----------------------------------------------------------------------
    // Interaction (dig / place) — server decides everything.
    // -----------------------------------------------------------------------

    fn eye(&self) -> Vec3 {
        self.player.pos + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0)
    }

    fn handle_dig(&mut self) {
        let dir = look_dir(self.player.yaw, self.player.pitch);
        let Some(hit) = raycast(&self.world, self.eye(), dir, REACH) else {
            return;
        };
        let [x, y, z] = hit.block;
        if self.world.set_block(x, y, z, AIR) {
            self.edited.insert((x.div_euclid(16), z.div_euclid(16)));
            let _ = self.send(&ServerPacket::BlockUpdate { x, y, z, block: AIR });
            self.send_sound(SoundId::Dig, x, y, z);
            // LOD must reflect the edit (server-driven LOD, specs.md §3).
            self.stream_lod((x.div_euclid(16), z.div_euclid(16)));
        }
    }

    fn handle_place(&mut self) {
        let dir = look_dir(self.player.yaw, self.player.pitch);
        let Some(hit) = raycast(&self.world, self.eye(), dir, REACH) else {
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
        // Never place inside the player.
        if player_aabb(self.player.pos)
            .intersects(&ssc_math::Aabb::block(x, y, z))
        {
            return;
        }

        let block = HOTBAR[self.player.selected_slot as usize % HOTBAR.len()];
        if self.world.set_block(x, y, z, block) {
            self.edited.insert((x.div_euclid(16), z.div_euclid(16)));
            let _ = self.send(&ServerPacket::BlockUpdate { x, y, z, block });
            self.send_sound(SoundId::Place, x, y, z);
            self.stream_lod((x.div_euclid(16), z.div_euclid(16)));
        }
    }

    /// Sound triggers are server-driven (`specs.md` §2): the client receives
    /// these packets and plays/logs them — it never decides sounds itself.
    fn send_sound(&mut self, id: SoundId, x: i32, y: i32, z: i32) {
        let _ = self.send(&ServerPacket::Sound {
            sound: id.to_u16(),
            x: x as f32 + 0.5,
            y: y as f32 + 0.5,
            z: z as f32 + 0.5,
        });
    }

    // -----------------------------------------------------------------------
    // Packet handling
    // -----------------------------------------------------------------------

    fn handle_frame(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        let pkt = ClientPacket::decode(&bytes).map_err(|e| e.to_string())?;
        match pkt {
            ClientPacket::Hello { version, name } => self.on_hello(version, name),
            ClientPacket::Input { keys, yaw, pitch } => {
                if !self.player.joined {
                    return Err("input before hello".into());
                }
                self.player.keys = keys;
                self.player.yaw = yaw;
                self.player.pitch = pitch.clamp(-89.9, 89.9);
            }
            ClientPacket::SelectSlot { slot } => {
                self.player.selected_slot = slot % HOTBAR.len() as u8;
            }
            ClientPacket::Dig => {
                if self.player.joined {
                    self.handle_dig();
                }
            }
            ClientPacket::Place => {
                if self.player.joined {
                    self.handle_place();
                }
            }
        }
        Ok(())
    }

    fn on_hello(&mut self, version: u32, name: String) {
        if version != PROTOCOL_VERSION {
            let _ = self.send(&ServerPacket::Goodbye {
                reason: format!("protocol mismatch: client {version}, server {PROTOCOL_VERSION}"),
            });
            self.running = false;
            return;
        }
        self.player.name = name;

        // Find the spawn point on real terrain.
        let h = self.gen.height(8, 8);
        self.player.pos = Vec3::new(8.5, (h + 1) as f32 + 0.02, 8.5);
        self.player.vel = Vec3::ZERO;
        self.player.joined = true;

        let _ = self.send(&ServerPacket::Welcome {
            spawn: [self.player.pos.x, self.player.pos.y, self.player.pos.z],
            render_distance: self.cfg.render_distance as u8,
            hotbar: HOTBAR,
        });
        self.refresh_streaming();
    }

    fn send(&mut self, pkt: &ServerPacket) -> Result<(), String> {
        self.wire.send(&pkt.encode())
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
