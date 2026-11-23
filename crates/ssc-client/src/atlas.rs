//! Procedural placeholder texture atlas + texture-pack loading.
//!
//! The procedural atlas is the asset-free default; `--textures PATH`
//! loads a real Minecraft-format atlas PNG (256×256, 16×16 tiles of
//! 16×16 px) into the exact same atlas slots (`specs.md` §3: we support
//! Minecraft textures, we just can't ship Mojang's).
//!
//! Row order is bottom-up (GL convention, `specs.md` engine contract).

/// Tiles per atlas edge.
pub const ATLAS_TILES: u32 = 16;
/// Pixels per tile edge.
pub const TILE_SIZE: u32 = 16;
/// Atlas edge in pixels (Minecraft's standard terrain-atlas size).
pub const ATLAS_PX: u32 = ATLAS_TILES * TILE_SIZE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glass_texture_is_translucent_with_a_visible_border() {
        let border = paint_pixel(ssc_world::TILE_GLASS, 0, 8);
        let center = paint_pixel(ssc_world::TILE_GLASS, 8, 8);
        assert!(center[3] > 0 && center[3] < border[3] && border[3] < 255);
        assert_eq!(paint_pixel(ssc_world::TILE_SMOOTH_STONE, 8, 8)[3], 255);
    }
}

/// Deterministic per-pixel hash noise in `0..1`.
fn noise2(x: i32, y: i32, s: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(3_747_613_937)
        ^ (y as u32).wrapping_mul(2_654_435_761)
        ^ (s as u32).wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^= h >> 16;
    (h & 0xFFFF) as f32 / 65535.0
}

/// Smooth-ish value noise by averaging a 2×2 hash neighborhood.
fn smooth_noise2(x: i32, y: i32, s: i32) -> f32 {
    let a = noise2(x, y, s);
    let b = noise2(x + 1, y, s);
    let c = noise2(x, y + 1, s);
    let d = noise2(x + 1, y + 1, s);
    (a + b + c + d) * 0.25
}

type Rgba = [u8; 4];

fn mix(base: [u8; 3], shade: f32, alpha: u8) -> Rgba {
    [
        (base[0] as f32 * shade).clamp(0.0, 255.0) as u8,
        (base[1] as f32 * shade).clamp(0.0, 255.0) as u8,
        (base[2] as f32 * shade).clamp(0.0, 255.0) as u8,
        alpha,
    ]
}

/// Paint one pixel of tile `tile` at intra-tile coords `(x, y)` (y down
/// in image space).
fn paint_pixel(tile: u16, x: i32, y: i32) -> Rgba {
    match tile {
        ssc_world::TILE_MISSING => {
            // Magenta/black 2×2 checker — unmissable.
            if (x / 2 + y / 2) % 2 == 0 {
                [255, 0, 255, 255]
            } else {
                [10, 10, 10, 255]
            }
        }
        ssc_world::TILE_STONE => {
            let n = smooth_noise2(x, y, 1);
            mix([128, 128, 128], 0.82 + n * 0.36, 255)
        }
        ssc_world::TILE_COBBLESTONE => {
            // 4×4 rounded stones with dark seams.
            let cell = (x / 4 + (y / 4) * 4) as i32;
            let in_seam = x % 4 == 0 || y % 4 == 0;
            let n = noise2(x, y, cell + 2);
            if in_seam {
                mix([70, 70, 70], 0.9 + n * 0.2, 255)
            } else {
                mix([135, 135, 135], 0.8 + n * 0.4, 255)
            }
        }
        ssc_world::TILE_DIRT => mix([134, 96, 67], 0.75 + noise2(x, y, 3) * 0.5, 255),
        ssc_world::TILE_GRASS_TOP => mix([98, 160, 74], 0.75 + noise2(x, y, 4) * 0.5, 255),
        ssc_world::TILE_GRASS_SIDE => {
            // Dirt with a ragged grass lip on top.
            let edge = 2 + (noise2(x, 0, 5) * 2.5) as i32;
            if y < edge {
                mix([98, 160, 74], 0.8 + noise2(x, y, 4) * 0.4, 255)
            } else {
                mix([134, 96, 67], 0.75 + noise2(x, y, 3) * 0.5, 255)
            }
        }
        ssc_world::TILE_SAND => mix([219, 207, 163], 0.88 + noise2(x, y, 6) * 0.24, 255),
        ssc_world::TILE_SANDSTONE_TOP => mix([216, 203, 155], 0.93 + noise2(x, y, 7) * 0.14, 255),
        ssc_world::TILE_SANDSTONE_SIDE => {
            if (4..7).contains(&y) {
                mix([196, 183, 135], 0.95 + noise2(x, y, 8) * 0.1, 255)
            } else {
                mix([216, 203, 155], 0.92 + noise2(x, y, 7) * 0.16, 255)
            }
        }
        ssc_world::TILE_GRAVEL => {
            let n = noise2(x, y, 9);
            if n > 0.72 {
                mix([160, 150, 140], 0.9 + n * 0.1, 255) // light pebbles
            } else {
                mix([110, 100, 95], 0.8 + n * 0.4, 255)
            }
        }
        ssc_world::TILE_LOG_TOP => {
            // Concentric rings.
            let dx = x as f32 - 7.5;
            let dy = y as f32 - 7.5;
            let r = (dx * dx + dy * dy).sqrt();
            let ring = ((r * 1.4).sin() * 0.5 + 0.5) * 0.25;
            mix([160, 130, 78], 0.85 - ring + noise2(x, y, 10) * 0.1, 255)
        }
        ssc_world::TILE_STRIPPED_LOG_SIDE => {
            let grain = noise2(x / 2, y / 6, 31);
            mix([182, 148, 91], 0.86 + grain * 0.22, 255)
        }
        ssc_world::TILE_LOG_SIDE => {
            // Vertical bark streaks.
            let streak = noise2(x, 0, 11);
            let dark_col = streak > 0.65;
            mix(
                [107, 84, 51],
                if dark_col { 0.72 } else { 0.98 } + noise2(x, y, 11) * 0.08,
                255,
            )
        }
        ssc_world::TILE_PLANKS => {
            let seam_row = y % 4 == 3;
            let band = y / 4;
            let seam_x = (noise2(0, band, 12) * 16.0) as i32;
            if seam_row || x == seam_x {
                mix([110, 86, 52], 0.9, 255)
            } else {
                mix([162, 130, 78], 0.9 + noise2(x, y, 12) * 0.2, 255)
            }
        }
        ssc_world::TILE_FARMLAND | ssc_world::TILE_FARMLAND_WET => {
            // Dirt rows; wet farmland is darker and has faint water sheen.
            let furrow = y % 4 == 2;
            let base = if tile == ssc_world::TILE_FARMLAND_WET {
                [84, 62, 45]
            } else {
                [120, 86, 58]
            };
            if furrow {
                mix(base, 0.7, 255)
            } else {
                mix(base, 0.9 + noise2(x, y, 37) * 0.2, 255)
            }
        }
        ssc_world::TILE_WHEAT_0
        | ssc_world::TILE_WHEAT_1
        | ssc_world::TILE_WHEAT_2
        | ssc_world::TILE_WHEAT_3 => {
            // Stems with heads. Each stage is taller and more golden.
            let stage = (tile - ssc_world::TILE_WHEAT_0) as i32;
            let height = 4 + stage * 3;
            let stem_x = x % 5 == 2;
            if stem_x && (15 - y) <= height {
                mix([74, 132, 52], 0.9 + noise2(x, y, 38) * 0.2, 220)
            } else if (15 - y) <= height && (x % 5 - 2).abs() <= 1 && y < 7 {
                let gold = stage >= 2;
                mix(
                    if gold { [196, 168, 70] } else { [122, 158, 64] },
                    0.85 + noise2(x, y, 39) * 0.3,
                    if stage == 0 { 180 } else { 230 },
                )
            } else {
                [0, 0, 0, 0]
            }
        }
        ssc_world::TILE_GLASS => {
            if x == 0 || x == 15 || y == 0 || y == 15 {
                [180, 220, 230, 180]
            } else if (x - y == 3 || x - y == 4) && (3..=10).contains(&x) {
                [210, 240, 250, 110]
            } else {
                [185, 220, 235, 24]
            }
        }
        ssc_world::TILE_SMOOTH_STONE => {
            let shade = if x == 0 || y == 0 || x == 15 || y == 15 {
                0.75
            } else {
                0.95 + noise2(x, y, 35) * 0.05
            };
            mix([160, 160, 160], shade, 255)
        }
        ssc_world::TILE_FURNACE | ssc_world::TILE_LIT_FURNACE => {
            if (3..=12).contains(&x) && (7..=12).contains(&y) {
                if tile == ssc_world::TILE_LIT_FURNACE {
                    mix([240, 119, 30], 0.8 + noise2(x, y, 33) * 0.2, 255)
                } else {
                    mix([35, 35, 35], 1.0, 255)
                }
            } else {
                mix([118, 118, 118], 0.8 + noise2(x, y, 34) * 0.25, 255)
            }
        }
        ssc_world::TILE_OAK_SAPLING => {
            // Placeholder plant silhouette, displayed on a translucent cube
            // until crossed-quad vegetation meshes are implemented.
            if (7..=8).contains(&x) && y >= 6 {
                mix([115, 80, 42], 1.0, 255)
            } else if (2..=13).contains(&x)
                && (2..=10).contains(&y)
                && (x - 8).abs() + (y - 6).abs() < 8
            {
                mix([62, 133, 44], 0.8 + noise2(x, y, 32) * 0.3, 255)
            } else {
                [0, 0, 0, 0]
            }
        }
        ssc_world::TILE_LEAVES => {
            let n = noise2(x, y, 13);
            if n > 0.8 {
                mix([30, 70, 25], 1.0, 255) // dark holes
            } else {
                mix([60, 120, 45], 0.75 + n * 0.5, 255)
            }
        }
        ssc_world::TILE_SNOW => mix([240, 246, 250], 0.96 + noise2(x, y, 14) * 0.06, 255),
        ssc_world::TILE_BRICKS => {
            let row = y / 4;
            let offset = (row % 2) * 4;
            let mortar_row = y % 4 == 3;
            let mortar_col = (x + offset) % 8 == 7;
            if mortar_row || mortar_col {
                mix([188, 188, 188], 0.92, 255)
            } else {
                mix([150, 67, 54], 0.85 + noise2(x, y, 15) * 0.3, 255)
            }
        }
        ssc_world::TILE_WATER => {
            // Translucent blue with soft horizontal currents.
            let wave = smooth_noise2(x, y / 4, 17);
            mix([63, 118, 228], 0.85 + wave * 0.3, 178)
        }
        ssc_world::TILE_COAL_ORE
        | ssc_world::TILE_IRON_ORE
        | ssc_world::TILE_DEEPSLATE_COAL_ORE
        | ssc_world::TILE_DEEPSLATE_IRON_ORE
        | ssc_world::TILE_COPPER_ORE
        | ssc_world::TILE_DEEPSLATE_COPPER_ORE => {
            let n = noise2(x / 3, y / 3, 23);
            if n > 0.64 {
                if tile == ssc_world::TILE_COAL_ORE || tile == ssc_world::TILE_DEEPSLATE_COAL_ORE {
                    mix([37, 35, 35], 0.8 + n * 0.2, 255)
                } else if tile == ssc_world::TILE_COPPER_ORE
                    || tile == ssc_world::TILE_DEEPSLATE_COPPER_ORE
                {
                    mix([191, 110, 73], 0.8 + n * 0.2, 255)
                } else {
                    mix([202, 164, 133], 0.8 + n * 0.2, 255)
                }
            } else if tile == ssc_world::TILE_DEEPSLATE_IRON_ORE
                || tile == ssc_world::TILE_DEEPSLATE_COAL_ORE
                || tile == ssc_world::TILE_DEEPSLATE_COPPER_ORE
            {
                mix([72, 75, 78], 0.82 + smooth_noise2(x, y, 29) * 0.36, 255)
            } else {
                mix([128, 128, 128], 0.82 + smooth_noise2(x, y, 1) * 0.36, 255)
            }
        }
        ssc_world::TILE_DEEPSLATE => mix([72, 75, 78], 0.82 + smooth_noise2(x, y, 29) * 0.36, 255),
        ssc_world::TILE_COBBLED_DEEPSLATE => {
            let seam = x % 4 == 0 || y % 4 == 0;
            mix(
                if seam { [45, 47, 51] } else { [82, 84, 88] },
                0.85 + noise2(x, y, 30) * 0.25,
                255,
            )
        }
        _ => [255, 0, 255, 255],
    }
}

/// Build the atlas as raw RGBA with row 0 at the **bottom** (GL upload
/// convention).
pub fn build_atlas_rgba() -> Vec<u8> {
    let mut out = vec![0u8; (ATLAS_PX * ATLAS_PX * 4) as usize];
    for tile in 0..(ATLAS_TILES * ATLAS_TILES) {
        let tx = (tile % ATLAS_TILES) * TILE_SIZE;
        let ty = (tile / ATLAS_TILES) * TILE_SIZE;
        for py in 0..TILE_SIZE {
            for px in 0..TILE_SIZE {
                let rgba = paint_pixel(tile as u16, px as i32, py as i32);
                let img_y = ty + py; // image row, y down from top
                let gl_y = ATLAS_PX - 1 - img_y; // flip for GL
                let idx = ((gl_y * ATLAS_PX) + tx + px) as usize * 4;
                out[idx] = rgba[0];
                out[idx + 1] = rgba[1];
                out[idx + 2] = rgba[2];
                out[idx + 3] = rgba[3];
            }
        }
    }
    out
}

/// Load a texture-pack atlas PNG (Minecraft terrain-atlas format:
/// 256×256, 16×16 tiles of 16×16 px). Returns bottom-up RGBA ready for
/// the engine upload.
pub fn load_png_atlas(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("png info {}: {e}", path.display()))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png decode {}: {e}", path.display()))?;
    if info.width != ATLAS_PX || info.height != ATLAS_PX {
        return Err(format!(
            "{}: atlas must be {ATLAS_PX}×{ATLAS_PX}, got {}×{}",
            path.display(),
            info.width,
            info.height
        ));
    }

    // Normalize to RGBA (paletted/RGB PNGs are common in packs).
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..(ATLAS_PX * ATLAS_PX * 4) as usize].to_vec(),
        png::ColorType::Rgb => {
            let src = &buf[..(ATLAS_PX * ATLAS_PX * 3) as usize];
            let mut out = Vec::with_capacity((ATLAS_PX * ATLAS_PX * 4) as usize);
            for px in src.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let src = &buf[..(ATLAS_PX * ATLAS_PX) as usize];
            let mut out = Vec::with_capacity((ATLAS_PX * ATLAS_PX * 4) as usize);
            for g in src {
                out.extend_from_slice(&[*g, *g, *g, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let src = &buf[..(ATLAS_PX * ATLAS_PX * 2) as usize];
            let mut out = Vec::with_capacity((ATLAS_PX * ATLAS_PX * 4) as usize);
            for ga in src.chunks_exact(2) {
                out.extend_from_slice(&[ga[0], ga[0], ga[0], ga[1]]);
            }
            out
        }
        png::ColorType::Indexed => return Err("paletted PNG: convert to RGBA first".into()),
    };

    // PNG is top-down; the engine upload wants bottom-up. Flip rows.
    let row = (ATLAS_PX * 4) as usize;
    let mut out = vec![0u8; rgba.len()];
    for y in 0..ATLAS_PX as usize {
        let src_row = &rgba[y * row..(y + 1) * row];
        let dst_y = (ATLAS_PX as usize - 1 - y) * row;
        out[dst_y..dst_y + row].copy_from_slice(src_row);
    }
    Ok(out)
}
