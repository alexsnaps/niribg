// SPDX-License-Identifier: GPL-3.0-or-later
//! Hex colour parsing for wallpaper fill / letterbox colours.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An opaque-by-default RGBA colour, parsed from `#RGB`, `#RRGGBB`, or
/// `#RRGGBBAA` (the leading `#` is optional).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };

    /// Premultiplied-alpha BGRA, the byte order of a `wl_shm`
    /// `Argb8888` / `Xrgb8888` buffer on little-endian.
    #[must_use]
    pub fn to_shm_bgra(self) -> [u8; 4] {
        let a = u16::from(self.a);
        let prem = |c: u8| ((u16::from(c) * a) / 255) as u8;
        [prem(self.b), prem(self.g), prem(self.r), self.a]
    }

    /// This colour with RGB scaled by `(1 - dim)` — the dimmed overview
    /// backdrop for a colour-only wallpaper.
    #[must_use]
    pub fn dimmed(self, dim: f64) -> Color {
        let k = 1.0 - dim.clamp(0.0, 1.0);
        let s = |c: u8| (f64::from(c) * k).round().clamp(0.0, 255.0) as u8;
        Color {
            r: s(self.r),
            g: s(self.g),
            b: s(self.b),
            a: self.a,
        }
    }
}

/// Why a colour string failed to parse. Hand-rolled to avoid a `thiserror`
/// dependency this early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseColorError {
    Length,
    Digit,
}

impl fmt::Display for ParseColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseColorError::Length => {
                f.write_str("expected a hex colour: #RGB, #RRGGBB, or #RRGGBBAA")
            }
            ParseColorError::Digit => f.write_str("colour contains a non-hex digit"),
        }
    }
}

impl std::error::Error for ParseColorError {}

impl FromStr for Color {
    type Err = ParseColorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let h = s.strip_prefix('#').unwrap_or(s);
        let byte = |i: usize| -> Result<u8, ParseColorError> {
            u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| ParseColorError::Digit)
        };
        let nybble = |i: usize| -> Result<u8, ParseColorError> {
            let v = u8::from_str_radix(&h[i..i + 1], 16).map_err(|_| ParseColorError::Digit)?;
            Ok(v << 4 | v)
        };
        match h.len() {
            3 => Ok(Color {
                r: nybble(0)?,
                g: nybble(1)?,
                b: nybble(2)?,
                a: 255,
            }),
            6 => Ok(Color {
                r: byte(0)?,
                g: byte(2)?,
                b: byte(4)?,
                a: 255,
            }),
            8 => Ok(Color {
                r: byte(0)?,
                g: byte(2)?,
                b: byte(4)?,
                a: byte(6)?,
            }),
            _ => Err(ParseColorError::Length),
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.a == 255 {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(
                f,
                "#{:02x}{:02x}{:02x}{:02x}",
                self.r, self.g, self.b, self.a
            )
        }
    }
}

impl Default for Color {
    fn default() -> Self {
        Color::BLACK
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_lengths() {
        assert_eq!(
            "#fff".parse::<Color>().unwrap(),
            Color {
                r: 255,
                g: 255,
                b: 255,
                a: 255
            }
        );
        assert_eq!(
            "#1e1e2e".parse::<Color>().unwrap(),
            Color {
                r: 0x1e,
                g: 0x1e,
                b: 0x2e,
                a: 255
            }
        );
        assert_eq!(
            "#1e1e2e80".parse::<Color>().unwrap(),
            Color {
                r: 0x1e,
                g: 0x1e,
                b: 0x2e,
                a: 0x80
            }
        );
    }

    #[test]
    fn hash_is_optional() {
        assert_eq!("000000".parse::<Color>().unwrap(), Color::BLACK);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(
            "#12345".parse::<Color>(),
            Err(ParseColorError::Length)
        ));
        assert!(matches!(
            "#gggggg".parse::<Color>(),
            Err(ParseColorError::Digit)
        ));
        assert!("".parse::<Color>().is_err());
    }

    #[test]
    fn display_roundtrips() {
        for s in ["#1e1e2e", "#1e1e2e80"] {
            let c: Color = s.parse().unwrap();
            assert_eq!(c.to_string(), s);
            assert_eq!(c.to_string().parse::<Color>().unwrap(), c);
        }
        assert_eq!("#abc".parse::<Color>().unwrap().to_string(), "#aabbcc");
    }

    #[test]
    fn shm_bgra_is_premultiplied() {
        assert_eq!(Color::BLACK.to_shm_bgra(), [0, 0, 0, 255]);
        assert_eq!(
            Color {
                r: 255,
                g: 128,
                b: 0,
                a: 255
            }
            .to_shm_bgra(),
            [0, 128, 255, 255]
        );
        // half alpha halves the colour channels
        assert_eq!(
            Color {
                r: 255,
                g: 255,
                b: 255,
                a: 128
            }
            .to_shm_bgra(),
            [128, 128, 128, 128]
        );
    }

    #[test]
    fn deserializes_from_toml() {
        #[derive(serde::Deserialize)]
        struct W {
            c: Color,
        }
        let w: W = toml::from_str(r##"c = "#1e1e2e""##).unwrap();
        assert_eq!(w.c, "#1e1e2e".parse().unwrap());
    }
}
