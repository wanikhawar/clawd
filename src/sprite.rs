//! Clawd's pixel art, animations and effects.
//!
//! The base shape is decoded from Claude Code's terminal logo:
//!
//! ```text
//!  ▐▛███▜▌
//! ▝▜█████▛▘
//!   ▘▘ ▝▝
//! ```
//!
//! Each glyph is a 2×2 grid of quadrant pixels, giving an 18×5 sprite.
//! Terminal cells are twice as tall as they are wide, so pixels are drawn
//! with a 1:2 aspect ratio to keep the original proportions.
//!
//! Shading gives a light 3D touch: light comes from the top left, so exposed
//! top/left edges get a highlight and bottom/right edges a shade, the body has
//! a soft vertical gradient, the eyes have a glint, and Clawd casts a ground
//! shadow that shrinks as it leaves the ground.

use gtk4::cairo;
use std::f64::consts::PI;

pub const COLS: usize = 18;
pub const ROWS: usize = 5;

/// Screen pixels per sprite pixel.
pub const SCALE_X: f64 = 6.0;
pub const SCALE_Y: f64 = 12.0;

/// Room around the sprite for effects (bubbles, sparkles, z's) and the
/// ground shadow.
const PAD_X: f64 = 18.0;
const HEADROOM: f64 = 40.0;
const FOOT: f64 = 8.0;

pub const WIDTH: i32 = (COLS as f64 * SCALE_X + 2.0 * PAD_X) as i32;
pub const HEIGHT: i32 = (HEADROOM + ROWS as f64 * SCALE_Y + FOOT) as i32;

/// Animation frames per second.
pub const FPS: u64 = 12;

type Rgba = (f64, f64, f64, f64);
const BODY: Rgba = (0.851, 0.467, 0.341, 1.0); // #D97757
const LIGHT: Rgba = (1.0, 0.87, 0.78, 0.38);
const SHADE: Rgba = (0.35, 0.12, 0.06, 0.30);
const EYE: Rgba = (0.08, 0.08, 0.08, 1.0);
const GLINT: Rgba = (1.0, 1.0, 1.0, 0.85);
const INK: Rgba = (0.93, 0.91, 0.87, 1.0);
const DIM: Rgba = (0.93, 0.91, 0.87, 0.35);
const GOLD: Rgba = (1.0, 0.82, 0.48, 1.0);
const SKY: Rgba = (0.55, 0.78, 1.0, 1.0);
const BUBBLE: Rgba = (0.141, 0.137, 0.129, 0.96); // the card's background
const ACCENT: Rgba = (0.851, 0.467, 0.341, 1.0);

// B = body, E = eye, l/r = left/right claw, L = leg, . = empty
const BASE: [&str; ROWS] = [
    "...BBBBBBBBBBBB...",
    "...BBEBBBBBBEBB...",
    ".llBBBBBBBBBBBBrr.",
    "...BBBBBBBBBBBB...",
    "....L.L....L.L....",
];

/// Leg columns, left to right.
const LEGS: [usize; 4] = [4, 6, 11, 13];

/// What Claude is using right now, shown in Clawd's work bubble.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Terminal,
    Edit,
    Search,
    Web,
    Other,
}

impl Tool {
    pub fn for_name(name: &str) -> Tool {
        match name {
            "Bash" | "BashOutput" | "KillShell" => Tool::Terminal,
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => Tool::Edit,
            "Read" | "Grep" | "Glob" | "LS" => Tool::Search,
            "WebFetch" | "WebSearch" => Tool::Web,
            _ => Tool::Other,
        }
    }

    fn icon(self) -> &'static [&'static str] {
        match self {
            Tool::Terminal => &TERMINAL,
            Tool::Edit => &PENCIL,
            Tool::Search => &MAGNIFIER,
            Tool::Web => &GLOBE,
            Tool::Other => &GEAR,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Anim {
    Idle,
    /// Idle for a long while.
    Sleep,
    /// Being dragged.
    Carried,
    /// Wandering about on its own.
    Walk,
    /// Recording your voice.
    Listen,
    /// Claude is thinking.
    Think,
    /// Claude's reply is streaming in.
    Write,
    /// A tool is running.
    Work(Tool),
    /// Waiting on a permission answer.
    Ask,
    /// A turn just finished.
    Happy,
    /// Something went wrong.
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Eyes {
    Open,
    Closed,
    Left,
    Right,
    Up,
    /// Squinting with concentration.
    Focused,
    /// ^ ^
    Happy,
    /// x x
    Cross,
}

/// An effect drawn around Clawd.
#[derive(Clone, Debug)]
enum Fx {
    /// A pixel glyph; `px` is the size of one glyph pixel.
    Glyph { rows: &'static [&'static str], x: f64, y: f64, px: f64, color: Rgba },
    /// A speech/thought bubble.
    Bubble { x: f64, y: f64, w: f64, h: f64, fill: Rgba },
    Rect { x: f64, y: f64, w: f64, h: f64, color: Rgba },
}

fn dot(x: f64, y: f64, size: f64, color: Rgba) -> Fx {
    Fx::Rect { x, y, w: size, h: size, color }
}

/// Everything needed to draw one frame. Offsets are in screen pixels.
#[derive(Clone, Debug)]
pub struct Frame {
    dx: f64,
    dy: f64,
    /// > 0 squashes the body down, < 0 stretches it up.
    squash: f64,
    eyes: Eyes,
    /// (dx, dy) of the left and right claws, on top of the body's offset.
    claws: [(f64, f64); 2],
    /// Length of each leg as a fraction of a sprite pixel.
    legs: [f64; 4],
    fx: Vec<Fx>,
}

impl Frame {
    fn rest() -> Frame {
        Frame { dx: 0.0, dy: 0.0, squash: 0.0, eyes: Eyes::Open, claws: [(0.0, 0.0); 2], legs: [1.0; 4], fx: Vec::new() }
    }
}

// Anchors for effects.
const HEAD_TOP: f64 = HEADROOM;
const CENTER_X: f64 = PAD_X + COLS as f64 * SCALE_X / 2.0;
/// Top-left of the bubble above Clawd's head (upper right).
const BUBBLE_X: f64 = PAD_X + 12.0 * SCALE_X + 2.0;
const BUBBLE_Y: f64 = 2.0;

/// Deterministic per-cycle randomness, so idle behaviour varies without state.
fn rnd(n: u64) -> u64 {
    let mut x = n.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Returns the frame for `anim`, `t` ticks after it started.
pub fn frame(anim: Anim, t: u64) -> Frame {
    let mut f = Frame::rest();
    match anim {
        Anim::Idle => {
            // Breathing.
            f.dy = if (t / 8) % 2 == 1 { -1.0 } else { 0.0 };
            // Every 5 seconds, maybe do something small.
            let (cycle, phase) = (t / 60, t % 60);
            match rnd(cycle) % 6 {
                0 if (8..20).contains(&phase) => f.eyes = Eyes::Left,
                0 if (22..34).contains(&phase) => f.eyes = Eyes::Right,
                1 if (10..22).contains(&phase) => {
                    f.claws[1] = if phase % 4 < 2 { (1.0, -9.0) } else { (1.0, -5.0) };
                }
                2 if (20..26).contains(&phase) => {
                    f.dy = [-3.0, -6.0, -7.0, -6.0, -3.0, 0.0][(phase - 20) as usize];
                    f.legs = [0.8; 4];
                }
                3 if (12..30).contains(&phase) => f.eyes = Eyes::Up,
                _ => {}
            }
            if phase == 44 || phase == 45 {
                f.eyes = Eyes::Closed;
            }
        }
        Anim::Sleep => {
            f.eyes = Eyes::Closed;
            let exhale = (t / 12) % 2 == 1;
            f.dy = if exhale { 1.0 } else { 0.0 };
            f.squash = if exhale { 0.6 } else { 0.0 };
            f.claws = [(0.0, 2.0); 2];
            for i in 0..3u64 {
                let p = ((t + i * 14) % 42) as f64 / 42.0;
                let alpha = (p / 0.15).min(1.0) * (1.0 - p);
                f.fx.push(Fx::Glyph {
                    rows: &Z,
                    x: PAD_X + 14.0 * SCALE_X + p * 14.0,
                    y: HEAD_TOP - 6.0 - p * 30.0,
                    px: if i == 1 { 3.0 } else { 2.0 },
                    color: (INK.0, INK.1, INK.2, alpha),
                });
            }
        }
        Anim::Carried => {
            f.dy = -2.0;
            f.squash = -0.6;
            let swing = t % 4 < 2;
            f.legs = if swing { [1.5, 1.15, 1.5, 1.15] } else { [1.15, 1.5, 1.15, 1.5] };
            f.claws = if t.is_multiple_of(2) { [(-2.0, -9.0), (2.0, -3.0)] } else { [(-2.0, -3.0), (2.0, -9.0)] };
        }
        Anim::Walk => {
            // A stepping gait: opposite leg pairs lift in turn, a small bob,
            // claws swinging against the stride.
            let step = (t / 2).is_multiple_of(2);
            f.legs = if step { [1.0, 0.55, 1.0, 0.55] } else { [0.55, 1.0, 0.55, 1.0] };
            f.dy = if (t / 2).is_multiple_of(2) { -1.0 } else { 0.0 };
            f.claws = if step { [(0.0, -2.0), (0.0, 1.0)] } else { [(0.0, 1.0), (0.0, -2.0)] };
            if t % 50 == 30 {
                f.eyes = Eyes::Closed;
            }
        }
        Anim::Listen => {
            // Leaning in, claws cupped, a mic bubble with pulsing waves.
            f.dy = if (t / 4).is_multiple_of(2) { -1.0 } else { -2.0 };
            f.claws = [(1.0, -6.0), (-1.0, -6.0)];
            f.fx.push(Fx::Bubble { x: BUBBLE_X - 1.0, y: BUBBLE_Y, w: 29.0, h: 29.0, fill: BUBBLE });
            f.fx.push(Fx::Glyph { rows: &MIC, x: BUBBLE_X + 3.0, y: BUBBLE_Y + 4.0, px: 3.0, color: ACCENT });
            for i in 0..3u64 {
                let lit = (t / 3) % 3 == i;
                let color = if lit { INK } else { DIM };
                let x = BUBBLE_X - 6.0 - i as f64 * 5.0;
                f.fx.push(Fx::Rect { x, y: BUBBLE_Y + 11.0 - i as f64 * 3.0, w: 2.0, h: 6.0 + i as f64 * 6.0, color });
            }
        }
        Anim::Think => {
            f.eyes = Eyes::Up;
            f.dx = [0.0, 1.0, 2.0, 3.0, 3.0, 2.0, 1.0, 0.0, -1.0, -2.0, -3.0, -3.0, -2.0, -1.0][(t % 14) as usize];
            f.dy = if (t / 7).is_multiple_of(2) { 0.0 } else { -1.0 };
            // Right claw on the chin, tapping.
            f.claws[1] = (-5.0, if (t / 5).is_multiple_of(2) { -9.0 } else { -7.0 });
            // Thought trail rising to a bubble with a loading wave.
            f.fx.push(dot(PAD_X + 14.0 * SCALE_X, HEAD_TOP - 7.0, 3.0, INK));
            f.fx.push(dot(PAD_X + 15.0 * SCALE_X - 1.0, HEAD_TOP - 13.0, 4.0, INK));
            f.fx.push(Fx::Bubble { x: BUBBLE_X, y: BUBBLE_Y, w: 32.0, h: 18.0, fill: BUBBLE });
            for i in 0..3u64 {
                let lit = (t / 3) % 4 == i;
                let y = BUBBLE_Y + 8.0 - if lit { 2.0 } else { 0.0 };
                f.fx.push(dot(BUBBLE_X + 7.0 + i as f64 * 7.0, y, 3.0, if lit { INK } else { DIM }));
            }
        }
        Anim::Write => {
            f.dy = [0.0, -2.0, -3.0, -2.0][(t % 4) as usize];
            f.claws = if (t / 3).is_multiple_of(2) { [(0.0, -7.0), (0.0, 0.0)] } else { [(0.0, 0.0), (0.0, -7.0)] };
            if t % 40 == 20 {
                f.eyes = Eyes::Closed;
            }
            // Speech bubble with lines being typed.
            f.fx.push(Fx::Bubble { x: BUBBLE_X, y: BUBBLE_Y, w: 32.0, h: 20.0, fill: BUBBLE });
            let progress = (t % 30) as f64;
            for (i, full) in [20.0, 16.0, 11.0].into_iter().enumerate() {
                let w = ((progress - i as f64 * 8.0) / 8.0).clamp(0.0, 1.0) * full;
                if w > 0.0 {
                    f.fx.push(Fx::Rect { x: BUBBLE_X + 6.0, y: BUBBLE_Y + 5.0 + i as f64 * 4.0, w, h: 2.0, color: INK });
                }
            }
        }
        Anim::Work(tool) => {
            let beat = t.is_multiple_of(2);
            f.dx = if beat { 1.0 } else { -1.0 };
            f.dy = if beat { 0.0 } else { -1.0 };
            f.eyes = Eyes::Focused;
            f.claws = if beat { [(0.0, -8.0), (0.0, 1.0)] } else { [(0.0, 1.0), (0.0, -8.0)] };
            // Tool bubble, bobbing gently.
            let bob = if (t / 4).is_multiple_of(2) { 0.0 } else { -1.0 };
            f.fx.push(Fx::Bubble { x: BUBBLE_X - 1.0, y: BUBBLE_Y + bob, w: 29.0, h: 29.0, fill: BUBBLE });
            f.fx.push(Fx::Glyph { rows: tool.icon(), x: BUBBLE_X + 3.0, y: BUBBLE_Y + 4.0 + bob, px: 3.0, color: INK });
            // Sparks off whichever claw just came down.
            let claw_x = if beat { PAD_X + 16.0 * SCALE_X } else { PAD_X + 1.5 * SCALE_X };
            let claw_y = HEAD_TOP + 2.0 * SCALE_Y;
            for (k, (vx, vy)) in [(-1.0, -1.0), (1.2, -1.3), (0.2, -1.8), (1.6, -0.4)].into_iter().enumerate() {
                let age = ((t + k as u64) % 4) as f64;
                let color = (GOLD.0, GOLD.1, GOLD.2, 1.0 - age / 4.5);
                f.fx.push(dot(claw_x + vx * age * 4.5, claw_y + vy * age * 4.5, 4.0 - age * 0.5, color));
            }
        }
        Anim::Ask => {
            f.dy = [0.0, -2.0, -4.0, -2.0][(t % 4) as usize];
            f.claws = if t % 4 < 2 { [(-2.0, -11.0), (2.0, -4.0)] } else { [(-2.0, -4.0), (2.0, -11.0)] };
            let pulse = (t / 3).is_multiple_of(2);
            f.fx.push(Fx::Bubble { x: BUBBLE_X + 4.0, y: BUBBLE_Y, w: 20.0, h: 26.0, fill: if pulse { ACCENT } else { BUBBLE } });
            f.fx.push(Fx::Glyph {
                rows: &EXCLAIM,
                x: BUBBLE_X + 12.5,
                y: BUBBLE_Y + 5.0,
                px: 3.0,
                color: if pulse { BUBBLE } else { INK },
            });
        }
        Anim::Happy => {
            // A bouncy hop with anticipation, squash and stretch.
            const HOP: [(f64, f64, f64); 10] = [
                (2.0, 1.0, 0.7),
                (-6.0, -0.6, 1.2),
                (-12.0, -0.3, 0.6),
                (-15.0, 0.0, 0.5),
                (-15.0, 0.0, 0.5),
                (-12.0, 0.0, 0.6),
                (-6.0, -0.3, 1.0),
                (2.0, 1.0, 0.7),
                (0.0, 0.3, 1.0),
                (0.0, 0.0, 1.0),
            ];
            let (dy, squash, legs) = HOP[(t % 10) as usize];
            f.dy = dy;
            f.squash = squash;
            f.legs = [legs; 4];
            f.eyes = Eyes::Happy;
            let wiggle = if t.is_multiple_of(2) { -1.0 } else { 1.0 };
            f.claws = [(-1.0 + wiggle, -9.0), (1.0 - wiggle, -9.0)];
            let w = WIDTH as f64;
            let spots = [(4.0, HEAD_TOP + 6.0, GOLD), (w - 12.0, HEAD_TOP + 12.0, ACCENT), (14.0, HEAD_TOP - 16.0, INK), (w - 26.0, HEAD_TOP - 20.0, GOLD)];
            for (i, (x, y, color)) in spots.into_iter().enumerate() {
                match (t + i as u64 * 2) % 6 {
                    0 | 1 => f.fx.push(Fx::Glyph { rows: &SPARK, x, y, px: 2.0, color }),
                    2 => f.fx.push(Fx::Glyph { rows: &TWINKLE, x: x + 2.0, y: y + 2.0, px: 2.0, color }),
                    _ => {}
                }
            }
        }
        Anim::Error => {
            f.dx = if t < 8 { if t.is_multiple_of(2) { 2.0 } else { -2.0 } } else { 0.0 };
            f.dy = 3.0;
            f.squash = 0.6;
            f.eyes = Eyes::Cross;
            f.claws = [(0.0, 4.0); 2];
            f.legs = [0.8; 4];
            let p = (t % 16) as f64 / 16.0;
            f.fx.push(Fx::Glyph {
                rows: &DROP,
                x: PAD_X + 15.0 * SCALE_X + 3.0,
                y: HEAD_TOP + 2.0 + p * 18.0,
                px: 2.0,
                color: (SKY.0, SKY.1, SKY.2, 1.0 - p * p),
            });
        }
    }
    f
}

pub fn draw(cr: &cairo::Context, f: &Frame) {
    cr.set_antialias(cairo::Antialias::None);

    ground_shadow(cr, f);

    let ground = HEAD_TOP + 4.0 * SCALE_Y; // top of the leg row
    // Squash/stretch moves only the top of the body; the rows share the
    // height evenly, so they never open gaps.
    let body_rows = (ROWS - 1) as f64;
    let (top, bottom) = (HEAD_TOP + f.dy + f.squash * 5.0, ground + f.dy);
    let row_h = (bottom - top) / body_rows;
    let row_y = |row: usize| top + row as f64 * row_h;

    let gradient = cairo::LinearGradient::new(0.0, HEAD_TOP + f.dy, 0.0, ground + f.dy);
    gradient.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.10);
    gradient.add_color_stop_rgba(1.0, 0.0, 0.0, 0.0, 0.14);

    for (row, line) in BASE.iter().enumerate().take(ROWS - 1) {
        for (col, ch) in line.chars().enumerate() {
            let (claw_dx, claw_dy) = match ch {
                'l' => f.claws[0],
                'r' => f.claws[1],
                _ => (0.0, 0.0),
            };
            if ch == '.' {
                continue;
            }
            let x = PAD_X + col as f64 * SCALE_X + f.dx + claw_dx;
            let y = (row_y(row) + claw_dy).round();
            // Round both edges so neighbouring rows meet exactly.
            let h = (row_y(row + 1) + claw_dy).round() - y;
            fill(cr, BODY, x, y, SCALE_X, h);
            let _ = cr.set_source(&gradient);
            cr.rectangle(x, y, SCALE_X, h);
            let _ = cr.fill();
            edges(cr, row, col, x, y, h);
            if ch == 'E' {
                eye(cr, f.eyes, x, y, h);
            }
        }
    }

    // Legs hang from the body; a lifted or tucked leg is shorter.
    for (i, &col) in LEGS.iter().enumerate() {
        let x = PAD_X + col as f64 * SCALE_X + f.dx;
        let y = ground + f.dy;
        let h = SCALE_Y * f.legs[i];
        fill(cr, BODY, x, y, SCALE_X, h);
        fill(cr, SHADE, x + SCALE_X - 2.0, y, 2.0, h);
    }

    for fx in &f.fx {
        effect(cr, fx);
    }
}

/// A soft shadow under Clawd that shrinks and fades as it leaves the ground.
fn ground_shadow(cr: &cairo::Context, f: &Frame) {
    let lift = (-f.dy).max(0.0);
    // Dangling legs mean Clawd is being carried, well off the ground.
    let carried = f.legs.iter().any(|&l| l > 1.2);
    let k = 1.0 - (lift / 30.0).min(0.5) - if carried { 0.3 } else { 0.0 };
    let y = HEAD_TOP + ROWS as f64 * SCALE_Y + 2.0;
    cr.save().ok();
    cr.set_antialias(cairo::Antialias::Default);
    cr.translate(CENTER_X + f.dx, y);
    cr.scale(42.0 * k, 4.0 * k);
    let shadow = cairo::RadialGradient::new(0.0, 0.0, 0.0, 0.0, 0.0, 1.0);
    shadow.add_color_stop_rgba(0.0, 0.0, 0.0, 0.0, 0.45 * k);
    shadow.add_color_stop_rgba(1.0, 0.0, 0.0, 0.0, 0.0);
    let _ = cr.set_source(&shadow);
    cr.arc(0.0, 0.0, 1.0, 0.0, 2.0 * PI);
    let _ = cr.fill();
    cr.restore().ok();
}

/// Highlights on edges facing the light (top, left), shade on the others.
fn edges(cr: &cairo::Context, row: usize, col: usize, x: f64, y: f64, h: f64) {
    let solid = |r: isize, c: isize| -> bool {
        r >= 0
            && c >= 0
            && (r as usize) < ROWS - 1
            && BASE[r as usize].as_bytes().get(c as usize).is_some_and(|&b| b != b'.' && b != b'L')
    };
    let (r, c) = (row as isize, col as isize);
    if !solid(r - 1, c) {
        fill(cr, LIGHT, x, y, SCALE_X, 3.0);
    }
    if !solid(r, c - 1) {
        fill(cr, LIGHT, x, y, 2.0, h);
    }
    if !solid(r + 1, c) {
        fill(cr, SHADE, x, y + h - 3.0, SCALE_X, 3.0);
    }
    if !solid(r, c + 1) {
        fill(cr, SHADE, x + SCALE_X - 2.0, y, 2.0, h);
    }
}

fn eye(cr: &cairo::Context, eyes: Eyes, x: f64, y: f64, h: f64) {
    let w = SCALE_X;
    match eyes {
        Eyes::Open => {
            fill(cr, EYE, x, y, w, h);
            fill(cr, GLINT, x + 1.0, y + 2.0, 2.0, 2.0);
        }
        Eyes::Left => {
            fill(cr, EYE, x - 2.0, y, w, h);
            fill(cr, GLINT, x - 1.0, y + 2.0, 2.0, 2.0);
        }
        Eyes::Right => {
            fill(cr, EYE, x + 2.0, y, w, h);
            fill(cr, GLINT, x + 3.0, y + 2.0, 2.0, 2.0);
        }
        Eyes::Up => {
            fill(cr, EYE, x, y - 3.0, w, h - 2.0);
            fill(cr, GLINT, x + 1.0, y - 2.0, 2.0, 2.0);
        }
        Eyes::Focused => fill(cr, EYE, x, y + 4.0, w, h - 6.0),
        Eyes::Closed => fill(cr, EYE, x, y + 7.0, w, 2.0),
        Eyes::Happy => {
            fill(cr, EYE, x, y + 6.0, 2.0, 4.0);
            fill(cr, EYE, x + 2.0, y + 3.0, 2.0, 4.0);
            fill(cr, EYE, x + 4.0, y + 6.0, 2.0, 4.0);
        }
        Eyes::Cross => {
            for (dx, dy) in [(0.0, 2.0), (4.0, 2.0), (2.0, 5.0), (0.0, 8.0), (4.0, 8.0)] {
                fill(cr, EYE, x + dx, y + dy, 2.0, 2.0);
            }
        }
    }
}

fn effect(cr: &cairo::Context, fx: &Fx) {
    match *fx {
        Fx::Glyph { rows, x, y, px, color } => {
            for (r, line) in rows.iter().enumerate() {
                for (c, ch) in line.chars().enumerate() {
                    if ch == '#' {
                        fill(cr, color, x + c as f64 * px, y + r as f64 * px, px, px);
                    }
                }
            }
        }
        Fx::Bubble { x, y, w, h, fill: color } => {
            cr.save().ok();
            cr.set_antialias(cairo::Antialias::Default);
            rounded(cr, x + 1.0, y + 2.0, w, h, 6.0);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.25);
            let _ = cr.fill();
            rounded(cr, x, y, w, h, 6.0);
            cr.set_source_rgba(color.0, color.1, color.2, color.3);
            let _ = cr.fill_preserve();
            let sheen = cairo::LinearGradient::new(0.0, y, 0.0, y + h);
            sheen.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.10);
            sheen.add_color_stop_rgba(0.5, 1.0, 1.0, 1.0, 0.0);
            let _ = cr.set_source(&sheen);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.14);
            cr.set_line_width(1.0);
            let _ = cr.stroke();
            cr.restore().ok();
        }
        Fx::Rect { x, y, w, h, color } => fill(cr, color, x, y, w, h),
    }
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

fn fill(cr: &cairo::Context, (r, g, b, a): Rgba, x: f64, y: f64, w: f64, h: f64) {
    cr.set_source_rgba(r, g, b, a);
    cr.rectangle(x, y, w, h);
    let _ = cr.fill();
}

// ---- Glyphs ('#' = one glyph pixel) ----

const Z: [&str; 4] = ["####", "..#.", ".#..", "####"];
const EXCLAIM: [&str; 5] = ["#", "#", "#", ".", "#"];
const SPARK: [&str; 5] = ["..#..", "..#..", "##.##", "..#..", "..#.."];
const TWINKLE: [&str; 3] = [".#.", "###", ".#."];
const DROP: [&str; 4] = [".#.", "###", "###", ".#."];

const TERMINAL: [&str; 7] = ["#######", "#.....#", "#.#...#", "#..#..#", "#.#.###", "#.....#", "#######"];
const PENCIL: [&str; 7] = [".....##", "....###", "...###.", "..###..", ".###...", "##.....", "#......"];
const MAGNIFIER: [&str; 7] = [".###...", "#...#..", "#...#..", "#...#..", ".####..", "....##.", ".....##"];
const GLOBE: [&str; 7] = ["..###..", ".#.#.#.", "#..#..#", "#######", "#..#..#", ".#.#.#.", "..###.."];
const MIC: [&str; 7] = ["..###..", "..###..", "..###..", "#.###.#", "#..#..#", ".#####.", "...#..."];
const GEAR: [&str; 7] = ["..#.#..", ".#####.", "##...##", ".#...#.", "##...##", ".#####.", "..#.#.."];

#[cfg(test)]
mod tests {
    use super::*;
    use gtk4::gdk_pixbuf::{Colorspace, Pixbuf};

    const ALL: [Anim; 15] = [
        Anim::Idle,
        Anim::Sleep,
        Anim::Carried,
        Anim::Walk,
        Anim::Listen,
        Anim::Think,
        Anim::Write,
        Anim::Work(Tool::Terminal),
        Anim::Work(Tool::Edit),
        Anim::Work(Tool::Search),
        Anim::Work(Tool::Web),
        Anim::Work(Tool::Other),
        Anim::Ask,
        Anim::Happy,
        Anim::Error,
    ];

    #[test]
    fn every_frame_draws() {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, WIDTH, HEIGHT).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        for anim in ALL {
            for t in 0..120 {
                draw(&cr, &frame(anim, t));
            }
        }
    }

    #[test]
    fn effects_stay_inside_the_canvas() {
        for anim in ALL {
            for t in 0..120 {
                for fx in frame(anim, t).fx {
                    let (x, y) = match fx {
                        Fx::Glyph { x, y, .. } | Fx::Bubble { x, y, .. } | Fx::Rect { x, y, .. } => (x, y),
                    };
                    assert!(x >= 0.0 && x < WIDTH as f64 && y >= 0.0 && y < HEIGHT as f64, "{anim:?} t={t} at ({x}, {y})");
                }
            }
        }
    }

    /// Renders every animation into a contact sheet for eyeballing:
    /// `CLAWD_SHEET=/tmp/sheet.png cargo test -- --ignored sheet`
    #[test]
    #[ignore]
    fn sheet() {
        let path = std::env::var("CLAWD_SHEET").expect("set CLAWD_SHEET to an output .png path");
        let frames: Vec<u64> = (0..10).map(|i| i * 2).collect();
        let (cols, rows) = (frames.len() as i32, ALL.len() as i32);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, cols * WIDTH, rows * HEIGHT).unwrap();
        {
            let cr = cairo::Context::new(&surface).unwrap();
            cr.set_source_rgb(0.11, 0.12, 0.14);
            let _ = cr.paint();
            for (r, anim) in ALL.iter().enumerate() {
                for (c, &t) in frames.iter().enumerate() {
                    cr.save().unwrap();
                    cr.translate((c as i32 * WIDTH) as f64, (r as i32 * HEIGHT) as f64);
                    draw(&cr, &frame(*anim, t));
                    cr.restore().unwrap();
                }
            }
        }
        surface.flush();
        let (w, h, stride) = (surface.width(), surface.height(), surface.stride() as usize);
        let data = surface.take_data().unwrap();
        // Cairo's ARGB32 is premultiplied BGRA in memory on little-endian; the
        // sheet is opaque, so swizzling is enough.
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let p = &data[y * stride + x * 4..];
                rgb.extend_from_slice(&[p[2], p[1], p[0]]);
            }
        }
        let pixbuf = Pixbuf::from_mut_slice(rgb, Colorspace::Rgb, false, 8, w, h, w * 3);
        pixbuf.savev(&path, "png", &[]).unwrap();
    }
}
