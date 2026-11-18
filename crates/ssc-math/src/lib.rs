//! Super Simple Cubes — shared math.
//!
//! Math primitives shared by the engine, world, LOD, protocol and server.
//! `glam` provides the vector/matrix types (owner-approved dependency);
//! this crate adds the project-specific geometry: AABBs, frustum culling,
//! the player's physical constants and the shared look/orientation math.
//!
//! No gameplay *logic* lives here — only geometry and constants both sides
//! need so client and server agree on them (`specs.md` §2).

pub use glam::{Mat4, Quat, Vec2, Vec3, Vec4};

// ---------------------------------------------------------------------------
// Player physics constants (shared so client presentation and server
// simulation agree; Bedrock-ish values per specs.md §1).
// ---------------------------------------------------------------------------

/// Half width of the player AABB (0.6 blocks wide, like Minecraft).
pub const PLAYER_HALF_WIDTH: f32 = 0.3;
/// Player AABB height.
pub const PLAYER_HEIGHT: f32 = 1.8;
/// Eye height above the feet position.
pub const PLAYER_EYE_HEIGHT: f32 = 1.62;
/// Gravity in blocks/s².
pub const GRAVITY: f32 = 32.0;
/// Instant upward velocity on jump (≈1.27 block apex, Minecraft-like).
pub const JUMP_VELOCITY: f32 = 9.0;
/// Walking speed (blocks/s).
pub const WALK_SPEED: f32 = 4.317;
/// Sprinting speed.
pub const SPRINT_SPEED: f32 = 5.612;
/// Sneaking speed.
pub const SNEAK_SPEED: f32 = 1.31;
/// Terminal fall velocity.
pub const TERMINAL_VELOCITY: f32 = 78.4;
/// Block interaction reach.
pub const REACH: f32 = 4.5;
/// Default mouse sensitivity (radians per raw input count).
pub const MOUSE_SENS: f32 = 0.0025;

// ---------------------------------------------------------------------------
// Orientation math — the single definition of yaw/pitch for the whole project.
//
// yaw 0 faces +X, increasing yaw turns to the right (+Z at yaw = 90°).
// pitch 0 is level, +pitch looks up, clamped to ±90°.
// ---------------------------------------------------------------------------

/// Unit view direction for a yaw/pitch pair.
pub fn look_dir(yaw: f32, pitch: f32) -> Vec3 {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    Vec3::new(cy * cp, sp, sy * cp)
}

/// Horizontal forward direction for a yaw (movement, not looking).
pub fn yaw_forward(yaw: f32) -> Vec2 {
    let (s, c) = yaw.sin_cos();
    Vec2::new(c, s)
}

/// Horizontal right direction for a yaw.
pub fn yaw_right(yaw: f32) -> Vec2 {
    let (s, c) = yaw.sin_cos();
    Vec2::new(-s, c)
}

// ---------------------------------------------------------------------------
// AABB
// ---------------------------------------------------------------------------

/// An axis-aligned bounding box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    pub fn new(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }

    /// AABB of the unit cube at block coordinates `x..x+1` etc.
    pub fn block(x: i32, y: i32, z: i32) -> Self {
        Self::new(
            Vec3::new(x as f32, y as f32, z as f32),
            Vec3::new(x as f32 + 1.0, y as f32 + 1.0, z as f32 + 1.0),
        )
    }

    pub fn center(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    /// Squared distance from a point to the closest point of this box.
    pub fn distance_squared_to_point(&self, p: Vec3) -> f32 {
        let cx = p.x.clamp(self.min.x, self.max.x);
        let cy = p.y.clamp(self.min.y, self.max.y);
        let cz = p.z.clamp(self.min.z, self.max.z);
        (Vec3::new(cx, cy, cz) - p).length_squared()
    }

    /// Whether this box overlaps another (used for placement validation).
    pub fn intersects(&self, other: &Aabb) -> bool {
        self.min.x < other.max.x
            && self.max.x > other.min.x
            && self.min.y < other.max.y
            && self.max.y > other.min.y
            && self.min.z < other.max.z
            && self.max.z > other.min.z
    }
}

// ---------------------------------------------------------------------------
// Frustum culling — the core of client performance (specs.md §3).
// ---------------------------------------------------------------------------

/// A plane as `dot(n, p) + d = 0`, `n` normalized.
#[derive(Debug, Clone, Copy)]
pub struct Plane {
    pub n: Vec3,
    pub d: f32,
}

impl Plane {
    fn from_vec4(v: Vec4) -> Self {
        let n = Vec3::new(v.x, v.y, v.z);
        let len = n.length();
        Plane {
            n: n / len,
            d: v.w / len,
        }
    }

    /// Signed distance of a point to the plane.
    pub fn distance(&self, p: Vec3) -> f32 {
        self.n.dot(p) + self.d
    }
}

/// Six-plane view frustum extracted from a view-projection matrix.
#[derive(Debug, Clone, Copy)]
pub struct Frustum {
    planes: [Plane; 6],
}

impl Frustum {
    /// Extract the frustum planes from a column-major view-projection matrix
    /// (Gribb & Hartmann).
    pub fn from_view_proj(m: &Mat4) -> Self {
        let r0 = m.row(0);
        let r1 = m.row(1);
        let r2 = m.row(2);
        let r3 = m.row(3);
        Frustum {
            planes: [
                Plane::from_vec4(r3 + r0), // left
                Plane::from_vec4(r3 - r0), // right
                Plane::from_vec4(r3 + r1), // bottom
                Plane::from_vec4(r3 - r1), // top
                Plane::from_vec4(r3 + r2), // near
                Plane::from_vec4(r3 - r2), // far
            ],
        }
    }

    /// Conservative AABB-vs-frustum test (p/n-vertex method): returns true
    /// if the box may be visible. Never culls a visible box; may keep a box
    /// that is outside in a corner pocket — conservative is correct for
    /// culling.
    pub fn intersects_aabb(&self, a: &Aabb) -> bool {
        for p in &self.planes {
            // The p-vertex: the corner furthest along the plane normal.
            let px = if p.n.x >= 0.0 { a.max.x } else { a.min.x };
            let py = if p.n.y >= 0.0 { a.max.y } else { a.min.y };
            let pz = if p.n.z >= 0.0 { a.max.z } else { a.min.z };
            if p.distance(Vec3::new(px, py, pz)) < 0.0 {
                return false; // fully outside one plane → invisible
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aabb_fully_inside_is_visible() {
        let proj = Mat4::perspective_rh(70f32.to_radians(), 16.0 / 9.0, 0.1, 100.0);
        let view = Mat4::look_to_rh(Vec3::new(0.0, 0.0, 0.0), -Vec3::Z, Vec3::Y);
        let f = Frustum::from_view_proj(&(proj * view));
        let box_near = Aabb::block(-1, -1, -5);
        assert!(f.intersects_aabb(&box_near));
    }

    #[test]
    fn aabb_behind_camera_is_culled() {
        let proj = Mat4::perspective_rh(70f32.to_radians(), 16.0 / 9.0, 0.1, 100.0);
        let view = Mat4::look_to_rh(Vec3::new(0.0, 0.0, 0.0), -Vec3::Z, Vec3::Y);
        let f = Frustum::from_view_proj(&(proj * view));
        let box_behind = Aabb::block(-1, -1, 5); // behind the camera
        assert!(!f.intersects_aabb(&box_behind));
    }
}
