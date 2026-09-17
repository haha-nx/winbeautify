//! Geometry helpers shared by every WinBeautify module.
//!
//! Coordinates are always *physical pixels* in virtual-screen space, which is
//! what the Win32 / DWM APIs hand back. Anything that needs logical pixels does
//! its own DPI conversion.

use serde::{Deserialize, Serialize};

/// A screen-space rectangle, mirroring `Win32::Foundation::RECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub const fn width(&self) -> i32 {
        self.right - self.left
    }

    pub const fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub const fn is_empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    /// True when the two rectangles share at least one pixel.
    pub fn intersects(&self, other: &Rect) -> bool {
        self.left < other.right
            && other.left < self.right
            && self.top < other.bottom
            && other.top < self.bottom
    }

    /// Intersection area in square pixels; `0` when the rectangles are disjoint.
    pub fn intersection_area(&self, other: &Rect) -> i64 {
        let w = (self.right.min(other.right) - self.left.max(other.left)).max(0) as i64;
        let h = (self.bottom.min(other.bottom) - self.top.max(other.top)).max(0) as i64;
        w * h
    }

    /// Shrink by `n` pixels on every side, clamping to a degenerate rect.
    pub fn deflate(&self, n: i32) -> Rect {
        Rect {
            left: self.left + n,
            top: self.top + n,
            right: (self.right - n).max(self.left + n),
            bottom: (self.bottom - n).max(self.top + n),
        }
    }

    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect {
            left: self.left + dx,
            top: self.top + dy,
            right: self.right + dx,
            bottom: self.bottom + dy,
        }
    }
}

/// An sRGB colour, serialised to config files as `"#RRGGBB"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Pack into the `0x00BBGGRR` layout that Win32 `COLORREF` and the DWM
    /// accent structs expect (note the byte order is *not* RGB).
    pub const fn to_bgr_u32(&self) -> u32 {
        (self.b as u32) << 16 | (self.g as u32) << 8 | self.r as u32
    }

    pub const fn to_rgb_u32(&self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }
}

impl Default for Color {
    fn default() -> Self {
        Self::rgb(0x14, 0x16, 0x1c)
    }
}

impl std::fmt::Display for Color {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

impl std::str::FromStr for Color {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let h = s.trim().trim_start_matches('#');
        let expanded;
        let h = match h.len() {
            3 => {
                // #abc -> #aabbcc
                expanded = h.chars().flat_map(|c| [c, c]).collect::<String>();
                expanded.as_str()
            }
            6 => h,
            other => return Err(format!("expected #RRGGBB, got {other} hex digits")),
        };
        let parse = |i: usize| {
            u8::from_str_radix(&h[i..i + 2], 16).map_err(|e| format!("invalid hex: {e}"))
        };
        Ok(Color::rgb(parse(0)?, parse(2)?, parse(4)?))
    }
}

impl Serialize for Color {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Accept both `"#RRGGBB"` and the `{ r, g, b }` table form so hand-edited
        // configs stay forgiving.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Hex(String),
            Table { r: u8, g: u8, b: u8 },
        }
        match Repr::deserialize(d)? {
            Repr::Hex(s) => s.parse().map_err(serde::de::Error::custom),
            Repr::Table { r, g, b } => Ok(Color::rgb(r, g, b)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_and_long_hex() {
        assert_eq!("#fff".parse::<Color>().unwrap(), Color::rgb(255, 255, 255));
        assert_eq!(
            "0a0B0c".parse::<Color>().unwrap(),
            Color::rgb(0x0a, 0x0b, 0x0c)
        );
        assert!("nope".parse::<Color>().is_err());
    }

    #[test]
    fn colorref_byte_order_is_bbggrr() {
        assert_eq!(Color::rgb(0x11, 0x22, 0x33).to_bgr_u32(), 0x0033_2211);
    }

    #[test]
    fn intersection_area_of_disjoint_rects_is_zero() {
        let a = Rect::new(0, 0, 10, 10);
        assert_eq!(a.intersection_area(&Rect::new(20, 20, 30, 30)), 0);
        assert_eq!(a.intersection_area(&Rect::new(5, 5, 30, 30)), 25);
    }
}
