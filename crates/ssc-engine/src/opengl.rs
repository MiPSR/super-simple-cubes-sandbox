//! OpenGL backend: winit (window) + glutin (context) + glow (calls).
//!
//! Owner-approved dependency stack (`specs.md` §4). Renders textured cubes
//! with a plain MVP + texture shader — no lighting, no effects (`specs.md`
//! §3). One draw call per submitted mesh; the engine facade has already
//! frustum-culled and sorted the list.

#![allow(unsafe_code)] // GFX boundary

use std::collections::HashMap;
use std::num::NonZeroU32;

use glow::HasContext;
use glutin::context::PossiblyCurrentContext;
use glutin::prelude::*;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::Window;

use crate::{DrawCmd, DrawStats, Mat4, MeshId, Renderer, Vertex};

type Gl = glow::Context;

/// The embedded HUD font: Monocraft (SIL Open Font License 1.1,
/// https://github.com/IdreesInc/Monocraft) — a pixel font with
/// Minecraft-style aesthetics. OFL permits redistribution; see
/// `assets/fonts/Monocraft-OFL.txt`.
#[cfg(feature = "text")]
const MONOCRAFT_TTF: &[u8] = include_bytes!("../assets/fonts/Monocraft.ttf");

const VS_SRC: &str = r#"
#version 330 core
layout(location = 0) in vec3 a_pos;
layout(location = 1) in vec2 a_uv;
layout(location = 2) in vec2 a_tile;
uniform mat4 u_mvp;
out vec2 v_uv;
out vec2 v_tile;
void main() {
    gl_Position = u_mvp * vec4(a_pos, 1.0);
    v_uv = a_uv;
    v_tile = a_tile;
}
"#;

const FS_SRC: &str = r#"
#version 330 core
in vec2 v_uv;
in vec2 v_tile;
out vec4 frag;
uniform sampler2D u_atlas;

// Atlas-aware texture tiling: local (repeating) UVs are wrapped per
// block unit and mapped into the block's tile with a half-texel inset.
// This is what lets greedy-merged quads keep per-block texture repeat.
void main() {
    vec2 tile_size = vec2(1.0 / 16.0);
    // Half-texel inset in tile units (0.5 px of a 16 px tile).
    vec2 inset = vec2(1.0 / 32.0);
    vec2 local = clamp(fract(v_uv), inset, 1.0 - inset);
    frag = texture(u_atlas, v_tile + local * tile_size);
}
"#;

const HUD_VS_SRC: &str = r#"
#version 330 core
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
layout(location = 2) in vec4 a_color;
uniform vec2 u_screen;
out vec2 v_uv;
out vec4 v_color;
void main() {
    vec2 clip = vec2(a_pos.x / u_screen.x * 2.0 - 1.0,
                     1.0 - a_pos.y / u_screen.y * 2.0);
    gl_Position = vec4(clip, 0.0, 1.0);
    v_uv = a_uv;
    v_color = a_color;
}
"#;

const HUD_FS_SRC: &str = r#"
#version 330 core
in vec2 v_uv;
in vec4 v_color;
out vec4 frag;
uniform sampler2D u_glyphs;
void main() {
    float a = texture(u_glyphs, v_uv).r;
    frag = vec4(v_color.rgb, v_color.a * a);
}
"#;

/// Sky color until world visuals land.
const SKY: (f32, f32, f32, f32) = (0.55, 0.75, 0.95, 1.0);

pub struct Backend {
    #[allow(dead_code)] // keeps the EGL/GLX display alive for the context.
    display: glutin::display::Display,
    context: PossiblyCurrentContext,
    surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    gl: Gl,
    program: <Gl as HasContext>::Program,
    loc_mvp: Option<<Gl as HasContext>::UniformLocation>,
    atlas: Option<<Gl as HasContext>::Texture>,
    meshes: HashMap<MeshId, Mesh>,
    width: u32,
    height: u32,
    /// HUD text layer (feature `text`).
    text: Option<TextLayer>,
}

/// Per-glyph HUD vertex (interleaved): pixel pos, glyph atlas uv, color.
#[repr(C)]
#[derive(Clone, Copy)]
struct HudVert {
    pos: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

/// Glyph-brush backed HUD text pipeline.
struct TextLayer {
    brush: glyph_brush::GlyphBrush<[HudVert; 4]>,
    program: <Gl as HasContext>::Program,
    loc_screen: Option<<Gl as HasContext>::UniformLocation>,
    glyph_tex: Option<<Gl as HasContext>::Texture>,
    vao: <Gl as HasContext>::VertexArray,
    vbo: <Gl as HasContext>::Buffer,
    ebo: <Gl as HasContext>::Buffer,
    verts: Vec<HudVert>,
    indices: Vec<u32>,
}

struct Mesh {
    vao: <Gl as HasContext>::VertexArray,
    vbo: <Gl as HasContext>::Buffer,
    ebo: <Gl as HasContext>::Buffer,
    count: i32,
}

impl Renderer for Backend {
    fn new(window: &Window, size: (u32, u32)) -> Result<Self, String> {
        // Connect to the platform GL display (EGL/GLX) from the window's
        // raw handles.
        let raw_display = window
            .display_handle()
            .map_err(|e| format!("display handle: {e}"))?
            .as_raw();
        let display = unsafe {
            glutin::display::Display::new(
                raw_display,
                glutin::display::DisplayApiPreference::GlxThenEgl(Box::new(|_hook| {
                    // No custom X11 error pump to register.
                })),
            )
            .map_err(|e| format!("gl display: {e}"))?
        };

        let template = glutin::config::ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .build();
        let config = {
            let mut configs = unsafe { display.find_configs(template) }
                .map_err(|e| format!("gl config: {e}"))?;
            configs
                .next()
                .ok_or("no GL config found for this window")?
        };

        let context = unsafe {
            display.create_context(
                &config,
                &glutin::context::ContextAttributesBuilder::new().build(None),
            )
        }
        .map_err(|e| format!("gl context: {e}"))?;

        let raw_window = window
            .window_handle()
            .map_err(|e| format!("window handle: {e}"))?
            .as_raw();
        let (nw, nh) = (NonZeroU32::new(size.0), NonZeroU32::new(size.1));
        let attrs =
            glutin::surface::SurfaceAttributesBuilder::<glutin::surface::WindowSurface>::new().build(
                raw_window,
                nw.unwrap_or(NonZeroU32::new(1).unwrap()),
                nh.unwrap_or(NonZeroU32::new(1).unwrap()),
            );
        let surface = unsafe { display.create_window_surface(&config, &attrs) }
            .map_err(|e| format!("gl surface: {e}"))?;

        let context = context
            .make_current(&surface)
            .map_err(|e| format!("make_current: {e}"))?;
        surface
            .set_swap_interval(&context, glutin::surface::SwapInterval::DontWait)
            .map_err(|e| format!("swap interval: {e}"))?;

        let gl = unsafe {
            Gl::from_loader_function(|name: &str| {
                let c = std::ffi::CString::new(name).expect("gl proc name");
                display.get_proc_address(&c) as *const core::ffi::c_void
            })
        };

        let program = unsafe { build_program(&gl)? };
        let loc_mvp = unsafe { gl.get_uniform_location(program, "u_mvp") };

        unsafe {
            gl.enable(glow::DEPTH_TEST);
            gl.enable(glow::CULL_FACE); // back-face culling: cube faces are CCW
            gl.cull_face(glow::BACK);
            gl.front_face(glow::CCW);
        }

        eprintln!(
            "[engine/opengl] context ready: {}",
            unsafe { gl.get_parameter_string(glow::VERSION) }
        );

        // HUD text layer (no-op without the `text` feature).
        let text = TextLayer::new(&gl)
            .map_err(|e| {
                eprintln!("[engine/opengl] HUD text unavailable: {e}");
            })
            .ok();

        Ok(Self {
            display,
            context,
            surface,
            gl,
            program,
            loc_mvp,
            atlas: None,
            meshes: HashMap::new(),
            width: size.0,
            height: size.1,
            text,
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        let (w, h) = (
            NonZeroU32::new(width.max(1)),
            NonZeroU32::new(height.max(1)),
        );
        if let (Some(w), Some(h)) = (w, h) {
            self.surface.resize(&self.context, w, h);
        }
    }

    fn upload_atlas(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<(), String> {
        unsafe {
            let gl = &self.gl;
            let tex = match self.atlas {
                Some(t) => t,
                None => {
                    let t = gl.create_texture().map_err(|e| format!("texture: {e}"))?;
                    self.atlas = Some(t);
                    t
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(rgba)),
            );
        }
        Ok(())
    }

    fn upload_mesh(&mut self, id: MeshId, verts: &[Vertex], indices: &[u32]) -> Result<(), String> {
        self.remove_mesh(id);
        unsafe {
            let gl = &self.gl;
            let vao = gl.create_vertex_array().map_err(|e| format!("vao: {e}"))?;
            let vbo = gl.create_buffer().map_err(|e| format!("vbo: {e}"))?;
            let ebo = gl.create_buffer().map_err(|e| format!("ebo: {e}"))?;

            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let vert_bytes = core::slice::from_raw_parts(
                verts.as_ptr() as *const u8,
                verts.len() * core::mem::size_of::<Vertex>(),
            );
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, vert_bytes, glow::STATIC_DRAW);

            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ebo));
            let idx_bytes = core::slice::from_raw_parts(
                indices.as_ptr() as *const u8,
                indices.len() * core::mem::size_of::<u32>(),
            );
            gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, idx_bytes, glow::STATIC_DRAW);

            const STRIDE: i32 = 28; // pos(3f) + uv(2f) + tile(2f)
            // position: location 0, 3 floats
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 3, glow::FLOAT, false, STRIDE, 0);
            // local uv: location 1, 2 floats
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, STRIDE, 12);
            // atlas tile origin: location 2, 2 floats
            gl.enable_vertex_attrib_array(2);
            gl.vertex_attrib_pointer_f32(2, 2, glow::FLOAT, false, STRIDE, 20);

            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            self.meshes.insert(
                id,
                Mesh { vao, vbo, ebo, count: indices.len() as i32 },
            );
        }
        Ok(())
    }

    fn remove_mesh(&mut self, id: MeshId) {
        if let Some(m) = self.meshes.remove(&id) {
            unsafe {
                let gl = &self.gl;
                gl.delete_vertex_array(m.vao);
                gl.delete_buffer(m.vbo);
                gl.delete_buffer(m.ebo);
            }
        }
    }

    fn render(
        &mut self,
        view_proj: &Mat4,
        opaque: &[&DrawCmd],
        translucent: &[&DrawCmd],
    ) -> DrawStats {
        let gl = &self.gl;
        let mut draw_calls = 0u32;
        unsafe {
            gl.viewport(0, 0, self.width as i32, self.height as i32);
            gl.clear_color(SKY.0, SKY.1, SKY.2, SKY.3);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);

            gl.use_program(Some(self.program));
            gl.uniform_matrix_4_f32_slice(
                self.loc_mvp.as_ref(),
                false,
                view_proj.to_cols_array().as_slice(),
            );
            if let Some(tex) = self.atlas {
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            }

            let draw_all = |list: &[&DrawCmd], gl: &Gl| {
                let mut calls = 0u32;
                let mut last_model: Option<Mat4> = None;
                for d in list {
                    let Some(m) = self.meshes.get(&d.mesh) else { continue };
                    // Model matrix: set the composed MVP only when it
                    // changes (terrain runs identity, entities move).
                    if last_model != Some(d.model) {
                        let mvp = *view_proj * d.model;
                        gl.uniform_matrix_4_f32_slice(
                            self.loc_mvp.as_ref(),
                            false,
                            mvp.to_cols_array().as_slice(),
                        );
                        last_model = Some(d.model);
                    }
                    gl.bind_vertex_array(Some(m.vao));
                    gl.draw_elements(glow::TRIANGLES, m.count, glow::UNSIGNED_INT, 0);
                    calls += 1;
                }
                gl.bind_vertex_array(None);
                calls
            };

            // Opaque pass: depth writes on.
            draw_calls += draw_all(opaque, gl);

            // Translucent pass: blend, no depth writes.
            if !translucent.is_empty() {
                gl.enable(glow::BLEND);
                gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                gl.depth_mask(false);
                draw_calls += draw_all(translucent, gl);
                gl.depth_mask(true);
                gl.disable(glow::BLEND);
            }
        }
        self.surface
            .swap_buffers(&self.context)
            .map_err(|e| format!("swap: {e}"))
            .err()
            .map(|e| eprintln!("[engine/opengl] {e}"));
        DrawStats { draw_calls }
    }

    fn render_hud(&mut self, texts: &[crate::HudText], width: u32, height: u32) {
        let Some(layer) = &mut self.text else { return };
        layer.draw(&self.gl, texts, width, height);
    }

    fn present(&mut self) {
        let _ = self.surface.swap_buffers(&self.context);
    }

    fn backend_name(&self) -> &'static str {
        "opengl"
    }
}

// ---------------------------------------------------------------------------
// HUD text pipeline (feature `text`): glyph_brush + Monocraft.
// ---------------------------------------------------------------------------

impl TextLayer {
    fn new(gl: &Gl) -> Result<Self, String> {
        #[cfg(not(feature = "text"))]
        {
            let _ = gl;
            return Err("engine built without the `text` feature".into());
        }
        #[cfg(feature = "text")]
        {
            use glyph_brush::ab_glyph::FontArc;

            let font =
                FontArc::try_from_slice(MONOCRAFT_TTF).map_err(|e| format!("font: {e}"))?;
            let brush = glyph_brush::GlyphBrushBuilder::using_font(font).build();

            let program = unsafe { build_hud_program(gl)? };
            let loc_screen = unsafe { gl.get_uniform_location(program, "u_screen") };
            let (vao, vbo, ebo) = unsafe {
                let vao = gl.create_vertex_array().map_err(|e| format!("vao: {e}"))?;
                let vbo = gl.create_buffer().map_err(|e| format!("vbo: {e}"))?;
                let ebo = gl.create_buffer().map_err(|e| format!("ebo: {e}"))?;
                gl.bind_vertex_array(Some(vao));
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ebo));
                const STRIDE: i32 = 32; // pos(2f) + uv(2f) + color(4f)
                gl.enable_vertex_attrib_array(0);
                gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, STRIDE, 0);
                gl.enable_vertex_attrib_array(1);
                gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, STRIDE, 8);
                gl.enable_vertex_attrib_array(2);
                gl.vertex_attrib_pointer_f32(2, 4, glow::FLOAT, false, STRIDE, 16);
                gl.bind_vertex_array(None);
                (vao, vbo, ebo)
            };
            Ok(Self {
                brush,
                program,
                loc_screen,
                glyph_tex: None,
                vao,
                vbo,
                ebo,
                verts: Vec::new(),
                indices: Vec::new(),
            })
        }
    }

    fn draw(
        &mut self,
        gl: &Gl,
        texts: &[crate::HudText],
        width: u32,
        height: u32,
    ) {
        #[cfg(not(feature = "text"))]
        {
            let _ = (gl, texts, width, height);
        }
        #[cfg(feature = "text")]
        {
            use glyph_brush::{BrushAction, GlyphVertex, Section, Text};

            if texts.is_empty() {
                return;
            }

            // Destructure so the glyph-atlas upload closure can borrow the
            // texture slot while the brush is borrowed mutably.
            let Self { brush, glyph_tex, verts, indices, .. } = self;

            for t in texts {
                brush.queue(
                    Section::default()
                        .with_screen_position((t.x, t.y))
                        .add_text(
                            Text::new(&t.text)
                                .with_scale(t.scale)
                                .with_color(t.color),
                        ),
                );
            }

            let (aw, ah) = brush.texture_dimensions();


            let to_vertex = |v: GlyphVertex| -> [HudVert; 4] {
                let r = v.pixel_coords;
                // NOTE: tex_coords are ALREADY normalized atlas UVs
                // (0..1) — do not divide by the atlas dimensions again.
                let uv0 = [v.tex_coords.min.x, v.tex_coords.min.y];
                let uv1 = [v.tex_coords.max.x, v.tex_coords.max.y];
                let c = [
                    v.extra.color[0],
                    v.extra.color[1],
                    v.extra.color[2],
                    v.extra.color[3],
                ];
                [
                    HudVert { pos: [r.min.x, r.min.y], uv: [uv0[0], uv0[1]], color: c },
                    HudVert { pos: [r.max.x, r.min.y], uv: [uv1[0], uv0[1]], color: c },
                    HudVert { pos: [r.max.x, r.max.y], uv: [uv1[0], uv1[1]], color: c },
                    HudVert { pos: [r.min.x, r.max.y], uv: [uv0[0], uv1[1]], color: c },
                ]
            };

            let update_texture = |rect: glyph_brush::Rectangle<u32>, data: &[u8]| {
                unsafe {
                    // Lazily create the glyph atlas texture at its full size.
                    let tex = match *glyph_tex {
                        Some(t) => t,
                        None => {
                            let t = gl
                                .create_texture()
                                .expect("glyph atlas texture");
                            gl.bind_texture(glow::TEXTURE_2D, Some(t));
                            gl.tex_parameter_i32(
                                glow::TEXTURE_2D,
                                glow::TEXTURE_MIN_FILTER,
                                glow::NEAREST as i32,
                            );
                            gl.tex_parameter_i32(
                                glow::TEXTURE_2D,
                                glow::TEXTURE_MAG_FILTER,
                                glow::NEAREST as i32,
                            );
                            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                            gl.tex_image_2d(
                                glow::TEXTURE_2D,
                                0,
                                glow::RGBA as i32,
                                aw as i32,
                                ah as i32,
                                0,
                                glow::RGBA,
                                glow::UNSIGNED_BYTE,
                                glow::PixelUnpackData::Slice(None),
                            );
                            gl.tex_parameter_i32(
                                glow::TEXTURE_2D,
                                glow::TEXTURE_WRAP_S,
                                glow::CLAMP_TO_EDGE as i32,
                            );
                            gl.tex_parameter_i32(
                                glow::TEXTURE_2D,
                                glow::TEXTURE_WRAP_T,
                                glow::CLAMP_TO_EDGE as i32,
                            );
                            *glyph_tex = Some(t);
                            t
                        }
                    };
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                    // Expand the grayscale coverage to RGBA (some GL
                    // implementations dislike single-channel sampling).
                    let n = data.len();
                    let mut rgba = Vec::with_capacity(n * 4);
                    for &b in data {
                        rgba.extend_from_slice(&[b, b, b, 255]);
                    }
                    gl.tex_sub_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        rect.min[0] as i32,
                        rect.min[1] as i32,
                        rect.width() as i32,
                        rect.height() as i32,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(&rgba)),
                    );
                }
            };

            let action = brush.process_queued(update_texture, to_vertex);

            match action {
                Ok(BrushAction::Draw(quads)) => {
                    verts.clear();
                    indices.clear();
                    for (i, quad) in quads.iter().enumerate() {
                        let base = (i * 4) as u32;
                        verts.extend_from_slice(quad);
                        indices
                            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
                    }
                }
                Ok(BrushAction::ReDraw) => {}
                Err(e) => {
                    eprintln!("[engine/opengl] glyph layout error: {e:?}");
                    return;
                }
            }

            let Some(tex) = *glyph_tex else { return };
            unsafe {
                gl.use_program(Some(self.program));
                gl.uniform_2_f32(
                    self.loc_screen.as_ref(),
                    width as f32,
                    height as f32,
                );
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));

                gl.disable(glow::DEPTH_TEST);
                // Glyph quads wind clockwise in clip space (the projection
                // flips Y) — culling must be off for the HUD pass.
                gl.disable(glow::CULL_FACE);
                gl.enable(glow::BLEND);
                gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

                gl.bind_vertex_array(Some(self.vao));
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
                let vert_bytes = core::slice::from_raw_parts(
                    verts.as_ptr() as *const u8,
                    verts.len() * core::mem::size_of::<HudVert>(),
                );
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, vert_bytes, glow::DYNAMIC_DRAW);
                gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(self.ebo));
                let idx_bytes = core::slice::from_raw_parts(
                    indices.as_ptr() as *const u8,
                    indices.len() * core::mem::size_of::<u32>(),
                );
                gl.buffer_data_u8_slice(
                    glow::ELEMENT_ARRAY_BUFFER,
                    idx_bytes,
                    glow::DYNAMIC_DRAW,
                );
                gl.draw_elements(
                    glow::TRIANGLES,
                    indices.len() as i32,
                    glow::UNSIGNED_INT,
                    0,
                );

                if std::env::var("SSC_DEBUG").map(|v| v == "1").unwrap_or(false) {
                    let err = gl.get_error();
                    let mut px = [0u8; 4];
                    gl.read_pixels(
                        width as i32 / 2,
                        height as i32 / 2,
                        1,
                        1,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelPackData::Slice(Some(&mut px)),
                    );
                    eprintln!(
                        "[engine/text] gl_error={err:?} center={px:?} verts={} prog={:?}",
                        verts.len(),
                        gl.is_program(self.program)
                    );
                }
                gl.bind_vertex_array(None);

                gl.disable(glow::BLEND);
                gl.enable(glow::CULL_FACE);
                gl.enable(glow::DEPTH_TEST);
                gl.use_program(None);
            }
        }
    }
}

unsafe fn build_hud_program(gl: &Gl) -> Result<<Gl as HasContext>::Program, String> {
    let vs = compile_shader(gl, glow::VERTEX_SHADER, HUD_VS_SRC)?;
    let fs = compile_shader(gl, glow::FRAGMENT_SHADER, HUD_FS_SRC)?;
    let program = gl.create_program().map_err(|e| format!("program: {e}"))?;
    gl.attach_shader(program, vs);
    gl.attach_shader(program, fs);
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        let log = gl.get_program_info_log(program);
        return Err(format!("hud link failed: {log}"));
    }
    gl.delete_shader(vs);
    gl.delete_shader(fs);
    let loc = gl.get_uniform_location(program, "u_glyphs");
    gl.use_program(Some(program));
    gl.uniform_1_i32(loc.as_ref(), 0);
    gl.use_program(None);
    Ok(program)
}

impl Drop for Backend {
    fn drop(&mut self) {
        let gl = &self.gl;
        unsafe {
            for m in self.meshes.values() {
                gl.delete_vertex_array(m.vao);
                gl.delete_buffer(m.vbo);
                gl.delete_buffer(m.ebo);
            }
            if let Some(t) = self.atlas {
                gl.delete_texture(t);
            }
            gl.delete_program(self.program);
        }
    }
}

unsafe fn build_program(gl: &Gl) -> Result<<Gl as HasContext>::Program, String> {
    let vs = compile_shader(gl, glow::VERTEX_SHADER, VS_SRC)?;
    let fs = compile_shader(gl, glow::FRAGMENT_SHADER, FS_SRC)?;
    let program = gl.create_program().map_err(|e| format!("program: {e}"))?;
    gl.attach_shader(program, vs);
    gl.attach_shader(program, fs);
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        let log = gl.get_program_info_log(program);
        return Err(format!("link failed: {log}"));
    }
    gl.delete_shader(vs);
    gl.delete_shader(fs);
    // Atlas is always texture unit 0.
    let loc = gl.get_uniform_location(program, "u_atlas");
    gl.use_program(Some(program));
    gl.uniform_1_i32(loc.as_ref(), 0);
    gl.use_program(None);
    Ok(program)
}

unsafe fn compile_shader(
    gl: &Gl,
    kind: u32,
    src: &str,
) -> Result<<Gl as HasContext>::Shader, String> {
    let shader = gl.create_shader(kind).map_err(|e| format!("shader: {e}"))?;
    gl.shader_source(shader, src);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        let log = gl.get_shader_info_log(shader);
        gl.delete_shader(shader);
        return Err(format!("compile failed: {log}"));
    }
    Ok(shader)
}
