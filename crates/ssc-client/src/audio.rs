//! Client sound playback (kira, owner-approved).
//!
//! Triggers arrive as packets from the *server* (`specs.md` §2) — the
//! client never decides sounds itself. Assets: a `--sounds DIR` pack
//! provides real OGG/WAV files (`dig.ogg`, `place.ogg`, `step.ogg`);
//! otherwise tiny procedural placeholder WAVs are synthesized in code
//! (we cannot ship Mojang assets).

use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;

use kira::sound::static_sound::StaticSoundData;
use kira::{
    AudioManager, AudioManagerSettings, Decibels, DefaultBackend, Panning,
};
use ssc_math::Vec3;
use ssc_protocol::packets::SoundId;

/// Sound files we look for in a pack, per trigger id.
const PACK_NAMES: &[(SoundId, &str)] = &[
    (SoundId::Dig, "dig"),
    (SoundId::Place, "place"),
    (SoundId::Step, "step"),
];

/// Attenuation: full volume within `NEAR` blocks, silent beyond `FAR`.
const NEAR: f32 = 3.0;
const FAR: f32 = 24.0;

pub struct AudioSystem {
    /// `None` on audio-less machines (headless CI): triggers are logged
    /// by the caller instead.
    manager: Option<AudioManager<DefaultBackend>>,
    sounds: HashMap<SoundId, StaticSoundData>,
}

impl AudioSystem {
    /// Build the system: load a sound pack if given, synthesize
    /// placeholders otherwise.
    pub fn new(pack_dir: Option<&Path>) -> Self {
        let manager = AudioManager::<DefaultBackend>::new(AudioManagerSettings::default())
            .map_err(|e| eprintln!("[client/audio] no audio device, silent mode: {e}"))
            .ok();
        let mut sounds = HashMap::new();
        for (id, name) in PACK_NAMES {
            let data = pack_dir.and_then(|dir| load_pack_sound(dir, name)).unwrap_or_else(|| {
                let (pcm, rate) = synth_placeholder(name);
                StaticSoundData::from_cursor(Cursor::new(wav_bytes(&pcm, rate)))
                    .expect("synthesized wav must decode")
            });
            sounds.insert(*id, data);
        }
        Self { manager, sounds }
    }

    /// Play a server-triggered sound at a world position, attenuated and
    /// stereo-panned relative to the camera.
    pub fn play(&mut self, id: SoundId, pos: Vec3, cam_pos: Vec3, cam_yaw: f32) {
        let Some(manager) = &mut self.manager else { return };
        let Some(data) = self.sounds.get(&id) else { return };
        let (db, pan) = spatialize(pos, cam_pos, cam_yaw);
        let played = manager.play(
            data.clone()
                .volume(Decibels::from(db))
                .panning(Panning(pan)),
        );
        if let Err(e) = played {
            eprintln!("[client/audio] playback failed: {e}");
        }
    }

    pub fn is_silent(&self) -> bool {
        self.manager.is_none()
    }
}

/// Distance attenuation (in dB) and stereo pan for a world-positioned
/// sound relative to the camera. Pure function — unit-tested.
pub fn spatialize(pos: Vec3, cam_pos: Vec3, cam_yaw: f32) -> (f32, f32) {
    let delta = pos - cam_pos;
    let dist = delta.length().max(0.001);
    // Linear attenuation between NEAR and FAR.
    let t = ((dist - NEAR) / (FAR - NEAR)).clamp(0.0, 1.0);
    let amplitude = 1.0 - t;
    let db = 20.0 * (amplitude.max(0.0001)).log10();

    // Pan by the horizontal direction of the sound relative to where the
    // camera faces (right = +pan). Yaw convention: forward = (cos, sin).
    let right = ssc_math::yaw_right(cam_yaw);
    let pan = if dist < 0.5 {
        0.0
    } else {
        (delta.x * right.x + delta.z * right.y) / dist
    };
    (db, pan.clamp(-1.0, 1.0))
}

/// Load `name.ogg` (or `.wav`) from a sound pack.
fn load_pack_sound(dir: &Path, name: &str) -> Option<StaticSoundData> {
    for ext in ["ogg", "wav"] {
        let path = dir.join(format!("{name}.{ext}"));
        if path.exists() {
            match StaticSoundData::from_file(&path) {
                Ok(d) => {
                    eprintln!("[client/audio] pack sound: {}", path.display());
                    return Some(d);
                }
                Err(e) => {
                    eprintln!("[client/audio] bad pack sound {}: {e}", path.display());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Procedural placeholder sounds — tiny synthesized WAVs.
// ---------------------------------------------------------------------------

/// Synthesize a placeholder mono PCM signal for one sound name.
fn synth_placeholder(name: &str) -> (Vec<f32>, u32) {
    let rate = 22_050u32;
    let pcm: Vec<f32> = match name {
        "dig" => {
            // Gritty noise burst with a fast decay — block breaking.
            let n = (rate as f32 * 0.13) as usize;
            (0..n)
                .map(|i| {
                    let t = i as f32 / n as f32;
                    let noise = hash_noise(i as i32, 17) * 2.0 - 1.0;
                    noise * (1.0 - t).powi(2) * 0.8
                })
                .collect()
        }
        "place" => {
            // Damped low knock.
            let n = (rate as f32 * 0.09) as usize;
            (0..n)
                .map(|i| {
                    let t = i as f32 / rate as f32;
                    (2.0 * std::f32::consts::PI * 170.0 * t).sin() * (-t * 34.0).exp() * 0.7
                })
                .collect()
        }
        _ => {
            // Step: soft short shuffle.
            let n = (rate as f32 * 0.07) as usize;
            (0..n)
                .map(|i| {
                    let t = i as f32 / n as f32;
                    (hash_noise(i as i32, 23) * 0.6 + 0.4 * (i % 97) as f32 / 97.0 - 0.5)
                        * (1.0 - t)
                        * 0.5
                })
                .collect()
        }
    };
    (pcm, rate)
}

/// Deterministic hash noise in 0..1.
fn hash_noise(x: i32, s: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(3_747_613_937) ^ ((s as u32).wrapping_mul(2_246_822_519));
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^= h >> 16;
    (h & 0xFFFF) as f32 / 65535.0
}

/// Encode mono float PCM as a 16-bit WAV file (for symphonia to decode).
pub fn wav_bytes(pcm: &[f32], sample_rate: u32) -> Vec<u8> {
    let mut samples = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        samples.extend_from_slice(&v.to_le_bytes());
    }
    let data_len = samples.len() as u32;
    let mut out = Vec::with_capacity(44 + samples.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&samples);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearby_sounds_are_loud_and_centered() {
        let (db, pan) = spatialize(Vec3::new(1.0, 0.0, 0.0), Vec3::ZERO, 0.0);
        assert!(db > -0.5, "near sound should be near 0 dB, got {db}");
        assert!(pan.abs() < 0.001);
    }

    #[test]
    fn far_sounds_are_attenuated() {
        let (db, _) = spatialize(Vec3::new(100.0, 0.0, 0.0), Vec3::ZERO, 0.0);
        assert!(db < -20.0, "far sound should be quiet, got {db}");
    }

    #[test]
    fn sounds_panned_by_camera_facing() {
        // Camera faces +X (yaw 0): a sound at +X is centered, a sound at
        // +Z is to the right (right vector is (0,1) in xz for yaw 0).
        let (_, pan_center) = spatialize(Vec3::new(10.0, 0.0, 0.0), Vec3::ZERO, 0.0);
        let (_, pan_right) = spatialize(Vec3::new(0.0, 0.0, 10.0), Vec3::ZERO, 0.0);
        assert!(pan_center.abs() < 0.001);
        assert!(pan_right > 0.9, "sound to the right should pan right, got {pan_right}");
    }

    #[test]
    fn synthesized_wav_is_a_valid_file() {
        let (pcm, rate) = synth_placeholder("dig");
        let bytes = wav_bytes(&pcm, rate);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        // kira/symphonia can decode it.
        let data = StaticSoundData::from_cursor(Cursor::new(bytes));
        assert!(data.is_ok(), "kira must decode the synthesized wav");
    }
}
