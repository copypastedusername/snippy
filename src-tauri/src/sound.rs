//! The optional sound played when a snip is taken. Off by default, like Snipping Tool.
//!
//! "Camera shutter" is synthesised, so it works everywhere. The others are Windows' own UI sounds,
//! read from `%WINDIR%\Media` at runtime (never shipped) and re-pitched; a trimmed Windows without
//! them falls back to the shutter.

use serde::{Deserialize, Deserializer, Serialize};
use std::sync::{Mutex, OnceLock};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT, SND_SYSTEM};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureSound {
    #[default]
    Off,
    Shutter,
    Click,
    ClickHigh,
    Tick,
    Ding,
}

impl CaptureSound {
    const ALL: [Self; 6] = [
        Self::Off,
        Self::Shutter,
        Self::Click,
        Self::ClickHigh,
        Self::Tick,
        Self::Ding,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shutter => "shutter",
            Self::Click => "click",
            Self::ClickHigh => "click-high",
            Self::Tick => "tick",
            Self::Ding => "ding",
        }
    }

    /// The Windows sound this preset re-pitches, and the pitch factor (2.0 = an octave up).
    fn windows_source(self) -> Option<(&'static str, f64)> {
        match self {
            Self::Click => Some(("Windows Navigation Start.wav", 1.0)),
            Self::ClickHigh => Some(("Windows Navigation Start.wav", 1.5)),
            Self::Tick => Some(("Windows Menu Command.wav", 1.6)),
            Self::Ding => Some(("Windows Ding.wav", 1.5)),
            Self::Off | Self::Shutter => None,
        }
    }
}

impl std::str::FromStr for CaptureSound {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|sound| sound.as_str() == value)
            .ok_or_else(|| format!("Unknown capture sound: {value}"))
    }
}

impl<'de> Deserialize<'de> for CaptureSound {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(value.parse().unwrap_or_default())
    }
}

/// Plays the sound without blocking. Failures are logged; a snip never fails because of its sound.
pub fn play(sound: CaptureSound) {
    if sound == CaptureSound::Off {
        return;
    }
    let wav = clip(sound);
    // SAFETY: with SND_MEMORY the pointer is the WAV image itself, which is 'static, so it outlives
    // the asynchronous playback. SND_SYSTEM follows the "System sounds" volume.
    let played = unsafe {
        PlaySoundW(
            PCWSTR(wav.as_ptr().cast()),
            HMODULE::default(),
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT | SND_SYSTEM,
        )
    };
    if !played.as_bool() {
        log::warn!("Could not play the {} capture sound", sound.as_str());
    }
}

/// Built clips, by preset; each is leaked once so asynchronous playback can keep reading it.
type ClipCache = Mutex<Vec<(CaptureSound, &'static [u8])>>;

/// Builds each clip once and keeps it for the life of the process (a handful of small buffers).
fn clip(sound: CaptureSound) -> &'static [u8] {
    static CLIPS: OnceLock<ClipCache> = OnceLock::new();
    let clips = CLIPS.get_or_init(|| Mutex::new(Vec::new()));
    let mut clips = clips
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, wav)) = clips.iter().find(|(cached, _)| *cached == sound) {
        return wav;
    }
    let wav: &'static [u8] = Box::leak(build(sound).into_boxed_slice());
    clips.push((sound, wav));
    wav
}

fn build(sound: CaptureSound) -> Vec<u8> {
    let Some((file, pitch)) = sound.windows_source() else {
        return shutter();
    };
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let path = std::path::Path::new(&windir).join("Media").join(file);
    match std::fs::read(&path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| repitch(&bytes, pitch))
    {
        Ok(wav) => wav,
        Err(error) => {
            log::info!(
                "{} is unavailable ({error}); using the shutter",
                path.display()
            );
            shutter()
        }
    }
}

const RATE: u32 = 44_100;

/// A mechanical shutter: two short, decaying bursts of filtered noise over a low thump.
fn shutter() -> Vec<u8> {
    const LENGTH_MS: u32 = 150;
    const SECOND_CLICK_MS: f64 = 62.0;
    let frames = (RATE * LENGTH_MS / 1000) as usize;
    let mut seed: u32 = 0x9E37_79B9;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        f64::from(seed) / f64::from(u32::MAX) * 2.0 - 1.0
    };
    let mut smoothed = 0.0;
    let samples: Vec<i16> = (0..frames)
        .map(|i| {
            let t = i as f64 / f64::from(RATE) * 1000.0; // milliseconds
            smoothed = smoothed * 0.55 + noise() * 0.45;
            let burst = |start: f64, decay: f64, gain: f64| {
                if t < start {
                    0.0
                } else {
                    gain * (-(t - start) / decay).exp()
                }
            };
            let envelope = burst(0.0, 4.0, 1.0) + burst(SECOND_CLICK_MS, 6.0, 0.8);
            let thump =
                (t * 0.001 * 2.0 * std::f64::consts::PI * 140.0).sin() * burst(0.0, 18.0, 0.35);
            let value = (smoothed * envelope + thump) * 0.55;
            (value.clamp(-1.0, 1.0) * f64::from(i16::MAX)) as i16
        })
        .collect();
    wav(1, RATE, &samples)
}

struct Pcm {
    channels: u16,
    rate: u32,
    samples: Vec<i16>,
}

/// Reads a 16-bit PCM WAV (the format of Windows' UI sounds).
fn parse(bytes: &[u8]) -> Result<Pcm, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".to_owned());
    }
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let mut format = None;
    let mut data = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(at + 4) as usize;
        let body = at + 8;
        let end = body.checked_add(size).filter(|end| *end <= bytes.len());
        let Some(end) = end else { break };
        match id {
            b"fmt " if size >= 16 => {
                format = Some((
                    u16_at(body),
                    u16_at(body + 2),
                    u32_at(body + 4),
                    u16_at(body + 14),
                ));
            }
            b"data" => data = Some(&bytes[body..end]),
            _ => {}
        }
        at = end + (size & 1);
    }
    let (tag, channels, rate, bits) = format.ok_or("no fmt chunk")?;
    // 1 = PCM, 0xFFFE = WAVE_FORMAT_EXTENSIBLE (PCM in every Windows UI sound).
    if !(tag == 1 || tag == 0xFFFE) || bits != 16 || channels == 0 || rate == 0 {
        return Err(format!("unsupported format {tag:#x}, {bits}-bit"));
    }
    let data = data.ok_or("no data chunk")?;
    let samples = data
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Ok(Pcm {
        channels,
        rate,
        samples,
    })
}

/// Shifts pitch by resampling (shorter and higher above 1.0), keeping the original sample rate.
fn repitch(bytes: &[u8], pitch: f64) -> Result<Vec<u8>, String> {
    let pcm = parse(bytes)?;
    let channels = usize::from(pcm.channels);
    let frames = pcm.samples.len() / channels;
    if frames < 2 {
        return Err("empty sound".to_owned());
    }
    let out_frames = ((frames - 1) as f64 / pitch) as usize;
    let mut samples = Vec::with_capacity(out_frames * channels);
    for frame in 0..out_frames {
        let position = frame as f64 * pitch;
        let index = position as usize;
        let fraction = position - index as f64;
        for channel in 0..channels {
            let a = f64::from(pcm.samples[index * channels + channel]);
            let b = f64::from(pcm.samples[(index + 1) * channels + channel]);
            samples.push((a + (b - a) * fraction) as i16);
        }
    }
    Ok(wav(pcm.channels, pcm.rate, &samples))
}

fn wav(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let block_align = channels * 2;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16_u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_unknown_values_turn_the_sound_off() {
        for sound in CaptureSound::ALL {
            assert_eq!(sound.as_str().parse::<CaptureSound>(), Ok(sound));
        }
        let parsed: CaptureSound = serde_json::from_str("\"bogus\"").unwrap();
        assert_eq!(parsed, CaptureSound::Off);
        assert_eq!(
            serde_json::to_string(&CaptureSound::ClickHigh).unwrap(),
            "\"click-high\""
        );
    }

    #[test]
    fn the_shutter_is_a_valid_short_wav() {
        let pcm = parse(&shutter()).unwrap();
        assert_eq!((pcm.channels, pcm.rate), (1, RATE));
        assert!(pcm
            .samples
            .iter()
            .any(|sample| sample.unsigned_abs() > 1000));
        assert!(pcm.samples.len() < RATE as usize / 4);
    }

    #[test]
    fn repitching_up_an_octave_halves_the_length_and_keeps_the_rate() {
        let source = wav(2, 48_000, &[100; 2 * 1000]);
        let pcm = parse(&repitch(&source, 2.0).unwrap()).unwrap();
        assert_eq!((pcm.channels, pcm.rate), (2, 48_000));
        assert_eq!(pcm.samples.len(), 2 * 499);
        assert!(pcm.samples.iter().all(|sample| *sample == 100));
    }

    #[test]
    fn broken_files_are_rejected_not_played() {
        assert!(parse(b"RIFF\0\0\0\0WAVE").is_err());
        assert!(parse(b"nonsense").is_err());
        let mut truncated = wav(1, RATE, &[1, 2, 3]);
        truncated.truncate(30);
        assert!(parse(&truncated).is_err());
    }

    #[test]
    #[ignore = "plays every preset through the speakers"]
    fn windows_accepts_and_plays_every_preset() {
        for sound in CaptureSound::ALL
            .into_iter()
            .filter(|s| *s != CaptureSound::Off)
        {
            let wav = clip(sound);
            // SAFETY: synchronous playback of a 'static WAV image.
            let played = unsafe {
                PlaySoundW(
                    PCWSTR(wav.as_ptr().cast()),
                    HMODULE::default(),
                    SND_MEMORY | SND_NODEFAULT,
                )
            };
            assert!(played.as_bool(), "{sound:?}");
        }
    }

    #[test]
    fn every_preset_builds_a_playable_clip() {
        for sound in CaptureSound::ALL
            .into_iter()
            .filter(|s| *s != CaptureSound::Off)
        {
            assert!(parse(&build(sound)).is_ok(), "{sound:?}");
        }
    }
}
