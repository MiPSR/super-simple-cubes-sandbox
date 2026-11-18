//! `ssc-client` — the game client.
//!
//! Per `specs.md` §2 this process contains **zero game logic**: it renders
//! what the server sends and forwards player input. Even sound triggers
//! come from the server (printed here until an audio backend is approved
//! and wired up — ask-before-deps, `specs.md` §4). All rendering goes
//! through `ssc-engine` — never a raw GFX API. For local play it embeds
//! the `ssc-server` library in-process.

mod atlas;
mod mesh;
mod state;

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ssc_engine::{
    Camera, DrawCmd, EngineEvent, EngineSettings, Flow, Frame, Key, KeyState, MeshId,
    MouseButton,
};
use ssc_math::{MOUSE_SENS, PLAYER_EYE_HEIGHT, Vec3};
use ssc_protocol::packets::{keys, ClientPacket, ServerPacket, SoundId};
use ssc_protocol::wire::{TcpWire, Wire};
use ssc_protocol::PROTOCOL_VERSION;
use ssc_server::{spawn_local, ServerConfig};
use ssc_world::{valid_id, BlockId, ColumnKey};

use mesh::SubchunkMesh;
use state::ClientWorld;

/// LOD cells are skipped within this many columns of the player (cheby).
/// Near meshes only exist for *interior* columns (a column's 4 neighbors
/// must be streamed), i.e. cheb ≤ rd−1 — so the skip margin must be rd−1
/// too: any wider and a sky gap ring opens between near terrain and LOD.
const NEAR_MARGIN_TRIM: i32 = 1;

/// Far plane: must cover the LOD ring.
const Z_FAR: f32 = 1800.0;

/// Dig/place repeat rate while the button is held.
const ACTION_REPEAT: Duration = Duration::from_millis(250);

/// Interval for player input snapshots.
const INPUT_INTERVAL: Duration = Duration::from_millis(45);

// ---------------------------------------------------------------------------
// Mesh worker (background meshing; GL stays on the main thread)
// ---------------------------------------------------------------------------

enum Job {
    Column(ColumnKey),
    Region {
        rx: i32,
        rz: i32,
        pcx: i32,
        pcz: i32,
        near_margin: i32,
    },
}

enum JobResult {
    Column {
        key: ColumnKey,
        meshes: Vec<SubchunkMesh>,
        /// Visibility masks for all 24 sub-chunks (occlusion culling data).
        masks: Vec<(usize, mesh::FaceMasks)>,
    },
    Region { rx: i32, rz: i32, mesh: Option<ssc_lod::LodMesh> },
}

fn spawn_worker(shared: Arc<RwLock<ClientWorld>>) -> (Sender<Job>, Receiver<JobResult>) {
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let (res_tx, res_rx) = mpsc::channel::<JobResult>();
    std::thread::Builder::new()
        .name("mesh-worker".into())
        .spawn(move || {
            while let Ok(job) = job_rx.recv() {
                match job {
                    Job::Column(key) => {
                        let guard = shared.read().unwrap_or_else(|e| e.into_inner());
                        let (meshes, masks) = mesh::build_column_meshes(&guard.world, key.0, key.1);
                        let _ = res_tx.send(JobResult::Column { key, meshes, masks });
                    }
                    Job::Region { rx, rz, pcx, pcz, near_margin } => {
                        let guard = shared.read().unwrap_or_else(|e| e.into_inner());
                        let built = guard.build_region(rx, rz, pcx, pcz, near_margin);
                        let _ = res_tx.send(JobResult::Region { rx, rz, mesh: built });
                    }
                }
            }
        })
        .expect("spawn mesh worker");
    (job_tx, res_rx)
}

// ---------------------------------------------------------------------------
// The client application
// ---------------------------------------------------------------------------

/// Interpolated view of another player.
struct EntityView {
    prev: Vec3,
    cur: Vec3,
    time: Instant,
    yaw: f32,
    mesh: MeshId,
}

struct App {
    wire: Box<dyn Wire>,
    shared: Arc<RwLock<ClientWorld>>,
    job_tx: Sender<Job>,
    job_rx: Receiver<JobResult>,
    /// Jobs in flight (avoid duplicates).
    queued_cols: HashMap<ColumnKey, Instant>,
    queued_regions: HashMap<(i32, i32), Instant>,
    /// Live GPU meshes: (column, sub, translucent) → (mesh id, bounds).
    sub_meshes: HashMap<(ColumnKey, usize, bool), (MeshId, ssc_math::Aabb)>,
    region_meshes: HashMap<(i32, i32), (MeshId, ssc_math::Aabb)>,
    /// Per-sub-chunk border masks for occlusion culling.
    face_masks: HashMap<mesh::SubKey, mesh::FaceMasks>,
    next_mesh_id: MeshId,
    /// Own server-assigned entity id.
    own_id: Option<u16>,
    /// Other players.
    entities: HashMap<u16, EntityView>,
    /// Placeholder avatar geometry (uploaded per entity, built once).
    avatar_verts: Vec<ssc_engine::Vertex>,
    avatar_indices: Vec<u32>,
    // Player presentation state.
    pos_prev: Vec3,
    pos_cur: Vec3,
    state_time: Instant,
    yaw: f32,
    pitch: f32,
    keys: u16,
    selected_slot: u8,
    hotbar: [BlockId; 9],
    joined: bool,
    render_distance: i32,
    last_player_col: ColumnKey,
    // Timers.
    hello_sent: bool,
    last_input_send: Instant,
    last_dig: Instant,
    last_place: Instant,
    ui_click: bool,
    // Stats reporting.
    frames: u32,
    stat_ms: f32,
    stat_drawn: u32,
    stat_culled: u32,
    stat_occluded: u32,
    stat_calls: u32,
    last_report: Instant,
    atlas_uploaded: bool,
    /// Optional texture-pack atlas (PNG) given via `--textures`.
    texture_path: Option<std::path::PathBuf>,
}

impl App {
    fn new(
        wire: Box<dyn Wire>,
        shared: Arc<RwLock<ClientWorld>>,
        job_tx: Sender<Job>,
        job_rx: Receiver<JobResult>,
        texture_path: Option<std::path::PathBuf>,
    ) -> Self {
        let now = Instant::now();
        // Debug camera overrides (orientation testing / screenshots).
        let yaw = std::env::var("SSC_YAW").ok().and_then(|v| v.parse().ok());
        let pitch = std::env::var("SSC_PITCH").ok().and_then(|v| v.parse().ok());
        let (avatar_verts, avatar_indices) = mesh::build_avatar();
        Self {
            wire,
            shared,
            job_tx,
            job_rx,
            queued_cols: HashMap::new(),
            queued_regions: HashMap::new(),
            sub_meshes: HashMap::new(),
            region_meshes: HashMap::new(),
            face_masks: HashMap::new(),
            next_mesh_id: 1,
            own_id: None,
            entities: HashMap::new(),
            pos_prev: Vec3::new(8.5, 128.0, 8.5),
            pos_cur: Vec3::new(8.5, 128.0, 8.5),
            state_time: now,
            avatar_verts,
            avatar_indices,
            yaw: yaw.unwrap_or(0.0),
            pitch: pitch.unwrap_or(0.0),
            keys: 0,
            selected_slot: 0,
            hotbar: [0; 9],
            joined: false,
            render_distance: 8,
            last_player_col: (0, 0),
            hello_sent: false,
            last_input_send: now,
            last_dig: now,
            last_place: now,
            ui_click: false,
            frames: 0,
            stat_ms: 0.0,
            stat_drawn: 0,
            stat_culled: 0,
            stat_occluded: 0,
            stat_calls: 0,
            last_report: now,
            atlas_uploaded: false,
            texture_path,
        }
    }

    fn send(&mut self, pkt: &ClientPacket) {
        if let Err(e) = self.wire.send(&pkt.encode()) {
            eprintln!("[client] send failed: {e}");
        }
    }

    /// One frame.
    fn tick(&mut self, frame: &mut Frame, events: &[EngineEvent]) -> Flow {
        // Upload the atlas once: a real texture pack if `--textures` was
        // given, otherwise the procedural placeholder (specs.md §3).
        if !self.atlas_uploaded {
            let rgba = match &self.texture_path {
                Some(path) => match atlas::load_png_atlas(path) {
                    Ok(rgba) => {
                        eprintln!("[client] texture pack loaded: {}", path.display());
                        rgba
                    }
                    Err(e) => {
                        eprintln!("[client] texture pack failed ({e}); using placeholder atlas");
                        atlas::build_atlas_rgba()
                    }
                },
                None => atlas::build_atlas_rgba(),
            };
            if let Err(e) = frame.upload_atlas(&rgba, atlas::ATLAS_PX, atlas::ATLAS_PX) {
                eprintln!("[client] atlas upload failed: {e}");
            }
            self.atlas_uploaded = true;
        }

        self.ui_click = false;
        for ev in events {
            match *ev {
                EngineEvent::Closed => return Flow::Exit,
                EngineEvent::Key { key: Key::Escape, state: KeyState::Pressed } => {
                    let locked = frame.mouse_locked();
                    frame.set_mouse_locked(!locked);
                }
                EngineEvent::Key { key: Key::Digit(n), state: KeyState::Pressed } => {
                    self.selected_slot = n - 1;
                    if self.joined {
                        let id = self.hotbar[self.selected_slot as usize];
                        println!(
                            "[hotbar] slot {}: {}",
                            self.selected_slot,
                            ssc_world::info(id).name
                        );
                        self.send(&ClientPacket::SelectSlot { slot: self.selected_slot });
                    }
                }
                // Immediate action on the press event: a quick click never
                // shows up in the held-state snapshot (press and release
                // land in the same frame's event batch).
                EngineEvent::MouseButton { button: MouseButton::Left, state: KeyState::Pressed } => {
                    if frame.mouse_locked() {
                        self.send(&ClientPacket::Dig);
                        self.last_dig = Instant::now();
                    } else {
                        // While unlocked, the first click grabs the mouse
                        // instead of interacting (classic FPS UX).
                        frame.set_mouse_locked(true);
                    }
                }
                EngineEvent::MouseButton { button: MouseButton::Right, state: KeyState::Pressed } => {
                    if frame.mouse_locked() {
                        self.send(&ClientPacket::Place);
                        self.last_place = Instant::now();
                    }
                }
                EngineEvent::Focused(false) => frame.set_mouse_locked(false),
                _ => {}
            }
        }

        // Look (client-side smoothing of server-driven state; logic stays
        // on the server — this is presentation only).
        let input = frame.input();
        if frame.mouse_locked() {
            self.yaw += input.mouse_dx * MOUSE_SENS;
            self.pitch -= input.mouse_dy * MOUSE_SENS;
            self.pitch = self.pitch.clamp(-1.5697, 1.5697); // ±89.9°
        }

        // Input mask.
        let i = frame.input();
        let mut mask = 0u16;
        if i.key(Key::W) {
            mask |= keys::FORWARD;
        }
        if i.key(Key::S) {
            mask |= keys::BACKWARD;
        }
        if i.key(Key::A) {
            mask |= keys::LEFT;
        }
        if i.key(Key::D) {
            mask |= keys::RIGHT;
        }
        if i.key(Key::Space) {
            mask |= keys::JUMP;
        }
        if i.key(Key::Shift) {
            mask |= keys::SNEAK;
        }
        if i.key(Key::Ctrl) {
            mask |= keys::SPRINT;
        }
        self.keys = mask;

        // Network out.
        if !self.hello_sent {
            self.hello_sent = true;
            self.send(&ClientPacket::Hello {
                version: PROTOCOL_VERSION,
                name: "player".into(),
            });
        }
        if Instant::now() - self.last_input_send >= INPUT_INTERVAL {
            self.last_input_send = Instant::now();
            self.send(&ClientPacket::Input { keys: self.keys, yaw: self.yaw, pitch: self.pitch });
        }

        // Held dig/place: repeat while the button stays down.
        {
            let now = Instant::now();
            if i.mouse_left && frame.mouse_locked() && now - self.last_dig >= ACTION_REPEAT {
                self.last_dig = now;
                self.send(&ClientPacket::Dig);
            }
            if i.mouse_right && frame.mouse_locked() && now - self.last_place >= ACTION_REPEAT {
                self.last_place = now;
                self.send(&ClientPacket::Place);
            }
        }

        // Network in.
        while let Some(bytes) = self.wire.try_recv() {
            match ServerPacket::decode(&bytes) {
                Ok(p) => {
                    if self.handle_packet(frame, p) == Flow::Exit {
                        return Flow::Exit;
                    }
                }
                Err(e) => {
                    eprintln!("[client] bad packet from server: {e}");
                    return Flow::Exit;
                }
            }
        }

        // Drop stale queued jobs (server moved on).
        self.gc_stale_jobs();

        // Enqueue mesh jobs for dirty columns/regions.
        self.pump_mesh_jobs(frame);

        // Consume finished meshes.
        self.upload_finished_meshes(frame);

        // Track player column: nearby LOD regions must re-mesh when the
        // near ring moves.
        if self.joined {
            let pcx = (self.pos_cur.x as i32).div_euclid(16);
            let pcz = (self.pos_cur.z as i32).div_euclid(16);
            if (pcx, pcz) != self.last_player_col {
                self.last_player_col = (pcx, pcz);
                let (prx, prz) = ssc_lod::region_of(pcx, pcz);
                {
                    let mut w = self.shared.write().unwrap_or_else(|e| e.into_inner());
                    for rx in prx - 2..=prx + 2 {
                        for rz in prz - 2..=prz + 2 {
                            w.dirty_regions.insert((rx, rz));
                        }
                    }
                }
            }
        }

        // Occlusion ("cave") culling: flood visibility from the camera's
        // sub-chunk through open borders. Missing masks (still streaming)
        // are treated as open; a missing camera node disables filtering.
        let eye_sub: mesh::SubKey = {
            let px = self.pos_cur.x as i32;
            let pz = self.pos_cur.z as i32;
            let py = (self.pos_cur.y as i32 + ssc_math::PLAYER_EYE_HEIGHT as i32)
                .clamp(ssc_world::MIN_Y, ssc_world::MAX_Y - 1);
            (
                (px.div_euclid(16), pz.div_euclid(16)),
                ssc_world::subchunk_index(py).unwrap_or(0),
            )
        };
        let occlusion = mesh::visible_flood(eye_sub, &|k| self.face_masks.get(&k).copied());

        // Build the draw list.
        let mut draws =
            Vec::with_capacity(self.sub_meshes.len() + self.region_meshes.len());
        let mut occluded = 0u32;
        for ((col, sub, translucent), (id, aabb)) in &self.sub_meshes {
            // Only opaque sub-chunk meshes participate in occlusion data
            // (water carries none — see mesh.rs); don't cull water on it.
            if !translucent {
                if let Some(vis) = &occlusion {
                    if !vis.contains(&(*col, *sub)) {
                        occluded += 1;
                        continue;
                    }
                }
            }
            draws.push(DrawCmd {
                mesh: *id,
                aabb: *aabb,
                lod: false,
                translucent: *translucent,
                model: ssc_math::Mat4::IDENTITY,
            });
        }
        for (_r, (id, aabb)) in &self.region_meshes {
            draws.push(DrawCmd {
                mesh: *id,
                aabb: *aabb,
                lod: true,
                translucent: false,
                model: ssc_math::Mat4::IDENTITY,
            });
        }
        // Other players: placeholder box avatars, placed by model matrix.
        for e in self.entities.values() {
            let alpha =
                ((Instant::now() - e.time).as_secs_f32() / 0.05).clamp(0.0, 1.0);
            let pos = e.prev.lerp(e.cur, alpha);
            let aabb = ssc_math::Aabb::new(
                Vec3::new(pos.x - 0.35, pos.y - 0.05, pos.z - 0.35),
                Vec3::new(pos.x + 0.35, pos.y + 1.9, pos.z + 0.35),
            );
            let model = ssc_math::Mat4::from_translation(pos)
                * ssc_math::Mat4::from_rotation_y(-e.yaw);
            draws.push(DrawCmd {
                mesh: e.mesh,
                aabb,
                lod: false,
                translucent: false,
                model,
            });
        }

        // Camera: interpolate the last two server states over one tick.
        let alpha = ((Instant::now() - self.state_time).as_secs_f32() / 0.05).clamp(0.0, 1.0);
        let pos = self.pos_prev.lerp(self.pos_cur, alpha);
        let camera = Camera {
            pos: pos + Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0),
            yaw: self.yaw,
            pitch: self.pitch,
            fov_y: ssc_engine::DEFAULT_FOV_Y,
            z_near: 0.1,
            z_far: Z_FAR,
        };

        let stats = frame.render(&camera, &draws);

        // Stats reporting (1 Hz).
        self.frames += 1;
        self.stat_ms += stats.frame_ms;
        self.stat_drawn += stats.drawn;
        self.stat_culled += stats.culled_frustum;
        self.stat_occluded += occluded;
        self.stat_calls += stats.draw_calls;
        let since = Instant::now() - self.last_report;
        if since >= Duration::from_secs(1) {
            let secs = since.as_secs_f32();
            let fps = self.frames as f32 / secs;
            if std::env::var("SSC_DEBUG").map(|v| v == "1").unwrap_or(false) {
                let has_spawn_col = self.sub_meshes.keys().any(|k| k.0 == (0, 0));
                eprintln!(
                    "[dbg] sub_meshes={} lod_regions={} queued_cols={} queued_regions={} spawn_col_mesh={}",
                    self.sub_meshes.len(),
                    self.region_meshes.len(),
                    self.queued_cols.len(),
                    self.queued_regions.len(),
                    has_spawn_col
                );
                if let Some((k, (id, aabb))) = self
                    .sub_meshes
                    .iter()
                    .find(|(k, _)| k.0 == (0, 0) && !k.2)
                {
                    eprintln!(
                        "[dbg] col(0,0) sub={} id={} aabb=({:.1},{:.1},{:.1})-({:.1},{:.1},{:.1})",
                        k.1, id,
                        aabb.min.x, aabb.min.y, aabb.min.z, aabb.max.x, aabb.max.y, aabb.max.z
                    );
                }
            }
            println!(
                "[stats] {:5.1} fps | render {:4.2} ms | draws {:4} (frust {:4} occ {:4}) | calls {:4} | ents {} | pos ({:7.2},{:7.2},{:7.2})",
                fps,
                self.stat_ms / self.frames as f32,
                self.stat_drawn / self.frames,
                self.stat_culled / self.frames,
                self.stat_occluded / self.frames,
                self.stat_calls / self.frames,
                self.entities.len(),
                self.pos_cur.x,
                self.pos_cur.y,
                self.pos_cur.z
            );
            self.frames = 0;
            self.stat_ms = 0.0;
            self.stat_drawn = 0;
            self.stat_culled = 0;
            self.stat_occluded = 0;
            self.stat_calls = 0;
            self.last_report = Instant::now();
        }

        Flow::Continue
    }

    fn handle_packet(&mut self, frame: &mut Frame, p: ServerPacket) -> Flow {
        match p {
            ServerPacket::Welcome { entity_id, spawn, render_distance, hotbar } => {
                self.own_id = Some(entity_id);
                self.hotbar = hotbar;
                self.render_distance = render_distance as i32;
                let s = Vec3::new(spawn[0], spawn[1], spawn[2]);
                self.pos_prev = s;
                self.pos_cur = s;
                self.state_time = Instant::now();
                self.joined = true;
                frame.set_mouse_locked(true);
                eprintln!(
                    "[client] joined as entity {entity_id}: rd {render_distance}, spawn {s:?}"
                );
            }
            ServerPacket::Subchunk { cx, cz, sub, blocks } => {
                if blocks.len() != ssc_world::SUBCHUNK_VOLUME {
                    eprintln!("[client] bad subchunk size, dropping");
                    return Flow::Exit;
                }
                if blocks.iter().any(|&b| !valid_id(b)) {
                    eprintln!("[client] invalid block id in subchunk, dropping");
                    return Flow::Exit;
                }
                let mut w = self.shared.write().unwrap_or_else(|e| e.into_inner());
                w.apply_subchunk(cx, cz, sub, &blocks);
            }
            ServerPacket::ColumnEnd { cx, cz } => {
                let mut w = self.shared.write().unwrap_or_else(|e| e.into_inner());
                w.apply_column_end(cx, cz);
            }
            ServerPacket::UnloadColumn { cx, cz } => {
                {
                    let mut w = self.shared.write().unwrap_or_else(|e| e.into_inner());
                    w.apply_unload(cx, cz);
                }
                // Drop GPU meshes for that column.
                let stale: Vec<((ColumnKey, usize, bool), MeshId)> = self
                    .sub_meshes
                    .iter()
                    .filter(|(k, _)| k.0 == (cx, cz))
                    .map(|(k, (id, _))| (*k, *id))
                    .collect();
                for (k, id) in stale {
                    self.sub_meshes.remove(&k);
                    frame.remove_mesh(id);
                }
                // Drop its occlusion masks too.
                for sub in 0..ssc_world::SUBCHUNKS_PER_COLUMN {
                    self.face_masks.remove(&((cx, cz), sub));
                }
            }
            ServerPacket::LodColumn { cx, cz, cells } => {
                let mut w = self.shared.write().unwrap_or_else(|e| e.into_inner());
                w.apply_lod(cx, cz, &cells);
            }
            ServerPacket::UnloadLod { cx, cz } => {
                let mut w = self.shared.write().unwrap_or_else(|e| e.into_inner());
                w.apply_lod_unload(cx, cz);
            }
            ServerPacket::EntityState { id, x, y, z, yaw, pitch } => {
                if Some(id) == self.own_id {
                    // Own authoritative state: interpolate for smooth
                    // rendering (the server owns the logic, specs.md §2).
                    self.pos_prev = self.pos_cur;
                    self.pos_cur = Vec3::new(x, y, z);
                    self.state_time = Instant::now();
                    let _ = (yaw, pitch);
                } else {
                    match self.entities.get_mut(&id) {
                        Some(e) => {
                            e.prev = e.cur;
                            e.cur = Vec3::new(x, y, z);
                            e.time = Instant::now();
                            e.yaw = yaw;
                        }
                        None => {
                            // First sighting: upload a placeholder avatar.
                            let mesh_id = self.next_mesh_id;
                            self.next_mesh_id += 1;
                            if let Err(e) = frame.upload_mesh(
                                mesh_id,
                                &self.avatar_verts,
                                &self.avatar_indices,
                            ) {
                                eprintln!("[client] avatar upload failed: {e}");
                            }
                            let p = Vec3::new(x, y, z);
                            self.entities.insert(
                                id,
                                EntityView {
                                    prev: p,
                                    cur: p,
                                    time: Instant::now(),
                                    yaw,
                                    mesh: mesh_id,
                                },
                            );
                        }
                    }
                }
            }
            ServerPacket::EntityRemove { id } => {
                if let Some(e) = self.entities.remove(&id) {
                    frame.remove_mesh(e.mesh);
                }
            }
            ServerPacket::BlockUpdate { x, y, z, block } => {
                if !valid_id(block) {
                    return Flow::Continue;
                }
                let mut w = self.shared.write().unwrap_or_else(|e| e.into_inner());
                w.apply_block_update(x, y, z, block);
            }
            ServerPacket::Sound { sound, x, y, z } => {
                let id = SoundId::from_u16(sound);
                // Audio playback awaits the audio dependency approval
                // (specs.md §4); the trigger itself is server-driven and
                // already flows end to end.
                println!(
                    "[sound] {} at ({x:.1}, {y:.1}, {z:.1})",
                    id.name()
                );
            }
            ServerPacket::Goodbye { reason } => {
                eprintln!("[client] server said goodbye: {reason}");
                return Flow::Exit;
            }
        }
        Flow::Continue
    }

    fn gc_stale_jobs(&mut self) {
        // Queued entries whose jobs were never picked up block re-queues.
        // (Jobs are drained by the worker immediately, so entries are
        // removed on result; anything ancient is a bug guard.)
        let cutoff = Duration::from_secs(5);
        self.queued_cols.retain(|_k, t| Instant::now() - *t < cutoff);
        self.queued_regions.retain(|_k, t| Instant::now() - *t < cutoff);
    }

    fn pump_mesh_jobs(&mut self, _frame: &mut Frame) {
        let (cols, regions) = {
            let mut w = self.shared.write().unwrap_or_else(|e| e.into_inner());
            let cols = w.take_dirty_columns();
            let regions = w.take_dirty_regions();
            (cols, regions)
        };

        for key in cols {
            // (Re)queue — uploads overwrite existing mesh ids, so a column
            // already on screen can simply be re-meshed.
            if !self.queued_cols.contains_key(&key) {
                self.queued_cols.insert(key, Instant::now());
                let _ = self.job_tx.send(Job::Column(key));
            }
        }
        let pcx = (self.pos_cur.x as i32).div_euclid(16);
        let pcz = (self.pos_cur.z as i32).div_euclid(16);
        let near_margin = (self.render_distance - NEAR_MARGIN_TRIM).max(0);
        for (rx, rz) in regions {
            if !self.queued_regions.contains_key(&(rx, rz)) {
                self.queued_regions.insert((rx, rz), Instant::now());
                let _ = self.job_tx.send(Job::Region { rx, rz, pcx, pcz, near_margin });
            }
        }
    }

    fn upload_finished_meshes(&mut self, frame: &mut Frame) {
        while let Ok(res) = self.job_rx.try_recv() {
            match res {
                JobResult::Column { key, meshes, masks } => {
                    self.queued_cols.remove(&key);
                    // Store occlusion masks for every sub-chunk (including
                    // all-air ones — they are pass-through nodes).
                    for (sub, m) in masks {
                        self.face_masks.insert((key, sub), m);
                    }
                    // Upload every produced mesh (solid + water separately).
                    let mut produced: HashSet<(usize, bool)> = HashSet::new();
                    for m in meshes {
                        produced.insert((m.sub, m.translucent));
                        let mesh_key = (key, m.sub, m.translucent);
                        let id = match self.sub_meshes.get(&mesh_key) {
                            Some((id, _)) => *id,
                            None => {
                                let id = self.next_mesh_id;
                                self.next_mesh_id += 1;
                                id
                            }
                        };
                        let aabb = ssc_math::Aabb::new(m.min, m.max);
                        if let Err(e) = frame.upload_mesh(id, &m.verts, &m.indices) {
                            eprintln!("[client] mesh upload failed: {e}");
                        } else {
                            self.sub_meshes.insert(mesh_key, (id, aabb));
                        }
                    }
                    // Remove stale meshes emptied by edits (same sub, same
                    // pass, no longer produced).
                    let stale: Vec<((ColumnKey, usize, bool), MeshId)> = self
                        .sub_meshes
                        .iter()
                        .filter(|(k, _)| k.0 == key && !produced.contains(&(k.1, k.2)))
                        .map(|(k, (id, _))| (*k, *id))
                        .collect();
                    for (k, id) in stale {
                        self.sub_meshes.remove(&k);
                        frame.remove_mesh(id);
                    }
                }
                JobResult::Region { rx, rz, mesh } => {
                    self.queued_regions.remove(&(rx, rz));
                    match mesh {
                        Some(m) => {
                            let span = (ssc_lod::REGION_COLS * 16) as f32;
                            let aabb = ssc_math::Aabb::new(
                                Vec3::new(rx as f32 * span, m.min_y, rz as f32 * span),
                                Vec3::new(
                                    rx as f32 * span + span,
                                    m.max_y,
                                    rz as f32 * span + span,
                                ),
                            );
                            let id = match self.region_meshes.get(&(rx, rz)) {
                                Some((id, _)) => *id,
                                None => {
                                    let id = self.next_mesh_id;
                                    self.next_mesh_id += 1;
                                    id
                                }
                            };
                            let verts: Vec<ssc_engine::Vertex> = m
                                .verts
                                .iter()
                                .map(|v| ssc_engine::Vertex { pos: [v[0], v[1], v[2]], uv: [v[3], v[4]] })
                                .collect();
                            if let Err(e) = frame.upload_mesh(id, &verts, &m.indices) {
                                eprintln!("[client] lod upload failed: {e}");
                            } else {
                                self.region_meshes.insert((rx, rz), (id, aabb));
                            }
                        }
                        None => {
                            if let Some((id, _)) = self.region_meshes.remove(&(rx, rz)) {
                                frame.remove_mesh(id);
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let mut connect: Option<String> = None;
    let mut textures: Option<std::path::PathBuf> = None;
    let mut cfg = ServerConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--connect" => {
                if let Some(v) = args.next() {
                    connect = Some(v);
                }
            }
            "--textures" => {
                if let Some(v) = args.next() {
                    textures = Some(std::path::PathBuf::from(v));
                }
            }
            "--seed" => {
                if let Some(v) = args.next() {
                    if let Ok(seed) = v.parse::<u64>() {
                        cfg.seed = seed;
                    }
                }
            }
            "--rd" => {
                if let Some(v) = args.next() {
                    if let Ok(rd) = v.parse::<i32>() {
                        cfg.render_distance = rd.clamp(2, 16);
                    }
                }
            }
            other => eprintln!("unknown argument {other:?}"),
        }
    }

    // Connect: dedicated over TCP, or embed the server locally.
    let wire: Box<dyn Wire> = match &connect {
        Some(addr) => {
            eprintln!("[client] connecting to {addr} …");
            Box::new(TcpWire::connect(addr).unwrap_or_else(|e| {
                eprintln!("cannot connect to {addr}: {e}");
                std::process::exit(1);
            }))
        }
        None => {
            eprintln!(
                "[client] starting local server (seed {}, rd {})",
                cfg.seed, cfg.render_distance
            );
            let (client_wire, _server_thread) = spawn_local(cfg);
            Box::new(client_wire)
        }
    };

    let shared = Arc::new(RwLock::new(ClientWorld::new()));
    let (job_tx, job_rx) = spawn_worker(shared.clone());

    let mut app = App::new(wire, shared, job_tx, job_rx, textures);

    let settings = EngineSettings {
        title: "Super Simple Cubes".into(),
        width: 1280,
        height: 720,
        vsync: false,
    };

    let code = ssc_engine::run(settings, move |frame, events| app.tick(frame, events));
    if let Err(e) = code {
        eprintln!("[client] engine error: {e}");
        std::process::exit(1);
    }
    eprintln!("[client] bye");
}
