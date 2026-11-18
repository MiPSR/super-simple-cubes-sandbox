//! Super Simple Cubes — rendering engine.
//!
//! This crate is the **only** component of the project allowed to talk to
//! windowing/graphics APIs (`specs.md` §2, §3). The game client drives the
//! engine through [`Engine::run`] and the [`Renderer`] trait; backends are
//! selected at build time via Cargo features:
//!
//! - `opengl` (default) — winit + glutin + glow
//! - `vulkan` — planned (ash)
//!
//! Visual baseline per spec: textured cubes, no lighting, no shader effects
//! beyond a plain MVP + texture pass. Priorities: correct culling → proper
//! rendering → high framerate. Frustum culling of the submitted draw list
//! happens here, engine-side, so every client benefits from it.
//!
//! The engine exposes no winit types: clients see [`EngineEvent`],
//! [`InputState`] and [`Frame`].

#![deny(unsafe_code)] // backend modules relax this locally: they are the GFX boundary

#[cfg(feature = "opengl")]
pub mod opengl;

use std::collections::HashSet;
use std::time::Instant;

use ssc_math::{look_dir, Aabb, Frustum, Mat4, Vec3};

// ---------------------------------------------------------------------------
// Core rendering types (backend-agnostic)
// ---------------------------------------------------------------------------

/// A textured-cube vertex: position + atlas UV.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub uv: [f32; 2],
}

/// Client-assigned mesh handle.
pub type MeshId = u64;

/// The camera for a frame.
#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// Eye position in world space.
    pub pos: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    /// Vertical field of view in radians.
    pub fov_y: f32,
    pub z_near: f32,
    pub z_far: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            pos: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            fov_y: 70f32.to_radians(),
            z_near: 0.1,
            z_far: 1024.0,
        }
    }
}

/// One drawable mesh submission with its world-space bounding box.
#[derive(Debug, Clone, Copy)]
pub struct DrawCmd {
    pub mesh: MeshId,
    pub aabb: Aabb,
    /// LOD meshes are drawn after (behind) near geometry.
    pub lod: bool,
}

/// Per-frame render statistics.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameStats {
    /// Draw commands that survived culling.
    pub drawn: u32,
    /// Draw commands culled by the frustum.
    pub culled_frustum: u32,
    /// Backend draw calls issued.
    pub draw_calls: u32,
    /// Time spent inside `render` (ms), including present.
    pub frame_ms: f32,
}

/// Statistics returned by a backend's render pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct DrawStats {
    pub draw_calls: u32,
}

/// The backend contract. Implementations live in backend modules and are
/// created through [`Engine::run`]; clients never touch backends directly.
pub trait Renderer {
    fn new(window: &winit::window::Window, size: (u32, u32)) -> Result<Self, String>
    where
        Self: Sized;
    fn resize(&mut self, width: u32, height: u32);
    /// Upload (or replace) a texture atlas: raw RGBA, row 0 at the bottom
    /// (GL convention).
    fn upload_atlas(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<(), String>;
    /// Upload (or replace) a mesh.
    fn upload_mesh(&mut self, id: MeshId, verts: &[Vertex], indices: &[u32]) -> Result<(), String>;
    fn remove_mesh(&mut self, id: MeshId);
    /// Draw the given (already culled) commands.
    fn render(&mut self, view_proj: &Mat4, draws: &[&DrawCmd]) -> DrawStats;
    fn backend_name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// Input abstraction (no winit types leak to the client)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    W,
    A,
    S,
    D,
    Space,
    Shift,
    Ctrl,
    Escape,
    Tab,
    E,
    Q,
    R,
    F,
    T,
    G,
    Digit(u8),
    F1,
    F2,
    F3,
    F4,
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Pressed,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy)]
pub enum EngineEvent {
    /// The window wants to close.
    Closed,
    Focused(bool),
    Key { key: Key, state: KeyState },
    MouseButton { button: MouseButton, state: KeyState },
    Resized { width: u32, height: u32 },
}

/// Input snapshot for the current frame.
#[derive(Debug, Default)]
pub struct InputState {
    pub keys: HashSet<Key>,
    /// Accumulated raw mouse motion since the last frame.
    pub mouse_dx: f32,
    pub mouse_dy: f32,
    pub mouse_left: bool,
    pub mouse_right: bool,
}

impl InputState {
    pub fn key(&self, k: Key) -> bool {
        self.keys.contains(&k)
    }
}

// ---------------------------------------------------------------------------
// Engine facade
// ---------------------------------------------------------------------------

/// Window/loop configuration.
#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub vsync: bool,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            title: "Super Simple Cubes".into(),
            width: 1280,
            height: 720,
            vsync: false,
        }
    }
}

/// What the client frame callback wants to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Exit,
}

/// Per-frame view over the engine, handed to the client callback.
pub struct Frame<'a> {
    renderer: &'a mut dyn Renderer,
    window: &'a winit::window::Window,
    input: &'a InputState,
    mouse_locked: &'a mut bool,
    size: (u32, u32),
}

impl Frame<'_> {
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn input(&self) -> &InputState {
        self.input
    }

    pub fn mouse_locked(&self) -> bool {
        *self.mouse_locked
    }

    /// Grab (or release) the mouse. Locking hides the cursor and delivers
    /// relative mouse motion.
    pub fn set_mouse_locked(&mut self, locked: bool) {
        if locked == *self.mouse_locked {
            return;
        }
        use winit::window::CursorGrabMode;
        let mode = if locked { CursorGrabMode::Locked } else { CursorGrabMode::None };
        if self.window.set_cursor_grab(mode).is_err() && locked {
            // Some platforms can't lock; confined is better than nothing.
            let _ = self.window.set_cursor_grab(CursorGrabMode::Confined);
        }
        self.window.set_cursor_visible(!locked);
        *self.mouse_locked = locked;
    }

    pub fn upload_atlas(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<(), String> {
        self.renderer.upload_atlas(rgba, width, height)
    }

    pub fn upload_mesh(&mut self, id: MeshId, verts: &[Vertex], indices: &[u32]) -> Result<(), String> {
        self.renderer.upload_mesh(id, verts, indices)
    }

    pub fn remove_mesh(&mut self, id: MeshId) {
        self.renderer.remove_mesh(id)
    }

    /// Build the view-projection, frustum-cull + sort the draw list, render
    /// and present. This is the culling hot path (`specs.md` §3).
    pub fn render(&mut self, camera: &Camera, draws: &[DrawCmd]) -> FrameStats {
        let t0 = Instant::now();
        let (w, h) = self.size;
        let aspect = w as f32 / h.max(1) as f32;
        let dir = look_dir(camera.yaw, camera.pitch);
        let view = Mat4::look_to_rh(camera.pos, dir, Vec3::Y);
        // GL expects NDC z in [-1, 1] — the `_gl` variant, not the wgpu/D3D
        // [0, 1] `perspective_rh` (which far-clips everything against +1).
        let proj = Mat4::perspective_rh_gl(camera.fov_y, aspect, camera.z_near, camera.z_far);
        let vp = proj * view;
        let frustum = Frustum::from_view_proj(&vp);
        let cull_enabled = std::env::var("SSC_NOCULL").map(|v| v != "1").unwrap_or(true);

        let mut near: Vec<(f32, &DrawCmd)> = Vec::new();
        let mut lod: Vec<(f32, &DrawCmd)> = Vec::new();
        let mut culled = 0u32;
        for d in draws {
            if cull_enabled && !frustum.intersects_aabb(&d.aabb) {
                culled += 1;
                continue;
            }
            let dist2 = d.aabb.distance_squared_to_point(camera.pos);
            if d.lod {
                lod.push((dist2, d));
            } else {
                near.push((dist2, d));
            }
        }
        // Front-to-back within the near pass (early-z friendly), LOD behind.
        near.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        lod.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut list: Vec<&DrawCmd> = near.into_iter().map(|(_, d)| d).collect();
        list.extend(lod.into_iter().map(|(_, d)| d));

        let stats = self.renderer.render(&vp, &list);
        let frame_ms = t0.elapsed().as_secs_f32() * 1000.0;

        // Debug: dump projected mesh positions once per second.
        if std::env::var("SSC_PROJDUMP").map(|v| v == "1").unwrap_or(false) {
            let mut n = 0;
            for d in list.iter().take(12) {
                let c = d.aabb.center().extend(1.0);
                let clip = vp * c;
                if clip.w > 0.0 {
                    let ndc = clip.truncate() / clip.w;
                    eprintln!(
                        "[projdump] mesh {} lod={} aabb {:?} center {:?} -> ndc ({:.2},{:.2},{:.2})",
                        d.mesh, d.lod, d.aabb, d.aabb.center(), ndc.x, ndc.y, ndc.z
                    );
                }
                n += 1;
                let _ = n;
            }
        }

        FrameStats {
            drawn: list.len() as u32,
            culled_frustum: culled,
            draw_calls: stats.draw_calls,
            frame_ms,
        }
    }
}

/// Run the engine until the client callback returns [`Flow::Exit`].
///
/// The callback receives a [`Frame`] (renderer access, input, mouse control)
/// and the events collected since the last frame. It is called once per
/// frame on the main thread.
pub fn run<F>(settings: EngineSettings, frame_fn: F) -> Result<(), String>
where
    F: FnMut(&mut Frame, &[EngineEvent]) -> Flow,
{
    #[cfg(not(any(feature = "opengl", feature = "vulkan")))]
    compile_error!("no graphics backend selected: enable the `opengl` or `vulkan` feature");
    #[cfg(all(feature = "opengl", feature = "vulkan"))]
    compile_error!("select a single graphics backend (opengl or vulkan), not both");

    use winit::application::ApplicationHandler;
    use winit::dpi::LogicalSize;
    use winit::event::{DeviceEvent, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::keyboard::{KeyCode, PhysicalKey};
    use winit::window::{Window, WindowId};

    struct App<F: FnMut(&mut Frame, &[EngineEvent]) -> Flow> {
        settings: EngineSettings,
        window: Option<Window>,
        renderer: Option<Box<dyn Renderer>>,
        size: (u32, u32),
        pending: Vec<EngineEvent>,
        input: InputState,
        mouse_locked: bool,
        init_error: Option<String>,
        frame_fn: F,
    }

    fn map_key(code: &KeyCode) -> Option<Key> {
        Some(match code {
            KeyCode::KeyW => Key::W,
            KeyCode::KeyA => Key::A,
            KeyCode::KeyS => Key::S,
            KeyCode::KeyD => Key::D,
            KeyCode::Space => Key::Space,
            KeyCode::ShiftLeft | KeyCode::ShiftRight => Key::Shift,
            KeyCode::ControlLeft | KeyCode::ControlRight => Key::Ctrl,
            KeyCode::Escape => Key::Escape,
            KeyCode::Tab => Key::Tab,
            KeyCode::KeyE => Key::E,
            KeyCode::KeyQ => Key::Q,
            KeyCode::KeyR => Key::R,
            KeyCode::KeyF => Key::F,
            KeyCode::KeyT => Key::T,
            KeyCode::KeyG => Key::G,
            KeyCode::Digit1 => Key::Digit(1),
            KeyCode::Digit2 => Key::Digit(2),
            KeyCode::Digit3 => Key::Digit(3),
            KeyCode::Digit4 => Key::Digit(4),
            KeyCode::Digit5 => Key::Digit(5),
            KeyCode::Digit6 => Key::Digit(6),
            KeyCode::Digit7 => Key::Digit(7),
            KeyCode::Digit8 => Key::Digit(8),
            KeyCode::Digit9 => Key::Digit(9),
            KeyCode::F1 => Key::F1,
            KeyCode::F2 => Key::F2,
            KeyCode::F3 => Key::F3,
            KeyCode::F4 => Key::F4,
            KeyCode::ArrowUp => Key::Up,
            KeyCode::ArrowDown => Key::Down,
            KeyCode::ArrowLeft => Key::Left,
            KeyCode::ArrowRight => Key::Right,
            _ => return Option::None,
        })
    }

    fn create_renderer(
        window: &Window,
        size: (u32, u32),
    ) -> Result<Box<dyn Renderer>, String> {
        #[cfg(feature = "opengl")]
        return Ok(Box::new(opengl::Backend::new(window, size)?));
        #[cfg(all(feature = "vulkan", not(feature = "opengl")))]
        {
            let _ = (window, size);
            return Err("vulkan backend not yet implemented (planned: ash)".into());
        }
    }

    impl<F: FnMut(&mut Frame, &[EngineEvent]) -> Flow> ApplicationHandler for App<F> {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let attrs = Window::default_attributes()
                .with_title(self.settings.title.clone())
                .with_inner_size(LogicalSize::new(self.settings.width, self.settings.height))
                .with_resizable(true);
            let window = match el.create_window(attrs) {
                Ok(w) => w,
                Err(e) => {
                    self.init_error = Some(format!("window creation failed: {e}"));
                    el.exit();
                    return;
                }
            };
            let size = window.inner_size();
            self.size = (size.width.max(1), size.height.max(1));
            match create_renderer(&window, self.size) {
                Ok(r) => {
                    eprintln!(
                        "[engine] backend: {} ({}x{})",
                        r.backend_name(),
                        self.size.0,
                        self.size.1
                    );
                    self.renderer = Some(r);
                }
                Err(e) => {
                    self.init_error = Some(e);
                    el.exit();
                    return;
                }
            }
            self.window = Some(window);
        }

        fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested => self.pending.push(EngineEvent::Closed),
                WindowEvent::Resized(size) => {
                    self.size = (size.width.max(1), size.height.max(1));
                    if let Some(r) = &mut self.renderer {
                        r.resize(self.size.0, self.size.1);
                    }
                    self.pending.push(EngineEvent::Resized {
                        width: self.size.0,
                        height: self.size.1,
                    });
                }
                WindowEvent::Focused(f) => {
                    self.pending.push(EngineEvent::Focused(f));
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    if let PhysicalKey::Code(code) = event.physical_key {
                        if let Some(key) = map_key(&code) {
                            let state = if event.state.is_pressed() {
                                KeyState::Pressed
                            } else {
                                KeyState::Released
                            };
                            match state {
                                KeyState::Pressed => self.input.keys.insert(key),
                                KeyState::Released => self.input.keys.remove(&key),
                            };
                            self.pending.push(EngineEvent::Key { key, state });
                        }
                    }
                }
                WindowEvent::MouseInput { button, state, .. } => {
                    let b = match button {
                        winit::event::MouseButton::Left => MouseButton::Left,
                        winit::event::MouseButton::Right => MouseButton::Right,
                        winit::event::MouseButton::Middle => MouseButton::Middle,
                        _ => return,
                    };
                    let pressed = state.is_pressed();
                    match b {
                        MouseButton::Left => self.input.mouse_left = pressed,
                        MouseButton::Right => self.input.mouse_right = pressed,
                        MouseButton::Middle => {}
                    }
                    self.pending.push(EngineEvent::MouseButton {
                        button: b,
                        state: if pressed { KeyState::Pressed } else { KeyState::Released },
                    });
                }
                _ => {}
            }
        }

        fn device_event(
            &mut self,
            _el: &ActiveEventLoop,
            _device_id: winit::event::DeviceId,
            event: DeviceEvent,
        ) {
            if let DeviceEvent::MouseMotion { delta } = event {
                self.input.mouse_dx += delta.0 as f32;
                self.input.mouse_dy += delta.1 as f32;
            }
        }

        fn about_to_wait(&mut self, el: &ActiveEventLoop) {
            let Some(renderer) = self.renderer.as_mut() else { return };
            let Some(window) = self.window.as_ref() else { return };

            let events = std::mem::take(&mut self.pending);
            let mut frame = Frame {
                renderer: renderer.as_mut(),
                window,
                input: &self.input,
                mouse_locked: &mut self.mouse_locked,
                size: self.size,
            };
            let flow = (self.frame_fn)(&mut frame, &events);
            // Deltas are per-frame; key/button state persists.
            self.input.mouse_dx = 0.0;
            self.input.mouse_dy = 0.0;

            if matches!(flow, Flow::Exit) {
                el.exit();
            }
        }
    }

    let event_loop = EventLoop::new().map_err(|e| format!("event loop: {e}"))?;
    event_loop.set_control_flow(ControlFlow::Poll); // high framerate (specs.md §3)
    let mut app = App {
        settings,
        window: None,
        renderer: None,
        size: (1, 1),
        pending: Vec::new(),
        input: InputState::default(),
        mouse_locked: false,
        init_error: None,
        frame_fn,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("event loop: {e}"))?;
    if let Some(err) = app.init_error {
        return Err(err);
    }
    Ok(())
}

/// Default vertical field of view (radians).
pub const DEFAULT_FOV_Y: f32 = 70f32.to_radians();

/// Pixels per atlas tile edge (the Minecraft texture convention).
pub const ATLAS_TILE_PX: u32 = 16;
/// Tiles per atlas edge this project's atlases use.
pub const ATLAS_TILES: u32 = 16;

/// UV rect `[u0, v0, u1, v1]` for a tile in a square atlas, GL convention
/// (v grows upward; image row 0 is the *top* of the image). Half-texel
/// inset prevents bleeding between neighboring tiles.
pub fn atlas_tile_uv(tile: u16) -> [f32; 4] {
    let n = ATLAS_TILES as f32;
    let tx = (tile % ATLAS_TILES as u16) as f32;
    let ty = (tile / ATLAS_TILES as u16) as f32;
    let half = 0.5 / ATLAS_TILE_PX as f32; // in tile units
    let u0 = (tx + half) / n;
    let u1 = (tx + 1.0 - half) / n;
    let v_top = 1.0 - (ty + half) / n;
    let v_bottom = 1.0 - (ty + 1.0 - half) / n;
    [u0, v_bottom, u1, v_top]
}
