//! How games show things, as data the agent writes: particle effects, sounds made from
//! a few numbers (no sound files), the sky, and what a hero looks like. Players' games
//! draw and play them; nothing here needs a file or an asset.

use serde::{Deserialize, Serialize};

/// A burst of particles.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Fx {
    /// "#rrggbb".
    pub color: String,
    /// A second color they fade to.
    pub to: Option<String>,
    pub count: u32,
    /// m/s outward.
    pub speed: f32,
    /// Meters across.
    pub size: f32,
    /// Seconds each lasts.
    pub life: f32,
    /// 0: straight up in a jet; 1: every way.
    pub spread: f32,
    /// m/s² downward (negative floats up).
    pub gravity: f32,
    pub glow: bool,
}

impl Default for Fx {
    fn default() -> Self {
        Self {
            color: "#ffd88a".into(),
            to: None,
            count: 20,
            speed: 3.0,
            size: 0.12,
            life: 0.8,
            spread: 1.0,
            gravity: 4.0,
            glow: true,
        }
    }
}

/// A short sound: a tone that slides from `freq` to `to`, or a tune of `notes`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Sound {
    /// "sine", "square", "triangle", "saw" or "noise".
    pub wave: String,
    /// Hz.
    pub freq: f32,
    /// Hz it slides to by the end.
    pub to: Option<f32>,
    /// Seconds (each note's, for a tune).
    pub length: f32,
    /// 0..1.
    pub volume: f32,
    /// Seconds to fade in.
    pub attack: f32,
    /// How it fades out: 0 stops flat, 1 dies away like a bell.
    pub decay: f32,
    /// A tune instead: MIDI note numbers (60 is middle C), one after another.
    pub notes: Vec<u8>,
}

impl Default for Sound {
    fn default() -> Self {
        Self {
            wave: "sine".into(),
            freq: 440.0,
            to: None,
            length: 0.25,
            volume: 0.5,
            attack: 0.005,
            decay: 0.6,
            notes: Vec::new(),
        }
    }
}

/// How the sky looks while a game says so. Unset parts stay the world's own.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Sky {
    /// Hold the time of day here, 0..1 (0.5 noon, 0 midnight), as players see it.
    pub time: Option<f32>,
    /// 0 clear to 1 thick.
    pub fog: Option<f32>,
    /// "#rrggbb": the fog's color.
    pub fog_color: Option<String>,
    /// The sun's strength, as a multiple of the usual (0..2).
    pub sun: Option<f32>,
    /// "rain" or "snow".
    pub weather: Option<String>,
}

/// What a game shows a hero as: one of its models, in place of the usual one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Appearance {
    /// The package's hash, and the model.
    #[serde(default)]
    pub package: String,
    pub model: String,
    #[serde(default = "one")]
    pub scale: f32,
}

fn one() -> f32 {
    1.0
}

/// Something shown or played once, where it happened.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Show {
    pub change: u64,
    pub pos: [f32; 3],
    /// Who sees it: everyone, or one player.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub what: Shown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shown {
    Effect(Fx),
    Sound(Sound),
}

pub fn is_color(text: &str) -> bool {
    text.len() == 7 && text.starts_with('#') && text[1..].chars().all(|c| c.is_ascii_hexdigit())
}

impl Fx {
    pub fn check(&self) -> Result<(), String> {
        let ok = is_color(&self.color)
            && self.to.as_deref().is_none_or(is_color)
            && (1..=200).contains(&self.count)
            && (0.0..=40.0).contains(&self.speed)
            && (0.01..=2.0).contains(&self.size)
            && (0.05..=10.0).contains(&self.life)
            && (0.0..=1.0).contains(&self.spread)
            && (-40.0..=40.0).contains(&self.gravity);
        if ok {
            Ok(())
        } else {
            Err("effect(pos, {color = \"#rrggbb\", to = \"#rrggbb\", count = 1-200, speed = 0-40 m/s, size = 0.01-2 m, life = 0.05-10 s, spread = 0-1, gravity = -40-40, glow = true})".into())
        }
    }
}

impl Sound {
    pub fn check(&self) -> Result<(), String> {
        let ok = ["sine", "square", "triangle", "saw", "noise"].contains(&self.wave.as_str())
            && (20.0..=8000.0).contains(&self.freq)
            && self.to.is_none_or(|t| (20.0..=8000.0).contains(&t))
            && (0.02..=4.0).contains(&self.length)
            && (0.0..=1.0).contains(&self.volume)
            && (0.0..=1.0).contains(&self.attack)
            && (0.0..=1.0).contains(&self.decay)
            && self.notes.len() <= 32
            && self.notes.iter().all(|n| (24..=108).contains(n));
        if ok {
            Ok(())
        } else {
            Err("sound(pos, {wave = \"sine\"|\"square\"|\"triangle\"|\"saw\"|\"noise\", freq = 20-8000 Hz, to = Hz, length = 0.02-4 s, volume = 0-1, attack = 0-1 s, decay = 0-1, notes = {60, 64, 67} (MIDI, up to 32)})".into())
        }
    }

    /// The sound as 16-bit mono samples at `rate`.
    pub fn samples(&self, rate: u32) -> Vec<i16> {
        let freqs: Vec<(f32, f32)> = if self.notes.is_empty() {
            vec![(self.freq, self.to.unwrap_or(self.freq))]
        } else {
            self.notes
                .iter()
                .map(|n| 440.0 * 2f32.powf((*n as f32 - 69.0) / 12.0))
                .map(|f| (f, f))
                .collect()
        };
        let per = (self.length * rate as f32) as usize;
        let mut out = Vec::with_capacity(per * freqs.len());
        let mut phase = 0.0f32;
        let mut noise = 0x1234_5678u32;
        for (from, to) in freqs {
            for i in 0..per {
                let t = i as f32 / per as f32;
                let f = from + (to - from) * t;
                phase = (phase + f / rate as f32).fract();
                let wave = match self.wave.as_str() {
                    "square" => {
                        if phase < 0.5 {
                            1.0
                        } else {
                            -1.0
                        }
                    }
                    "triangle" => 4.0 * (phase - 0.5).abs() - 1.0,
                    "saw" => 2.0 * phase - 1.0,
                    "noise" => {
                        noise ^= noise << 13;
                        noise ^= noise >> 17;
                        noise ^= noise << 5;
                        noise as f32 / u32::MAX as f32 * 2.0 - 1.0
                    }
                    _ => (phase * std::f32::consts::TAU).sin(),
                };
                let seconds = i as f32 / rate as f32;
                let rise = if self.attack > 0.0 {
                    (seconds / self.attack).min(1.0)
                } else {
                    1.0
                };
                // Dies away exponentially with decay, and always ends quiet (no click).
                let fall = (1.0 - t).powf(1.0 + self.decay * 4.0);
                out.push((wave * rise * fall * self.volume * 0.8 * i16::MAX as f32) as i16);
            }
        }
        out
    }

    /// The sound as a WAV file's bytes.
    pub fn wav(&self) -> Vec<u8> {
        const RATE: u32 = 22_050;
        let samples = self.samples(RATE);
        let data = (samples.len() * 2) as u32;
        let mut out = Vec::with_capacity(44 + data as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&RATE.to_le_bytes());
        out.extend_from_slice(&(RATE * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }
}

impl Sky {
    pub fn check(&self) -> Result<(), String> {
        let ok = self.time.is_none_or(|t| (0.0..=1.0).contains(&t))
            && self.fog.is_none_or(|f| (0.0..=1.0).contains(&f))
            && self.fog_color.as_deref().is_none_or(is_color)
            && self.sun.is_none_or(|s| (0.0..=2.0).contains(&s))
            && self
                .weather
                .as_deref()
                .is_none_or(|w| w == "rain" || w == "snow");
        if ok {
            Ok(())
        } else {
            Err("sky({time = 0-1 (0.5 noon), fog = 0-1, fog_color = \"#rrggbb\", sun = 0-2, weather = \"rain\"|\"snow\"}); sky(nil) puts it back".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sounds_are_made_from_numbers() {
        let bell = Sound {
            notes: vec![72, 76, 79],
            length: 0.2,
            ..Default::default()
        };
        assert!(bell.check().is_ok());
        let wav = bell.wav();
        assert_eq!(&wav[..4], b"RIFF");
        // Three notes of 0.2 s at 22 050 Hz, 2 bytes each, after a 44-byte header.
        assert_eq!(wav.len(), 44 + 3 * 4410 * 2);
        assert!(
            Sound {
                wave: "kazoo".into(),
                ..Default::default()
            }
            .check()
            .is_err()
        );
        assert!(
            Fx {
                count: 500,
                ..Default::default()
            }
            .check()
            .is_err()
        );
        assert!(
            Sky {
                weather: Some("rain".into()),
                ..Default::default()
            }
            .check()
            .is_ok()
        );
    }
}
