//! Tab icons, drawn from lines and shapes in a `SIZE` square: they take any
//! theme color and stay sharp at every resolution, with no image files.

use std::f32::consts::FRAC_PI_4;

use macroquad::prelude::*;

use super::shape::fill_rounded;
use super::theme;

/// Width and height of every icon, in design pixels.
pub const SIZE: f32 = 16.0;
const LINE: f32 = 1.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Library,
    Mods,
    Saves,
    Updates,
    Network,
    Settings,
}

/// Draw `icon` with its top-left corner at (`x`, `y`).
pub fn draw(icon: Icon, x: f32, y: f32, color: Color) {
    let at = |dx: f32, dy: f32| vec2(x + dx, y + dy);
    match icon {
        Icon::Library => library(at, color),
        Icon::Mods => sliders(at, color),
        Icon::Saves => floppy(at, color),
        Icon::Updates => download(at, color),
        Icon::Network => nodes(at, color),
        Icon::Settings => gear(at(8.0, 8.0), color),
    }
}

/// A small dot on the icon's top-right corner: something waits there.
pub fn dot(x: f32, y: f32, color: Color) {
    draw_circle(x + SIZE - 1.0, y + 1.0, 2.5, color);
}

fn line(a: Vec2, b: Vec2, width: f32, color: Color) {
    draw_line(a.x, a.y, b.x, b.y, width, color);
}

/// The theme's corners, kept small enough for an icon.
fn corner() -> f32 {
    theme::current().radius.min(1.5)
}

/// Four tiles: the games.
fn library(at: impl Fn(f32, f32) -> Vec2, color: Color) {
    for (dx, dy) in [(0.5, 0.5), (9.0, 0.5), (0.5, 9.0), (9.0, 9.0)] {
        let p = at(dx, dy);
        fill_rounded(Rect::new(p.x, p.y, 6.5, 6.5), corner(), color);
    }
}

/// Three sliders: settings a mod turns on or off.
fn sliders(at: impl Fn(f32, f32) -> Vec2, color: Color) {
    for (dy, knob) in [(3.0, 11.0), (8.0, 5.0), (13.0, 9.5)] {
        line(at(1.0, dy), at(15.0, dy), LINE, color);
        let p = at(knob - 2.0, dy - 2.5);
        fill_rounded(Rect::new(p.x, p.y, 4.0, 5.0), corner(), color);
    }
}

/// A floppy disk: its outline, shutter and label.
fn floppy(at: impl Fn(f32, f32) -> Vec2, color: Color) {
    let outline = at(1.0, 1.0);
    draw_rectangle_lines(outline.x, outline.y, 14.0, 14.0, LINE * 2.0, color);
    let shutter = at(4.5, 1.0);
    draw_rectangle(shutter.x, shutter.y, 6.5, 4.5, color);
    let label = at(4.0, 8.5);
    draw_rectangle_lines(label.x, label.y, 8.0, 6.5, LINE * 2.0, color);
}

/// An arrow down into a tray.
fn download(at: impl Fn(f32, f32) -> Vec2, color: Color) {
    line(at(8.0, 1.0), at(8.0, 8.0), 2.0, color);
    draw_triangle(at(3.5, 6.5), at(12.5, 6.5), at(8.0, 11.5), color);
    let tray = [at(1.5, 10.0), at(1.5, 14.5), at(14.5, 14.5), at(14.5, 10.0)];
    for pair in tray.windows(2) {
        line(pair[0], pair[1], LINE, color);
    }
}

/// Three linked machines.
fn nodes(at: impl Fn(f32, f32) -> Vec2, color: Color) {
    let points = [at(8.0, 3.0), at(3.0, 13.0), at(13.0, 13.0)];
    for (a, b) in [(0, 1), (0, 2), (1, 2)] {
        line(points[a], points[b], LINE, color);
    }
    for p in points {
        draw_circle(p.x, p.y, 2.5, color);
    }
}

/// A cog: eight teeth around a ring.
fn gear(center: Vec2, color: Color) {
    for k in 0..8 {
        let angle = k as f32 * FRAC_PI_4;
        let dir = vec2(angle.cos(), angle.sin());
        line(center + dir * 4.0, center + dir * 7.5, 3.0, color);
    }
    draw_circle_lines(center.x, center.y, 4.5, 2.2, color);
}
