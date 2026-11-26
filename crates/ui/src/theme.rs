//! Colors, fonts and metrics in one place.

use gpui::{Hsla, Rgba, rgb};

pub const ROW_H: f32 = 24.0;
pub const DIFF_ROW_H: f32 = 19.0;
pub const FILE_ROW_H: f32 = 24.0;
pub const LANE_W: f32 = 14.0;
/// Lanes drawn before the graph column clips (linux.git can be thousands
/// wide; the column is a viewport onto them).
pub const MAX_GRAPH_LANES: u16 = 14;
pub const TEXT_SIZE: f32 = 13.0;
pub const MONO_SIZE: f32 = 12.5;

#[cfg(target_os = "macos")]
pub const UI_FONT: &str = "Helvetica Neue";
#[cfg(target_os = "macos")]
pub const MONO_FONT: &str = "Menlo";
#[cfg(target_os = "windows")]
pub const UI_FONT: &str = "Segoe UI";
#[cfg(target_os = "windows")]
pub const MONO_FONT: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const UI_FONT: &str = "Inter";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const MONO_FONT: &str = "DejaVu Sans Mono";

pub fn bg() -> Rgba {
    rgb(0x1e2127)
}
pub fn panel() -> Rgba {
    rgb(0x23272e)
}
pub fn header() -> Rgba {
    rgb(0x2b3038)
}
pub fn border() -> Rgba {
    rgb(0x363c46)
}
pub fn text() -> Rgba {
    rgb(0xd7dae0)
}
pub fn muted() -> Rgba {
    rgb(0x8b92a0)
}
pub fn selection() -> Rgba {
    rgb(0x2f4a72)
}
pub fn hover() -> Rgba {
    rgb(0x2a2f37)
}
pub fn add_bg() -> Rgba {
    rgb(0x1f3326)
}
pub fn del_bg() -> Rgba {
    rgb(0x3a2226)
}
pub fn add_fg() -> Rgba {
    rgb(0x89d185)
}
pub fn del_fg() -> Rgba {
    rgb(0xf0868d)
}
pub fn hunk_fg() -> Rgba {
    rgb(0x6fb3d2)
}

const LANE_COLORS: [u32; 10] = [
    0x5aa9e6, 0xf2a65a, 0x7bd389, 0xe56b6f, 0xb388eb, 0xf7d354, 0x4ecdc4, 0xff8fab, 0xa3c4f3,
    0xc9e265,
];

/// Color of a lane (by column, so a lane keeps its color while it runs).
pub fn lane(l: u16) -> Hsla {
    rgb(LANE_COLORS[l as usize % LANE_COLORS.len()]).into()
}

pub fn branch_badge() -> Rgba {
    rgb(0x2d4f3c)
}
pub fn remote_badge() -> Rgba {
    rgb(0x3b3f6b)
}
pub fn tag_badge() -> Rgba {
    rgb(0x5c4a24)
}
pub fn head_badge() -> Rgba {
    rgb(0x2f6d4a)
}
