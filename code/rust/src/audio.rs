//! Audio output: SDL's device, with a mixer callback that plays the sounds the
//! client hands it.
//!
//! The device opens at the engine's mixing format, and every sound arrives
//! already resampled to it (see [`crate::wav`]), so the callback only has to
//! add each voice into the buffer and retire the ones that finished. Voices are
//! shared rather than copied: a machinegun shot is the same decoded buffer
//! played again.

use std::sync::{Arc, Mutex};

use sdl2::audio::{AudioCallback, AudioDevice, AudioSpec, AudioSpecDesired};
use sdl2::{AudioSubsystem, Sdl};

/// Sample rate the engine mixes at, matching `dma.speed` on a modern host.
pub const SAMPLE_RATE: i32 = 48_000;
/// Channels in the output stream.
pub const CHANNELS: u8 = 2;
/// Samples per callback.
pub const BUFFER_SAMPLES: u16 = 512;
/// How many voices may be in flight at once, so a held trigger cannot grow the
/// mix without bound.
const MAX_VOICES: usize = 32;
/// How loud the mix is, leaving room for several voices at once before the sum
/// clips.
const VOLUME: f32 = 0.35;
/// Inside this distance a sound plays at full strength.
const NEAR_DISTANCE: f32 = 64.0;
/// A sound never falls entirely silent with distance, so something audible
/// still tells the player what is happening.
const MIN_GAIN: f32 = 0.05;

/// How loud a sound at `at` is to a listener at `listener`: full strength
/// nearby, falling off with distance after that. The engine's own attenuation
/// weights a speaker's falloff per sound; this is the plain inverse-distance
/// curve the client uses until that table is ported.
/// Which ear a sound at `at` leans toward for a listener at `listener` facing
/// `yaw`: +1 hard right, -1 hard left, 0 dead ahead or behind. Yaw grows to the
/// left, so the right-hand vector is `[sin yaw, -cos yaw]`.
pub fn pan_at(listener: [f32; 3], yaw: f32, at: [f32; 3]) -> f32 {
    let delta = [at[0] - listener[0], at[1] - listener[1]];
    let length = (delta[0] * delta[0] + delta[1] * delta[1]).sqrt();
    if length < 1.0 {
        return 0.0;
    }
    let (sin, cos) = yaw.to_radians().sin_cos();
    let right = [sin, -cos];
    ((delta[0] / length) * right[0] + (delta[1] / length) * right[1]).clamp(-1.0, 1.0)
}

pub fn gain_at(listener: [f32; 3], at: [f32; 3]) -> f32 {
    let delta = [
        at[0] - listener[0],
        at[1] - listener[1],
        at[2] - listener[2],
    ];
    let distance = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
    if distance <= NEAR_DISTANCE {
        1.0
    } else {
        (NEAR_DISTANCE / distance).max(MIN_GAIN)
    }
}

/// A sound that is playing, and how far through it is.
struct Voice {
    samples: Arc<Vec<f32>>,
    at: usize,
    /// How far away the sound was when it started, already folded in.
    gain: f32,
    /// Which ear it leans toward, from -1 (left) to +1 (right).
    pan: f32,
}

/// What the audio thread reads: the voices still playing.
#[derive(Default)]
struct MixState {
    voices: Vec<Voice>,
    listener: [f32; 3],
    /// The listener's facing, for how sounds are placed left and right.
    yaw: f32,
}

impl MixState {
    /// Adds every voice into `out` and drops the finished ones. `out` is
    /// interleaved stereo; a voice whose buffer has run out is silent.
    fn mix_into(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        for voice in &mut self.voices {
            let remaining = voice.samples.len().saturating_sub(voice.at);
            let count = remaining.min(out.len());
            let mix = VOLUME * voice.gain;
            let left = mix * (1.0 - voice.pan).min(1.0);
            let right = mix * (1.0 + voice.pan).min(1.0);
            for (offset, (slot, sample)) in out
                .iter_mut()
                .zip(&voice.samples[voice.at..voice.at + count])
                .enumerate()
            {
                *slot += sample * if offset % 2 == 0 { left } else { right };
            }
            voice.at += count;
        }
        self.voices.retain(|voice| voice.at < voice.samples.len());
    }
}

/// The callback the device calls; it shares the voices with the client.
struct Mixer {
    state: Arc<Mutex<MixState>>,
}

impl AudioCallback for Mixer {
    type Channel = f32;

    fn callback(&mut self, out: &mut [f32]) {
        if let Ok(mut state) = self.state.lock() {
            state.mix_into(out);
        } else {
            out.fill(0.0);
        }
    }
}

pub struct Audio {
    // The subsystem owns the device, so it has to outlive it.
    _subsystem: AudioSubsystem,
    device: AudioDevice<Mixer>,
    state: Arc<Mutex<MixState>>,
}

impl Audio {
    /// Opens the playback device. A host without audio returns an error rather
    /// than stopping the client.
    pub fn init(sdl: &Sdl) -> Result<Self, String> {
        let subsystem = sdl.audio()?;
        let desired = AudioSpecDesired {
            freq: Some(SAMPLE_RATE),
            channels: Some(CHANNELS),
            samples: Some(BUFFER_SAMPLES),
        };
        let state = Arc::new(Mutex::new(MixState::default()));
        let shared = Arc::clone(&state);
        let device = subsystem
            .open_playback(None, &desired, move |_spec| Mixer { state: shared })
            .map_err(|e| format!("opening the audio device: {e}"))?;
        device.resume();
        Ok(Audio {
            _subsystem: subsystem,
            device,
            state,
        })
    }

    /// The format the device actually opened with.
    pub fn spec(&self) -> &AudioSpec {
        self.device.spec()
    }

    /// Moves the ear to `at` and turns it to `yaw`, which is where the player's
    /// camera is.
    pub fn set_listener(&self, at: [f32; 3], yaw: f32) {
        if let Ok(mut state) = self.state.lock() {
            state.listener = at;
            state.yaw = yaw;
        }
    }

    /// Starts a decoded sound from `at`. The buffer is shared with every other
    /// play of the same sound, and a full mix drops the request rather than
    /// growing.
    pub fn play_at(&self, samples: Arc<Vec<f32>>, at: [f32; 3]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.voices.len() >= MAX_VOICES {
            return;
        }
        let gain = gain_at(state.listener, at);
        let pan = pan_at(state.listener, state.yaw, at);
        state.voices.push(Voice {
            samples,
            at: 0,
            gain,
            pan,
        });
    }

    /// A sound with no position, played at the listener's feet.
    pub fn play(&self, samples: Arc<Vec<f32>>) {
        self.play_at(samples, [0.0; 3]);
    }

    /// How many voices are still playing, for the console's report.
    pub fn voices(&self) -> usize {
        self.state.lock().map(|state| state.voices.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(samples: &[f32]) -> Voice {
        Voice {
            samples: Arc::new(samples.to_vec()),
            at: 0,
            gain: 1.0,
            pan: 0.0,
        }
    }

    #[test]
    fn a_sound_falls_off_with_distance_but_never_vanishes() {
        let listener = [0.0, 0.0, 0.0];
        assert_eq!(gain_at(listener, [0.0; 3]), 1.0);
        assert_eq!(gain_at(listener, [64.0, 0.0, 0.0]), 1.0);
        let far = gain_at(listener, [1_280.0, 0.0, 0.0]);
        assert!((far - 0.05).abs() < 0.001, "{far}");
        let mid = gain_at(listener, [128.0, 0.0, 0.0]);
        assert!((mid - 0.5).abs() < 0.001, "{mid}");
        assert!(gain_at(listener, [0.0, 0.0, 100_000.0]) > 0.0);
    }

    #[test]
    fn voices_add_into_the_buffer_and_retire_when_done() {
        let mut state = MixState::default();
        state.voices.push(voice(&[0.5; 6]));
        state.voices.push(voice(&[0.1, -0.1]));

        let mut out = [0.0f32; 4];
        state.mix_into(&mut out);
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        assert!(close(out[0], (0.5 + 0.1) * VOLUME), "{}", out[0]);
        assert!(close(out[1], (0.5 - 0.1) * VOLUME), "{}", out[1]);
        assert!(close(out[2], 0.5 * VOLUME), "{}", out[2]);
        assert!(close(out[3], 0.5 * VOLUME), "{}", out[3]);
        // The short voice has finished; the long one has two samples left.
        assert_eq!(state.voices.len(), 1);

        state.mix_into(&mut out);
        assert!(close(out[0], 0.5 * VOLUME), "{}", out[0]);
        assert!(close(out[1], 0.5 * VOLUME), "{}", out[1]);
        assert!(out[2..].iter().all(|sample| *sample == 0.0));
        assert!(state.voices.is_empty());
    }

    #[test]
    fn a_sound_to_the_side_leans_toward_that_ear() {
        let listener = [0.0; 3];
        // Facing +x, so the listener's right hand points to -y.
        assert_eq!(pan_at(listener, 0.0, [0.0, -100.0, 0.0]), 1.0);
        assert_eq!(pan_at(listener, 0.0, [0.0, 100.0, 0.0]), -1.0);
        assert_eq!(pan_at(listener, 0.0, [100.0, 0.0, 0.0]), 0.0);
        assert_eq!(pan_at(listener, 0.0, [0.0, 0.0, 0.0]), 0.0);
        // Turning to face the sound puts it in the middle again.
        assert!(pan_at(listener, 90.0, [0.0, 100.0, 0.0]).abs() < 1e-6);
    }

    #[test]
    fn a_panned_voice_reaches_one_ear() {
        let mut state = MixState::default();
        state.voices.push(Voice {
            samples: Arc::new(vec![1.0; 4]),
            at: 0,
            gain: 1.0,
            pan: 1.0,
        });
        let mut out = [0.0f32; 4];
        state.mix_into(&mut out);
        assert_eq!(out[0], 0.0, "left ear is silent for a hard-right sound");
        assert!((out[1] - VOLUME).abs() < 1e-6, "{}", out[1]);
        assert_eq!(out[2], 0.0);
        assert!((out[3] - VOLUME).abs() < 1e-6, "{}", out[3]);
    }

    #[test]
    fn silence_with_no_voices() {
        let mut state = MixState::default();
        let mut out = [1.0f32; 8];
        state.mix_into(&mut out);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }
}
