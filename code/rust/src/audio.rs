//! Audio output, from SDL's audio device.
//!
//! The device is opened at the sample rate the engine mixes at and the
//! callback currently writes silence: the mixer that fills these buffers is
//! the next subsystem, but the device, its format and its thread are real.

use sdl2::audio::{AudioCallback, AudioDevice, AudioSpec, AudioSpecDesired};
use sdl2::{AudioSubsystem, Sdl};

/// Sample rate the engine mixes at, matching `dma.speed` on a modern host.
pub const SAMPLE_RATE: i32 = 48_000;
/// Channels in the output stream.
pub const CHANNELS: u8 = 2;
/// Samples per callback.
pub const BUFFER_SAMPLES: u16 = 512;

/// The callback the mixer will eventually own.
struct Silence;

impl AudioCallback for Silence {
    type Channel = f32;

    fn callback(&mut self, out: &mut [f32]) {
        out.fill(0.0);
    }
}

pub struct Audio {
    // The subsystem owns the device, so it has to outlive it.
    _subsystem: AudioSubsystem,
    device: AudioDevice<Silence>,
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
        let device = subsystem
            .open_playback(None, &desired, |_spec| Silence)
            .map_err(|e| format!("opening the audio device: {e}"))?;
        device.resume();
        Ok(Audio {
            _subsystem: subsystem,
            device,
        })
    }

    /// The format the device actually opened with.
    pub fn spec(&self) -> &AudioSpec {
        self.device.spec()
    }
}
