//! Palette and glyphs.
//!
//! The bar ramp is the load-bearing idea: colour is interpolated **per
//! character along the track**, so a bar encodes its magnitude twice — once by
//! length and once by hue. A 60% bar is visibly warmer at the tip than a 40%
//! one even before the number is read.
//!
//! The ramp is pastel: high lightness, restrained saturation. Saturated
//! terminal colours vibrate against a dark ground and tire the eye on a panel
//! that is meant to be glanced at all day.

use ratatui::style::Color;

/// Anchors for the gradient, as (position 0..1, RGB). Interpolated between.
const RAMP: [(f64, (u8, u8, u8)); 5] = [
    (0.00, (167, 230, 170)), // mint
    (0.35, (214, 230, 158)), // chartreuse
    (0.60, (242, 216, 158)), // amber
    (0.80, (243, 186, 152)), // peach
    (1.00, (238, 155, 155)), // soft red
];

fn lerp(a: u8, b: u8, k: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * k).round().clamp(0.0, 255.0) as u8
}

/// Colour at fractional position `t` along the track.
pub fn ramp(t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    for w in RAMP.windows(2) {
        let (t0, c0) = w[0];
        let (t1, c1) = w[1];
        if t <= t1 {
            let k = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
            return Color::Rgb(lerp(c0.0, c1.0, k), lerp(c0.1, c1.1, k), lerp(c0.2, c1.2, k));
        }
    }
    let l = RAMP[RAMP.len() - 1].1;
    Color::Rgb(l.0, l.1, l.2)
}

/// Colour for a bare number, matched to where that value sits on the ramp so
/// figures and bars agree.
pub fn value_colour(pct: f64) -> Color {
    if pct <= 0.5 {
        DIM
    } else {
        ramp(pct / 100.0)
    }
}

pub const TRACK: Color = Color::Rgb(58, 66, 80);
pub const CHROME: Color = Color::Rgb(68, 78, 92);
pub const DIM: Color = Color::Rgb(108, 120, 136);
pub const LABEL: Color = Color::Rgb(150, 163, 180);
pub const TEXT: Color = Color::Rgb(201, 211, 223);
pub const TITLE: Color = Color::Rgb(158, 216, 224); // pastel cyan
pub const BLUE: Color = Color::Rgb(168, 197, 235); // pastel blue
pub const MINT: Color = Color::Rgb(167, 230, 170);
pub const WARN: Color = Color::Rgb(238, 155, 155);
pub const AMBER: Color = Color::Rgb(242, 216, 158);
pub const VIOLET: Color = Color::Rgb(198, 180, 236);

pub const EIGHTHS: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
pub const TRACK_CH: &str = "░";

/// Nerd Font glyphs, all from the Font Awesome range that Nerd Fonts has
/// carried since v1, so they survive most patched fonts.
///
/// Every bar row carries exactly one glyph. That matters: if a terminal renders
/// these as double-width, all rows shift by the same amount and the columns
/// still line up. A row without a glyph would break that.
#[derive(Clone, Copy)]
pub struct Icons {
    pub gpu: &'static str,
    pub media: &'static str,
    pub video: &'static str,
    pub enhance: &'static str,
    pub render: &'static str,
    pub copy: &'static str,
    pub compute: &'static str,
    pub power: &'static str,
    pub clock: &'static str,
    pub memory: &'static str,
    pub temp: &'static str,
    pub fan: &'static str,
    pub link: &'static str,
    pub warn: &'static str,
    pub container: &'static str,
    pub history: &'static str,
}

pub const NERD: Icons = Icons {
    gpu: "\u{f26c}",
    media: "\u{f0e4}",
    video: "\u{f008}",
    enhance: "\u{f0d0}",
    render: "\u{f1b2}",
    copy: "\u{f0c5}",
    compute: "\u{f2db}",
    power: "\u{f0e7}",
    clock: "\u{f017}",
    memory: "\u{f0a0}",
    temp: "\u{f2c9}",
    fan: "\u{f863}",
    link: "\u{f0c1}",
    warn: "\u{f071}",
    container: "\u{f0e8}",
    history: "\u{f1da}",
};

/// Fallback for a terminal without a patched font. Same column width, so the
/// layout does not move when icons are switched off.
pub const PLAIN: Icons = Icons {
    gpu: "*",
    media: "@",
    video: "V",
    enhance: "E",
    render: "R",
    copy: "C",
    compute: "K",
    power: "W",
    clock: "F",
    memory: "M",
    temp: "T",
    fan: "~",
    link: "=",
    warn: "!",
    container: "#",
    history: "-",
};

pub fn icons(enabled: bool) -> Icons {
    if enabled {
        NERD
    } else {
        PLAIN
    }
}
