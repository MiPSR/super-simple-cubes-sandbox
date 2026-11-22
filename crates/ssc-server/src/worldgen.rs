//! Deterministic world generation: 2D simplex noise heightmap terrain.
//!
//! Owned implementation (no noise dependency, `specs.md` §4). Classic
//! Gustavson-style simplex with a seeded permutation table.
//!
//! Rolling hills over a bedrock floor with caves, trees and basic ores.
//! Every extension is logged in `implemented-features.md`.

use ssc_world::{
    Column, SubChunk, AIR, BEDROCK, COAL_ORE, COPPER_ORE, DEEPSLATE, DEEPSLATE_COAL_ORE,
    DEEPSLATE_COPPER_ORE, DEEPSLATE_IRON_ORE, DIRT, GRASS_BLOCK, GRAVEL, IRON_ORE, MAX_Y, MIN_Y,
    OAK_LEAVES, OAK_LOG, SAND, SANDSTONE, SNOW_BLOCK, STONE, SUBCHUNKS_PER_COLUMN, WATER,
};

/// Sea level (water surfaces sit at the top of this block).
pub const SEA_LEVEL: i32 = 63;

/// World generator (deterministic for a seed).
pub struct WorldGen {
    seed: u64,
    noise: Simplex2,
    noise_d: Simplex2,     // second field for material variation
    biome_noise: Simplex2, // broad desert/plains regions
    noise3: Simplex3,      // cave carving
}

impl WorldGen {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            noise: Simplex2::new(seed),
            noise_d: Simplex2::new(seed ^ 0x9E37_79B9_7F4A_7C15),
            biome_noise: Simplex2::new(seed ^ 0xB10B_1A5E_0123_45AB),
            noise3: Simplex3::new(seed ^ 0xD1B5_4A32_D192_ED5),
        }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Terrain height (top solid Y, i.e. the Y of the highest block) at
    /// world coordinates.
    pub fn height(&self, x: i32, z: i32) -> i32 {
        let fx = x as f32;
        let fz = z as f32;
        // Four fbm octaves, gentle rolling hills.
        let mut amp = 1.0f32;
        let mut freq = 1.0f32 / 160.0;
        let mut sum = 0.0f32;
        let mut norm = 0.0f32;
        for _ in 0..4 {
            sum += amp * self.noise.value(fx * freq, fz * freq);
            norm += amp;
            amp *= 0.5;
            freq *= 2.0;
        }
        let h = SEA_LEVEL as f32 + 2.0 + (sum / norm) * 34.0;
        let h = h.floor() as i32;
        h.clamp(MIN_Y + 8, MAX_Y - 40)
    }

    /// Cave carve test: blobby tunnels through the stone body, kept away
    /// from the surface crust and the bedrock floor.
    fn is_cave(&self, x: i32, y: i32, z: i32, surface_h: i32, bedrock_h: i32) -> bool {
        if y <= bedrock_h + 3 || y >= surface_h - 5 {
            return false;
        }
        let v = self
            .noise3
            .value(x as f32 / 56.0, y as f32 / 28.0, z as f32 / 56.0);
        v > 0.58
    }

    /// Seeded 8³ cells hold small ore clusters. Use world coordinates and
    /// Euclidean division so generation is independent of column boundaries
    /// and works equally on both sides of the origin. Replaces the generated
    /// stone/deepslate body, not cave air or the surface.
    fn ore_at(&self, x: i32, y: i32, z: i32) -> u16 {
        let cell_x = x.div_euclid(8);
        let cell_y = y.div_euclid(8);
        let cell_z = z.div_euclid(8);
        let local_x = x.rem_euclid(8);
        let local_y = y.rem_euclid(8);
        let local_z = z.rem_euclid(8);
        for (ore, salt, min_y, max_y, chance) in [
            (IRON_ORE, 0x1A0F_u64, MIN_Y, 64, 3),
            (COPPER_ORE, 0xC0AA_u64, -16, 112, 2),
            (COAL_ORE, 0xC0A1_u64, 0, 192, 2),
        ] {
            if !(min_y..=max_y).contains(&y) {
                continue;
            }
            let hash = hash3(
                cell_x,
                cell_z,
                self.seed ^ salt ^ (cell_y as u64).wrapping_mul(0xD1B5_4A32_D192_ED5),
            );
            if hash % chance != 0 {
                continue;
            }
            let dx = local_x - 2 - ((hash >> 8) % 4) as i32;
            let dy = local_y - 2 - ((hash >> 16) % 4) as i32;
            let dz = local_z - 2 - ((hash >> 24) % 4) as i32;
            if dx * dx + dy * dy + dz * dz <= 4 {
                return match (ore, self.deepslate_at(x, y, z)) {
                    (IRON_ORE, true) => DEEPSLATE_IRON_ORE,
                    (COPPER_ORE, true) => DEEPSLATE_COPPER_ORE,
                    (COAL_ORE, true) => DEEPSLATE_COAL_ORE,
                    _ => ore,
                };
            }
        }
        // Small gravel deposits, including below Y=0. Generated deposits
        // remain suspended at cave edges until block updates are simulated.
        let gravel_hash = hash3(
            cell_x,
            cell_z,
            self.seed ^ 0x6A4E_1_u64 ^ (cell_y as u64).wrapping_mul(0xA24B_8D9C),
        );
        if gravel_hash % 4 == 0 {
            let dx = local_x - 2 - ((gravel_hash >> 8) % 4) as i32;
            let dy = local_y - 2 - ((gravel_hash >> 16) % 4) as i32;
            let dz = local_z - 2 - ((gravel_hash >> 24) % 4) as i32;
            if dx * dx + dy * dy + dz * dz <= 4 {
                return GRAVEL;
            }
        }
        if self.deepslate_at(x, y, z) {
            DEEPSLATE
        } else {
            STONE
        }
    }

    /// Below Y=0 stone is deepslate. Between Y=0 and Y=8, choose a
    /// deterministic mix that becomes progressively more common downwards.
    fn deepslate_at(&self, x: i32, y: i32, z: i32) -> bool {
        y < 0
            || (y < 8
                && hash3(x, z, self.seed ^ (y as u64).wrapping_mul(0x81)) % 8 < (8 - y) as u64)
    }

    /// Coarse climate classification, deterministic in world coordinates
    /// (including negative positions and column boundaries).
    fn is_desert(&self, x: i32, z: i32) -> bool {
        self.biome_noise.value(x as f32 / 350.0, z as f32 / 350.0) > 0.42
    }

    /// Top block at (x, z) for the given terrain height.
    fn top_block(&self, x: i32, z: i32, h: i32) -> u16 {
        if h <= SEA_LEVEL + 1 {
            // Small seeded gravel disks on submerged basin floors. Centers
            // stay inside their 12×12 cells; world coordinates ensure the
            // same disk is generated across sub-chunk/column boundaries.
            let cx = x.div_euclid(12);
            let cz = z.div_euclid(12);
            let hash = hash3(cx, cz, self.seed ^ 0xB451_6A2E);
            let dx = x.rem_euclid(12) - 3 - ((hash >> 8) % 6) as i32;
            let dz = z.rem_euclid(12) - 3 - ((hash >> 16) % 6) as i32;
            if h < SEA_LEVEL && hash % 3 == 0 && dx * dx + dz * dz <= 9 {
                GRAVEL
            } else {
                SAND
            }
        } else if self.is_desert(x, z) {
            SAND
        } else if h > SEA_LEVEL + 45 {
            SNOW_BLOCK
        } else {
            GRASS_BLOCK
        }
    }

    /// Deterministic tree candidate hosted by column `(cx, cz)`:
    /// `Some((local_x, local_z, base_y, trunk_h))`.
    fn tree_at(&self, cx: i32, cz: i32) -> Option<(i32, i32, i32, i32)> {
        let h = hash3(cx, cz, self.seed ^ 0x7EE5_0000_0000_0001);
        if h % 16 != 0 {
            return None; // ~1 column in 16 hosts a tree.
        }
        let base_h = self.height(cx * 16 + 8, cz * 16 + 8);
        let lx = (4 + (h >> 8) % 8) as i32;
        let lz = (4 + (h >> 16) % 8) as i32;
        // No oaks on the desert's sand or the snow caps.
        if base_h <= SEA_LEVEL + 1
            || self.top_block(cx * 16 + lx, cz * 16 + lz, base_h) != GRASS_BLOCK
        {
            return None;
        }
        let trunk_h = 4 + ((h >> 24) % 3) as i32;
        Some((lx, lz, base_h + 1, trunk_h))
    }

    /// Generate one full 16×16 column.
    pub fn generate_column(&self, cx: i32, cz: i32) -> Column {
        let mut column = Column::default();
        for lx in 0..16 {
            for lz in 0..16 {
                let wx = cx * 16 + lx;
                let wz = cz * 16 + lz;
                let h = self.height(wx, wz);
                let top = self.top_block(wx, wz, h);
                // Material variation in the dirt band.
                let dirt_noise = self.noise_d.value(wx as f32 * 0.06, wz as f32 * 0.06);

                let bedrock_h = MIN_Y + (hash3(wx, wz, self.seed) % 4) as i32;
                for y in MIN_Y..=h {
                    let id = if y <= bedrock_h {
                        BEDROCK
                    } else if self.is_cave(wx, y, wz, h, bedrock_h) {
                        AIR // carved
                    } else if y == h {
                        top
                    } else if y > h - 4 {
                        // Sub-surface band: dirt, or sand/sandstone near sea.
                        if self.is_desert(wx, wz) && h > SEA_LEVEL + 1 {
                            if y >= h - 2 {
                                SAND
                            } else {
                                SANDSTONE
                            }
                        } else if h <= SEA_LEVEL + 1 {
                            if dirt_noise > 0.35 && y > h - 2 {
                                SANDSTONE
                            } else {
                                SAND
                            }
                        } else {
                            DIRT
                        }
                    } else {
                        self.ore_at(wx, y, wz)
                    };
                    if id != AIR {
                        set_col_block(&mut column, lx, y, lz, id);
                    }
                }
                // Water: fill the basin above the terrain surface up to
                // sea level. Cave pockets stay dry (no flood simulation).
                if h < SEA_LEVEL {
                    for y in (h + 1)..=SEA_LEVEL {
                        set_col_block(&mut column, lx, y, lz, WATER);
                    }
                }
            }
        }
        // Trees: scan this column plus its ±2 neighborhood so canopies
        // crossing column borders are placed consistently (each tree is
        // owned by its host column, deterministically).
        for dcx in -2i32..=2 {
            for dcz in -2i32..=2 {
                if let Some((tlx, tlz, base, trunk_h)) = self.tree_at(cx + dcx, cz + dcz) {
                    self.place_tree(&mut column, cx, cz, dcx, dcz, tlx, tlz, base, trunk_h);
                }
            }
        }
        // Ensure every column has at least one sub-chunk allocated so
        // `has_content` bookkeeping stays uniform (cheap: all air).
        if column.subchunks.iter().all(|s| s.is_none()) {
            column.subchunks[0] = Some(Box::new(SubChunk::default()));
        }
        let _ = SUBCHUNKS_PER_COLUMN;
        column
    }

    /// Place the parts of a tree (owned by a neighbor column) that fall
    /// inside the column being generated.
    #[allow(clippy::too_many_arguments)]
    fn place_tree(
        &self,
        column: &mut Column,
        cx: i32,
        cz: i32,
        dcx: i32,
        dcz: i32,
        tlx: i32,
        tlz: i32,
        base: i32,
        trunk_h: i32,
    ) {
        // World position of the trunk center.
        let twx = (cx + dcx) * 16 + tlx;
        let twz = (cz + dcz) * 16 + tlz;
        // Trunk (only inside the host column; also draw when this column
        // IS the host).
        for i in 0..trunk_h {
            let wy = base + i;
            let lx = twx - cx * 16;
            let lz = twz - cz * 16;
            if (0..16).contains(&lx) && (0..16).contains(&lz) {
                set_col_block(column, lx, wy, lz, OAK_LOG);
            }
        }
        // Canopy: two 5×5-ish layers around the trunk top, one 3×3 cap,
        // corner leaves trimmed semi-randomly (deterministic per corner).
        let top = base + trunk_h - 1;
        for dy in -2i32..=1 {
            let r = if dy <= -1 { 2 } else { 1 };
            for dx in -r..=r {
                for dz in -r..=r {
                    let lx = twx + dx - cx * 16;
                    let lz = twz + dz - cz * 16;
                    if !(0..16).contains(&lx) || !(0..16).contains(&lz) {
                        continue;
                    }
                    let wy = top + dy + 1;
                    // Cap corners always trimmed; lower-layer corners
                    // trimmed deterministically per position.
                    let is_corner = dx.abs() == r && dz.abs() == r;
                    if is_corner
                        && (dy == 1 || hash3(twx + dx, twz + dz, self.seed ^ (dy as u64)) % 2 == 0)
                    {
                        continue;
                    }
                    if get_col_block(column, lx, wy, lz) == AIR {
                        set_col_block(column, lx, wy, lz, OAK_LEAVES);
                    }
                }
            }
        }
    }

    /// LOD summary for one column as streamed to the client
    /// (Distant Horizons-style far data): 4×4 cells of (height, top tile).
    /// Water is ignored by LOD v0 (lake beds read as sand pits) — noted
    /// in implemented-features.md. `world` is consulted for edited
    /// columns.
    pub fn lod_summary(
        &self,
        cx: i32,
        cz: i32,
        world: &ssc_world::World,
        edited: &std::collections::HashSet<ssc_world::ColumnKey>,
    ) -> [u16; 32] {
        let mut cells = [0u16; 32];
        // If the column was edited, summarize the real stored data.
        if edited.contains(&(cx, cz)) {
            if let Some(column) = world.column(cx, cz) {
                for lx in 0..4 {
                    for lz in 0..4 {
                        let (mut best_y, mut best_tile) = (0i32, ssc_world::TILE_STONE);
                        for bx in 0..4 {
                            for bz in 0..4 {
                                if let Some(y) = column.top_solid_y(lx * 4 + bx, lz * 4 + bz) {
                                    if y >= best_y {
                                        best_y = y;
                                        let id = column.get(lx * 4 + bx, y, lz * 4 + bz);
                                        best_tile =
                                            ssc_world::block_info(id).tiles[ssc_world::FACE_TOP];
                                    }
                                }
                            }
                        }
                        let i = ((lz * 4 + lx) * 2) as usize;
                        cells[i] = if best_y == 0 { 0 } else { (best_y + 1) as u16 };
                        cells[i + 1] = best_tile;
                    }
                }
                return cells;
            }
        }
        // Unedited: pure height function, no world storage.
        for lx in 0..4 {
            for lz in 0..4 {
                let (mut best_h, mut best_tile) = (i32::MIN, ssc_world::TILE_STONE);
                for bx in 0..4 {
                    for bz in 0..4 {
                        let wx = cx * 16 + lx * 4 + bx;
                        let wz = cz * 16 + lz * 4 + bz;
                        let h = self.height(wx, wz);
                        if h >= best_h {
                            best_h = h;
                            best_tile = ssc_world::block_info(self.top_block(wx, wz, h)).tiles
                                [ssc_world::FACE_TOP];
                        }
                    }
                }
                let i = ((lz * 4 + lx) * 2) as usize;
                cells[i] = (best_h + 1) as u16;
                cells[i + 1] = best_tile;
            }
        }
        cells
    }
}

fn set_col_block(column: &mut Column, lx: i32, y: i32, lz: i32, id: u16) {
    column.set(lx, y, lz, id);
}

fn get_col_block(column: &Column, lx: i32, y: i32, lz: i32) -> u16 {
    column.get(lx, y, lz)
}

/// Deterministic integer hash for small decoration decisions.
fn hash3(x: i32, z: i32, seed: u64) -> u64 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (z as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ seed.wrapping_mul(0x165667B19E3779F4);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h
}

// ---------------------------------------------------------------------------
// 2D simplex noise (Gustavson-style, seeded, no dependencies)
// ---------------------------------------------------------------------------

pub struct Simplex2 {
    perm: Box<[u8; 512]>,
}

const F2: f32 = 0.3660254037844386; // (sqrt(3) - 1) / 2
const G2: f32 = 0.21132486540518713; // (3 - sqrt(3)) / 6

/// 8 gradient directions for 2D simplex.
const GRAD2: [(f32, f32); 8] = [
    (1.0, 1.0),
    (-1.0, 1.0),
    (1.0, -1.0),
    (-1.0, -1.0),
    (1.0, 0.0),
    (-1.0, 0.0),
    (0.0, 1.0),
    (0.0, -1.0),
];

impl Simplex2 {
    pub fn new(seed: u64) -> Self {
        let mut rng = seed | 1;
        let mut perm = [0u8; 256];
        for (i, p) in perm.iter_mut().enumerate() {
            *p = i as u8;
        }
        // Fisher-Yates with xorshift64.
        for i in (1..256).rev() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let j = (rng % (i as u64 + 1)) as usize;
            perm.swap(i, j);
        }
        let mut full = [0u8; 512];
        full[..256].copy_from_slice(&perm);
        full[256..].copy_from_slice(&perm);
        Self {
            perm: Box::new(full),
        }
    }
    /// Noise value in about −1..1.
    pub fn value(&self, x: f32, y: f32) -> f32 {
        let s = (x + y) * F2;
        let i = (x + s).floor();
        let j = (y + s).floor();
        let t = (i + j) * G2;
        let x0 = x - (i - t);
        let y0 = y - (j - t);

        // Which simplex triangle corner ordering.
        let (i1, j1) = if x0 > y0 { (1.0, 0.0) } else { (0.0, 1.0) };

        let x1 = x0 - i1 + G2;
        let y1 = y0 - j1 + G2;
        let x2 = x0 - 1.0 + 2.0 * G2;
        let y2 = y0 - 1.0 + 2.0 * G2;

        let ii = (i as i64 & 255) as usize;
        let jj = (j as i64 & 255) as usize;

        let n = |dx: f32, dy: f32, gi: usize| -> f32 {
            let t = 0.5 - dx * dx - dy * dy;
            if t <= 0.0 {
                return 0.0;
            }
            let t2 = t * t;
            let (gx, gy) = GRAD2[gi & 7];
            t2 * t2 * (gx * dx + gy * dy)
        };

        let gi0 = self.perm[ii + self.perm[jj] as usize] as usize;
        let gi1 = self.perm[ii + i1 as usize + self.perm[jj + j1 as usize] as usize] as usize;
        let gi2 = self.perm[ii + 1 + self.perm[jj + 1] as usize] as usize;

        70.0 * (n(x0, y0, gi0) + n(x1, y1, gi1) + n(x2, y2, gi2))
    }
}

// ---------------------------------------------------------------------------
// 3D simplex noise (Gustavson-style, seeded, no dependencies) — cave carving.
// ---------------------------------------------------------------------------

pub struct Simplex3 {
    perm: Box<[u8; 512]>,
}

/// Skewing factors for 3D simplex.
const F3: f32 = 1.0 / 3.0;
const G3: f32 = 1.0 / 6.0;

/// The classic 12 simplex gradients.
const GRAD3: [(f32, f32, f32); 12] = [
    (1.0, 1.0, 0.0),
    (-1.0, 1.0, 0.0),
    (1.0, -1.0, 0.0),
    (-1.0, -1.0, 0.0),
    (1.0, 0.0, 1.0),
    (-1.0, 0.0, 1.0),
    (1.0, 0.0, -1.0),
    (-1.0, 0.0, -1.0),
    (0.0, 1.0, 1.0),
    (0.0, -1.0, 1.0),
    (0.0, 1.0, -1.0),
    (0.0, -1.0, -1.0),
];

impl Simplex3 {
    pub fn new(seed: u64) -> Self {
        let mut rng = seed | 1;
        let mut perm = [0u8; 256];
        for (i, p) in perm.iter_mut().enumerate() {
            *p = i as u8;
        }
        for i in (1..256).rev() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let j = (rng % (i as u64 + 1)) as usize;
            perm.swap(i, j);
        }
        let mut full = [0u8; 512];
        full[..256].copy_from_slice(&perm);
        full[256..].copy_from_slice(&perm);
        Self {
            perm: Box::new(full),
        }
    }

    /// Noise value in about −1..1.
    pub fn value(&self, x: f32, y: f32, z: f32) -> f32 {
        let s = (x + y + z) * F3;
        let i = (x + s).floor();
        let j = (y + s).floor();
        let k = (z + s).floor();
        let t = (i + j + k) * G3;
        let x0 = x - (i - t);
        let y0 = y - (j - t);
        let z0 = z - (k - t);

        // Rank the coordinates to pick the simplex ordering.
        let (i1, j1, k1, i2, j2, k2) = if x0 >= y0 {
            if y0 >= z0 {
                (1, 0, 0, 1, 1, 0)
            } else if x0 >= z0 {
                (1, 0, 0, 1, 0, 1)
            } else {
                (0, 0, 1, 1, 0, 1)
            }
        } else if y0 < z0 {
            (0, 0, 1, 0, 1, 1)
        } else if x0 < z0 {
            (0, 1, 0, 0, 1, 1)
        } else {
            (0, 1, 0, 1, 1, 0)
        };

        let x1 = x0 - i1 as f32 + G3;
        let y1 = y0 - j1 as f32 + G3;
        let z1 = z0 - k1 as f32 + G3;
        let x2 = x0 - i2 as f32 + 2.0 * G3;
        let y2 = y0 - j2 as f32 + 2.0 * G3;
        let z2 = z0 - k2 as f32 + 2.0 * G3;
        let x3 = x0 - 1.0 + 3.0 * G3;
        let y3 = y0 - 1.0 + 3.0 * G3;
        let z3 = z0 - 1.0 + 3.0 * G3;

        let ii = (i as i64 & 255) as usize;
        let jj = (j as i64 & 255) as usize;
        let kk = (k as i64 & 255) as usize;

        let corner = |dx: f32, dy: f32, dz: f32, gi: usize| -> f32 {
            let t = 0.6 - dx * dx - dy * dy - dz * dz;
            if t <= 0.0 {
                return 0.0;
            }
            let t2 = t * t;
            let (gx, gy, gz) = GRAD3[gi % 12];
            t2 * t2 * (gx * dx + gy * dy + gz * dz)
        };

        let gi0 = self.perm[ii + self.perm[jj + self.perm[kk] as usize] as usize] as usize;
        let gi1 = self.perm[(ii + i1 as usize)
            + self.perm[(jj + j1 as usize) + self.perm[kk + k1 as usize] as usize] as usize]
            as usize;
        let gi2 = self.perm[(ii + i2 as usize)
            + self.perm[(jj + j2 as usize) + self.perm[kk + k2 as usize] as usize] as usize]
            as usize;
        let gi3 = self.perm[(ii + 1) + self.perm[(jj + 1) + self.perm[kk + 1] as usize] as usize]
            as usize;

        32.0 * (corner(x0, y0, z0, gi0)
            + corner(x1, y1, z1, gi1)
            + corner(x2, y2, z2, gi2)
            + corner(x3, y3, z3, gi3))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desert_and_plains_surfaces_follow_seeded_climate_across_columns() {
        let gen = WorldGen::new(7823);
        let mut desert = None;
        let mut plains = None;
        for x in (-1536..1536).step_by(16) {
            for z in (-1536..1536).step_by(16) {
                let h = gen.height(x, z);
                if h <= SEA_LEVEL + 1 || h >= SEA_LEVEL + 45 {
                    continue;
                }
                if gen.is_desert(x, z) {
                    desert.get_or_insert((x, h, z));
                } else {
                    plains.get_or_insert((x, h, z));
                }
                if desert.is_some() && plains.is_some() {
                    break;
                }
            }
            if desert.is_some() && plains.is_some() {
                break;
            }
        }
        for (position, top, under) in [
            (desert.expect("desert in seed 7823"), SAND, SANDSTONE),
            (plains.expect("plains in seed 7823"), GRASS_BLOCK, DIRT),
        ] {
            let (x, h, z) = position;
            let col = gen.generate_column(x.div_euclid(16), z.div_euclid(16));
            let (lx, lz) = (x.rem_euclid(16), z.rem_euclid(16));
            assert_eq!(col.get(lx, h, lz), top);
            // Sandstone is a lower band: the first desert layer can be sand.
            assert_eq!(col.get(lx, h - 3, lz), under);
            let summary = gen.lod_summary(
                x.div_euclid(16),
                z.div_euclid(16),
                &ssc_world::World::default(),
                &std::collections::HashSet::new(),
            );
            assert!(summary
                .chunks_exact(2)
                .any(|cell| cell[1] == ssc_world::block_info(top).tiles[ssc_world::FACE_TOP]));
        }
    }

    #[test]
    fn desert_columns_do_not_host_oak_trees() {
        let gen = WorldGen::new(7823);
        let mut candidates = 0;
        for cx in -64..64 {
            for cz in -64..64 {
                let roll = hash3(cx, cz, gen.seed ^ 0x7EE5_0000_0000_0001);
                if roll % 16 != 0 {
                    continue;
                }
                let lx = (4 + (roll >> 8) % 8) as i32;
                let lz = (4 + (roll >> 16) % 8) as i32;
                let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                if gen.is_desert(x, z) && gen.height(x, z) > SEA_LEVEL + 1 {
                    assert!(gen.tree_at(cx, cz).is_none());
                    candidates += 1;
                }
            }
        }
        assert!(
            candidates > 0,
            "test region must include dry desert tree candidates"
        );
    }

    #[test]
    fn coal_and_iron_generate_only_in_stone_at_their_depths() {
        let gen = WorldGen::new(31415);
        let mut coal = 0;
        let mut deep_coal = 0;
        let mut copper = 0;
        let mut deep_copper = 0;
        let mut iron = 0;
        for cx in -2..2 {
            for cz in -2..2 {
                let col = gen.generate_column(cx, cz);
                for lx in 0..16 {
                    for lz in 0..16 {
                        let x = cx * 16 + lx;
                        let z = cz * 16 + lz;
                        let h = gen.height(x, z);
                        let bedrock_h = MIN_Y + (hash3(x, z, gen.seed) % 4) as i32;
                        for y in MIN_Y..=h {
                            let block = col.get(lx, y, lz);
                            if block != COAL_ORE
                                && block != DEEPSLATE_COAL_ORE
                                && block != COPPER_ORE
                                && block != DEEPSLATE_COPPER_ORE
                                && block != IRON_ORE
                                && block != DEEPSLATE_IRON_ORE
                            {
                                continue;
                            }
                            assert!(y > bedrock_h && y <= h - 4);
                            assert!(!gen.is_cave(x, y, z, h, bedrock_h));
                            assert_eq!(block, gen.ore_at(x, y, z));
                            if matches!(block, COAL_ORE | DEEPSLATE_COAL_ORE) {
                                assert!(y >= 0);
                                assert_eq!(block == DEEPSLATE_COAL_ORE, gen.deepslate_at(x, y, z));
                                coal += 1;
                                deep_coal += usize::from(block == DEEPSLATE_COAL_ORE);
                            } else if matches!(block, COPPER_ORE | DEEPSLATE_COPPER_ORE) {
                                assert!((-16..=112).contains(&y));
                                assert_eq!(
                                    block == DEEPSLATE_COPPER_ORE,
                                    gen.deepslate_at(x, y, z)
                                );
                                copper += 1;
                                deep_copper += usize::from(block == DEEPSLATE_COPPER_ORE);
                            } else {
                                assert!(y <= 64);
                                assert_eq!(block == DEEPSLATE_IRON_ORE, gen.deepslate_at(x, y, z));
                                iron += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(
            coal > 0 && iron > 0,
            "expected both ores: coal={coal} iron={iron}"
        );
        assert!(
            deep_coal > 0,
            "transition layer should contain rare deepslate coal"
        );
        assert!(
            copper > 0 && deep_copper > 0,
            "both copper variants should generate"
        );
        let a = gen.generate_column(-1, -1);
        let b = gen.generate_column(-1, -1);
        for y in MIN_Y..=64 {
            assert_eq!(a.get(15, y, 15), b.get(15, y, 15));
        }
    }

    #[test]
    fn deepslate_replaces_stone_below_zero_and_transitions_through_eight() {
        let gen = WorldGen::new(972);
        let col = gen.generate_column(-1, 0);
        for lx in 0..16 {
            for lz in 0..16 {
                let x = lx - 16;
                let z = lz;
                for y in -48..=10 {
                    let h = gen.height(x, z);
                    let bedrock_h = MIN_Y + (hash3(x, z, gen.seed) % 4) as i32;
                    if gen.is_cave(x, y, z, h, bedrock_h) || y > h - 4 {
                        continue;
                    }
                    let block = col.get(lx, y, lz);
                    if y < 0 {
                        assert!(matches!(
                            block,
                            DEEPSLATE | DEEPSLATE_IRON_ORE | DEEPSLATE_COPPER_ORE | GRAVEL
                        ));
                    }
                    if y >= 8 {
                        assert!(matches!(
                            block,
                            STONE | COAL_ORE | IRON_ORE | COPPER_ORE | GRAVEL
                        ));
                    }
                }
            }
        }
    }

    #[test]
    fn gravel_deposits_generate_deterministically_on_both_sides_of_origin() {
        let gen = WorldGen::new(2137);
        for (cx, cz) in [(-2, -1), (1, 2)] {
            let a = gen.generate_column(cx, cz);
            let b = gen.generate_column(cx, cz);
            let mut count = 0;
            for lx in 0..16 {
                for lz in 0..16 {
                    for y in -50..40 {
                        if a.get(lx, y, lz) == GRAVEL {
                            assert_eq!(b.get(lx, y, lz), GRAVEL);
                            count += 1;
                        }
                    }
                }
            }
            assert!(count > 0, "expected gravel near column ({cx}, {cz})");
        }
    }

    #[test]
    fn caves_carve_air_below_the_surface() {
        let gen = WorldGen::new(1234);
        // Scan a few columns for carved cells strictly below the surface.
        let mut carved = 0usize;
        for cx in 0..8 {
            for cz in 0..8 {
                let col = gen.generate_column(cx, cz);
                let h = gen.height(cx * 16, cz * 16);
                for y in (MIN_Y + 10)..(h - 5) {
                    for lz in 0..16 {
                        for lx in 0..16 {
                            if col.get(lx, y, lz) == AIR {
                                carved += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(
            carved > 200,
            "expected substantial cave carving, got {carved} air cells"
        );
    }

    #[test]
    fn water_fills_basins_up_to_sea_level() {
        let gen = WorldGen::new(42);
        // Find a column below sea level (the seed-42 spawn basin is one).
        let mut found = false;
        for cx in -8..8 {
            for cz in -8..8 {
                let h = gen.height(cx * 16, cz * 16);
                if h >= SEA_LEVEL {
                    continue;
                }
                found = true;
                let col = gen.generate_column(cx, cz);
                // The top of the water body sits at sea level and the
                // surface block right above the terrain is water.
                assert_eq!(col.get(0, SEA_LEVEL, 0), WATER);
                assert_eq!(col.get(0, h + 1, 0), WATER);
                // No water above sea level.
                assert_eq!(col.get(0, SEA_LEVEL + 1, 0), AIR);
            }
        }
        assert!(found, "seed 42 should have a basin below sea level");
    }

    #[test]
    fn submerged_basins_have_gravel_disks_and_sand_between_them() {
        let gen = WorldGen::new(42);
        let mut gravel = None;
        let mut sand = None;
        for x in -96..96 {
            for z in -96..96 {
                let h = gen.height(x, z);
                if h >= SEA_LEVEL {
                    continue;
                }
                match gen.top_block(x, z, h) {
                    GRAVEL => gravel.get_or_insert((x, h, z)),
                    SAND => sand.get_or_insert((x, h, z)),
                    _ => unreachable!(),
                };
                if gravel.is_some() && sand.is_some() {
                    break;
                }
            }
            if gravel.is_some() && sand.is_some() {
                break;
            }
        }
        for (expected, pos) in [(GRAVEL, gravel), (SAND, sand)] {
            let (x, h, z) = pos.expect("seed 42 has both submerged gravel and sand");
            let col = gen.generate_column(x.div_euclid(16), z.div_euclid(16));
            assert_eq!(col.get(x.rem_euclid(16), h, z.rem_euclid(16)), expected);
            assert_eq!(
                col.get(x.rem_euclid(16), SEA_LEVEL, z.rem_euclid(16)),
                WATER
            );
        }
    }

    #[test]
    fn trees_are_deterministic_and_have_trunks() {
        let gen = WorldGen::new(7);
        // Deterministic: identical columns on repeated generation.
        let a = gen.generate_column(3, 5);
        let b = gen.generate_column(3, 5);
        for sub in 0..SUBCHUNKS_PER_COLUMN {
            let (sa, sb) = (a.subchunks[sub].as_ref(), b.subchunks[sub].as_ref());
            match (sa, sb) {
                (None, None) => continue,
                (Some(x), Some(y)) => assert_eq!(
                    x.blocks[..],
                    y.blocks[..],
                    "column (3,5) must generate identically"
                ),
                _ => panic!("sub-chunk presence must match"),
            }
        }
        // Somewhere in a reasonable area there is at least one oak trunk.
        let mut trunk = false;
        'outer: for cx in -12..12 {
            for cz in -12..12 {
                if let Some((lx, lz, base, _)) = gen.tree_at(cx, cz) {
                    let col = gen.generate_column(cx, cz);
                    if col.get(lx, base, lz) == OAK_LOG {
                        trunk = true;
                        break 'outer;
                    }
                }
            }
        }
        assert!(trunk, "expected at least one generated oak trunk");
    }
}
