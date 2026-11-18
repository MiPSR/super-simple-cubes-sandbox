//! Deterministic world generation: 2D simplex noise heightmap terrain.
//!
//! Owned implementation (no noise dependency, `specs.md` §4). Classic
//! Gustavson-style simplex with a seeded permutation table.
//!
//! v0 shape: rolling hills over a bedrock floor, grass/sand tops.
//! Trees, caves, biomes and structures land in later commits; every
//! extension is logged in `implemented-features.md`.

use ssc_world::{
    Column, SubChunk, AIR, BEDROCK, DIRT, GRASS_BLOCK, SAND, SANDSTONE, SNOW_BLOCK, STONE, MAX_Y,
    MIN_Y, SUBCHUNKS_PER_COLUMN,
};

/// Sea level (used for sand beaches; water itself is a later feature).
pub const SEA_LEVEL: i32 = 63;

/// World generator (deterministic for a seed).
pub struct WorldGen {
    seed: u64,
    noise: Simplex2,
    noise_d: Simplex2, // second field for material variation
}

impl WorldGen {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            noise: Simplex2::new(seed),
            noise_d: Simplex2::new(seed ^ 0x9E37_79B9_7F4A_7C15),
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

    /// Top block at (x, z) for the given terrain height.
    fn top_block(&self, _x: i32, _z: i32, h: i32) -> u16 {
        if h <= SEA_LEVEL + 1 {
            SAND
        } else if h > SEA_LEVEL + 45 {
            SNOW_BLOCK
        } else {
            GRASS_BLOCK
        }
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
                let dirt_noise =
                    self.noise_d.value(wx as f32 * 0.06, wz as f32 * 0.06);

                let bedrock_h = MIN_Y + (hash3(wx, wz, self.seed) % 4) as i32;
                for y in MIN_Y..=h {
                    let id = if y <= bedrock_h {
                        BEDROCK
                    } else if y == h {
                        top
                    } else if y > h - 4 {
                        // Sub-surface band: dirt, or sand/sandstone near sea.
                        if h <= SEA_LEVEL + 1 {
                            if dirt_noise > 0.35 && y > h - 2 {
                                SANDSTONE
                            } else {
                                SAND
                            }
                        } else {
                            DIRT
                        }
                    } else {
                        STONE
                    };
                    if id != AIR {
                        let (ci, ly) = sub_of(y);
                        if let Some(slot) = column.subchunks.get_mut(ci) {
                            if slot.is_none() {
                                *slot = Some(Box::new(SubChunk::default()));
                            }
                            slot.as_mut().unwrap().set(lx, ly, lz, id);
                        }
                    }
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

    /// LOD summary for one column as streamed to the client
    /// (Distant Horizons-style far data): 4×4 cells of (height, top tile).
    /// For unmodified columns this is computed from the height function
    /// directly — no column materialization needed. `world` is consulted
    /// for edited columns.
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
                                        best_tile = ssc_world::info(id).tiles[ssc_world::FACE_TOP];
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
                            best_tile = ssc_world::info(self.top_block(wx, wz, h)).tiles
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

/// Sub-chunk index and local Y for a world Y.
fn sub_of(y: i32) -> (usize, i32) {
    let rel = y - MIN_Y;
    ((rel / 16) as usize, rel % 16)
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
        Self { perm: Box::new(full) }
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
