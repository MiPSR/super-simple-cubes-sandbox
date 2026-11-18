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

const VS_SRC: &str = r#"
#version 330 core
layout(location = 0) in vec3 a_pos;
layout(location = 1) in vec2 a_uv;
uniform mat4 u_mvp;
out vec2 v_uv;
void main() {
    gl_Position = u_mvp * vec4(a_pos, 1.0);
    v_uv = a_uv;
}
"#;

const FS_SRC: &str = r#"
#version 330 core
in vec2 v_uv;
out vec4 frag;
uniform sampler2D u_atlas;
void main() {
    frag = texture(u_atlas, v_uv);
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

            // position: location 0, 3 floats
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 3, glow::FLOAT, false, 20, 0);
            // uv: location 1, 2 floats
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 20, 12);

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

    fn render(&mut self, view_proj: &Mat4, draws: &[&DrawCmd]) -> DrawStats {
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

            for d in draws {
                let Some(m) = self.meshes.get(&d.mesh) else { continue };
                gl.bind_vertex_array(Some(m.vao));
                gl.draw_elements(glow::TRIANGLES, m.count, glow::UNSIGNED_INT, 0);
                draw_calls += 1;
            }
            gl.bind_vertex_array(None);
        }
        self.surface
            .swap_buffers(&self.context)
            .map_err(|e| format!("swap: {e}"))
            .err()
            .map(|e| eprintln!("[engine/opengl] {e}"));
        DrawStats { draw_calls }
    }

    fn backend_name(&self) -> &'static str {
        "opengl"
    }
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
