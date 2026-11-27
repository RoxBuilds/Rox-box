//! The look of Rox-Box: obsidian ground, slate cards with one-pixel edges,
//! one accent colour with a soft glow where something is alive. Everything
//! is drawn here by code, the emblem and the wordmark too: the launcher
//! ships no image, font or artwork file of any kind.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontFamily, FontId, Pos2, Rect, Response, RichText, Rounding, Sense, Shape, Stroke,
    TextStyle, Ui, Vec2, pos2, vec2,
};

pub const BG: Color32 = Color32::from_rgb(0x0b, 0x0c, 0x0f);
pub const RAIL: Color32 = Color32::from_rgb(0x0f, 0x11, 0x15);
pub const CARD: Color32 = Color32::from_rgb(0x15, 0x18, 0x1d);
pub const CARD_HI: Color32 = Color32::from_rgb(0x1c, 0x20, 0x27);
pub const WELL: Color32 = Color32::from_rgb(0x07, 0x08, 0x0a);
pub const LINE: Color32 = Color32::from_rgb(0x26, 0x2b, 0x33);
pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xee);
pub const DIM: Color32 = Color32::from_rgb(0x8a, 0x91, 0x9c);
pub const FAINT: Color32 = Color32::from_rgb(0x55, 0x5b, 0x66);
pub const ACCENT: Color32 = Color32::from_rgb(0xc8, 0xf0, 0x31);
pub const ACCENT_DEEP: Color32 = Color32::from_rgb(0x7f, 0x9c, 0x14);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0x23, 0x2b, 0x10);
pub const GOOD: Color32 = Color32::from_rgb(0x4a, 0xd6, 0x6d);
pub const WARN: Color32 = Color32::from_rgb(0xff, 0xb0, 0x20);
pub const BAD: Color32 = Color32::from_rgb(0xff, 0x55, 0x45);

/// Corner radius of cards.
pub const RADIUS: f32 = 12.0;

/// `a` towards `b` by `t` (0 to 1).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

fn sans(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Small, sans(12.0)),
        (TextStyle::Body, sans(14.5)),
        (TextStyle::Monospace, mono(12.5)),
        (TextStyle::Button, sans(14.0)),
        (TextStyle::Heading, sans(22.0)),
    ]
    .into();
    style.spacing.item_spacing = vec2(10.0, 8.0);
    style.spacing.button_padding = vec2(14.0, 8.0);
    style.spacing.interact_size.y = 32.0;
    style.spacing.scroll.bar_width = 8.0;
    style.interaction.tooltip_delay = 0.25;

    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD_HI;
    v.extreme_bg_color = WELL;
    v.faint_bg_color = CARD;
    v.window_rounding = Rounding::same(10.0);
    v.menu_rounding = Rounding::same(10.0);
    v.window_stroke = Stroke::new(1.0_f32, LINE);
    v.selection.bg_fill = ACCENT_SOFT;
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    v.hyperlink_color = ACCENT;
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.rounding = Rounding::same(8.0);
        w.bg_stroke = Stroke::new(1.0_f32, LINE);
        w.expansion = 0.0;
        w.weak_bg_fill = CARD_HI;
        w.bg_fill = CARD_HI;
        w.fg_stroke = Stroke::new(1.0_f32, TEXT);
    }
    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.hovered.weak_bg_fill = mix(CARD_HI, DIM, 0.15);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, DIM);
    v.widgets.active.weak_bg_fill = ACCENT_SOFT;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    ctx.set_style(style);
}

/// A slate card.
pub fn card() -> egui::Frame {
    egui::Frame::none()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, LINE))
        .rounding(Rounding::same(RADIUS))
        .inner_margin(egui::Margin::same(20.0))
        .shadow(egui::epaint::Shadow { offset: vec2(0.0, 8.0), blur: 24.0, spread: 0.0, color: Color32::from_black_alpha(90) })
}

/// The hero card: raised, with the accent's glow under it when `alive`.
pub fn hero(alive: bool) -> egui::Frame {
    let glow = if alive { Color32::from_rgba_unmultiplied(0xc8, 0xf0, 0x31, 26) } else { Color32::from_black_alpha(110) };
    egui::Frame::none()
        .fill(CARD_HI)
        .stroke(Stroke::new(1.0_f32, if alive { mix(LINE, ACCENT, 0.35) } else { LINE }))
        .rounding(Rounding::same(16.0))
        .inner_margin(egui::Margin::same(28.0))
        .shadow(egui::epaint::Shadow { offset: vec2(0.0, 10.0), blur: 36.0, spread: 0.0, color: glow })
}

/// A bar or rail: a flat surface with no edge of its own.
pub fn surface(fill: Color32, margin: Vec2) -> egui::Frame {
    egui::Frame::none().fill(fill).inner_margin(egui::Margin::symmetric(margin.x, margin.y))
}

/// A small label in capitals over a group ("ACTIVE TITLE").
pub fn eyebrow(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(sans(11.5)).color(DIM).extra_letter_spacing(1.6));
}

/// A page's heading with its line of explanation.
pub fn page_title(ui: &mut Ui, title: &str, about: &str) {
    ui.label(RichText::new(title).font(sans(28.0)).strong());
    ui.label(RichText::new(about).color(DIM));
    ui.add_space(10.0);
}

fn width_of(ui: &Ui, text: &str, font: &FontId) -> f32 {
    ui.painter().layout_no_wrap(text.to_string(), font.clone(), TEXT).size().x
}

fn hover(ui: &Ui, resp: &Response) -> f32 {
    ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered())
}

/// A status dot.
pub fn dot(ui: &Ui, center: Pos2, color: Color32) {
    ui.painter().circle_filled(center, 7.0, Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40));
    ui.painter().circle_filled(center, 4.0, color);
}

/// A pill that is on or off.
pub fn pill(ui: &mut Ui, label: &str, on: bool) -> Response {
    let font = sans(13.5);
    let size = vec2(width_of(ui, label, &font) + 30.0, 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = hover(ui, &resp);
    let fill = if on { mix(ACCENT_SOFT, ACCENT, 0.06 * t) } else { mix(CARD, CARD_HI, t) };
    let edge = if on { ACCENT } else { mix(LINE, DIM, t) };
    ui.painter().rect(rect, Rounding::same(16.0), fill, Stroke::new(1.0_f32, edge));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, font, if on { ACCENT } else { mix(DIM, TEXT, t) });
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// One choice out of a few, side by side in one outline. Gives the choice clicked.
pub fn segmented<'a>(ui: &mut Ui, choices: &[(&'a str, &str)], current: &str) -> Option<&'a str> {
    let font = sans(13.5);
    let widths: Vec<f32> = choices.iter().map(|c| width_of(ui, c.1, &font) + 28.0).collect();
    let (rect, _) = ui.allocate_exact_size(vec2(widths.iter().sum(), 32.0), Sense::hover());
    ui.painter().rect(rect, Rounding::same(16.0), CARD, Stroke::new(1.0_f32, LINE));
    let mut clicked = None;
    let mut x = rect.left();
    for ((value, label), w) in choices.iter().zip(widths) {
        let part = Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height()));
        x += w;
        let resp = ui.interact(part, ui.id().with(("segment", *value, *label)), Sense::click());
        let on = *value == current;
        let t = hover(ui, &resp);
        if on {
            ui.painter().rect(part.shrink(3.0), Rounding::same(13.0), ACCENT_SOFT, Stroke::new(1.0_f32, ACCENT));
        }
        ui.painter().text(part.center(), Align2::CENTER_CENTER, *label, font.clone(), if on { ACCENT } else { mix(DIM, TEXT, t) });
        if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() && !on {
            clicked = Some(*value);
        }
    }
    clicked
}

/// What the big button is, which decides its colour.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Big {
    Play,
    Stop,
    Off,
}

/// The one big button: PLAY, or STOP while something runs.
pub fn big_button(ui: &mut Ui, label: &str, kind: Big) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(232.0, 60.0), if kind == Big::Off { Sense::hover() } else { Sense::click() });
    let t = hover(ui, &resp);
    let (fill, ink, glow) = match kind {
        Big::Play => (mix(ACCENT, Color32::WHITE, 0.18 * t), BG, Some(ACCENT)),
        Big::Stop => (mix(Color32::from_rgb(0x3a, 0x14, 0x12), BAD, 0.25 * t), BAD, Some(BAD)),
        Big::Off => (CARD, FAINT, None),
    };
    if let Some(g) = glow {
        for (grow, alpha) in [(10.0, 10), (6.0, 20), (3.0, 34)] {
            let a = (f32::from(alpha as u8) * (0.6 + 0.4 * t)) as u8;
            ui.painter().rect_filled(rect.expand(grow), Rounding::same(14.0 + grow), Color32::from_rgba_unmultiplied(g.r(), g.g(), g.b(), a));
        }
    }
    let edge = match kind {
        Big::Stop => BAD,
        Big::Off => LINE,
        Big::Play => fill,
    };
    ui.painter().rect(rect, Rounding::same(14.0), fill, Stroke::new(1.0_f32, edge));
    // A drawn sign before the word: a triangle to play, a square to stop.
    let font = sans(19.0);
    let w = width_of(ui, label, &font);
    let sign = pos2(rect.center().x - w / 2.0 - 16.0, rect.center().y);
    match kind {
        Big::Stop => {
            ui.painter().rect_filled(Rect::from_center_size(sign, vec2(12.0, 12.0)), Rounding::same(2.0), ink);
        }
        _ => {
            let tri = vec![pos2(sign.x - 5.0, sign.y - 7.0), pos2(sign.x + 7.0, sign.y), pos2(sign.x - 5.0, sign.y + 7.0)];
            ui.painter().add(Shape::convex_polygon(tri, ink, Stroke::NONE));
        }
    }
    ui.painter().text(pos2(rect.center().x + 8.0, rect.center().y), Align2::CENTER_CENTER, label, font, ink);
    if kind == Big::Off { resp } else { resp.on_hover_cursor(CursorIcon::PointingHand) }
}

/// An ordinary button; `strong` gives it the accent's outline.
pub fn button(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    ui.add_enabled(enabled, egui::Button::new(text).rounding(Rounding::same(8.0)).min_size(vec2(0.0, 34.0)))
}

/// A button that removes or ends something.
pub fn danger(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).color(BAD)).stroke(Stroke::new(1.0_f32, mix(LINE, BAD, 0.6))).rounding(Rounding::same(8.0)).min_size(vec2(0.0, 34.0)),
    )
}

/// A word that is a button (footers, card corners).
pub fn link(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(RichText::new(text).color(DIM).font(sans(13.0))).frame(false)).on_hover_cursor(CursorIcon::PointingHand)
}

/// A small outlined word: a badge.
pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    let font = sans(11.5);
    let (rect, _) = ui.allocate_exact_size(vec2(width_of(ui, text, &font) + 18.0, 22.0), Sense::hover());
    ui.painter().rect(rect, Rounding::same(11.0), Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 22), Stroke::new(1.0_f32, color));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font, color);
}

/// A fact of the PC as a pill: a dot, what it is, its value. `tip` is said on hover.
pub fn fact(ui: &mut Ui, name: &str, value: &str, color: Color32, tip: &str) -> Response {
    let (small, body) = (sans(11.0), sans(14.0));
    let w = width_of(ui, name, &small).max(width_of(ui, value, &body)) + 52.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w.max(120.0), 54.0), Sense::hover());
    let t = hover(ui, &resp);
    ui.painter().rect(rect, Rounding::same(RADIUS), mix(CARD, CARD_HI, t), Stroke::new(1.0_f32, mix(LINE, color, 0.25 + 0.35 * t)));
    dot(ui, pos2(rect.left() + 20.0, rect.center().y), color);
    ui.painter().text(pos2(rect.left() + 38.0, rect.top() + 17.0), Align2::LEFT_CENTER, name, small, DIM);
    ui.painter().text(pos2(rect.left() + 38.0, rect.top() + 36.0), Align2::LEFT_CENTER, value, body, TEXT);
    if tip.is_empty() { resp } else { resp.on_hover_text(tip) }
}

/// A row of the rail: a drawn sign, a name, the accent's bar when chosen.
pub fn nav(ui: &mut Ui, sign: Sign, label: &str, on: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::click());
    let t = hover(ui, &resp);
    if on || t > 0.0 {
        ui.painter().rect_filled(rect, Rounding::same(10.0), if on { CARD_HI } else { mix(RAIL, CARD, t) });
    }
    if on {
        ui.painter().rect_filled(Rect::from_min_size(pos2(rect.left(), rect.top() + 11.0), vec2(3.0, 22.0)), Rounding::same(2.0), ACCENT);
    }
    let ink = if on { ACCENT } else { mix(DIM, TEXT, t) };
    sign.draw(ui, pos2(rect.left() + 28.0, rect.center().y), ink);
    ui.painter().text(pos2(rect.left() + 52.0, rect.center().y), Align2::LEFT_CENTER, label, sans(14.5), if on { TEXT } else { ink });
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// The little drawn signs of the rail and the window's own buttons.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sign {
    Play,
    Pulse,
    Sliders,
    Minimize,
    Maximize,
    Close,
}

impl Sign {
    fn draw(self, ui: &Ui, c: Pos2, ink: Color32) {
        let p = ui.painter();
        let s = Stroke::new(1.6_f32, ink);
        match self {
            Sign::Play => {
                p.add(Shape::convex_polygon(vec![pos2(c.x - 5.0, c.y - 7.0), pos2(c.x + 7.0, c.y), pos2(c.x - 5.0, c.y + 7.0)], ink, Stroke::NONE));
            }
            Sign::Pulse => {
                let pts = [(-9.0, 0.0), (-4.0, 0.0), (-2.0, -6.0), (2.0, 6.0), (4.0, 0.0), (9.0, 0.0)];
                p.add(Shape::line(pts.iter().map(|(x, y)| pos2(c.x + x, c.y + y)).collect(), s));
            }
            Sign::Sliders => {
                for (y, knob) in [(-5.0, 3.0), (0.0, -4.0), (5.0, 1.0)] {
                    p.line_segment([pos2(c.x - 8.0, c.y + y), pos2(c.x + 8.0, c.y + y)], Stroke::new(1.4_f32, ink));
                    p.circle_filled(pos2(c.x + knob, c.y + y), 2.6, ink);
                }
            }
            Sign::Minimize => {
                p.line_segment([pos2(c.x - 5.0, c.y), pos2(c.x + 5.0, c.y)], Stroke::new(1.2_f32, ink));
            }
            Sign::Maximize => {
                p.rect_stroke(Rect::from_center_size(c, vec2(10.0, 10.0)), Rounding::same(1.5), Stroke::new(1.2_f32, ink));
            }
            Sign::Close => {
                p.line_segment([pos2(c.x - 5.0, c.y - 5.0), pos2(c.x + 5.0, c.y + 5.0)], Stroke::new(1.2_f32, ink));
                p.line_segment([pos2(c.x + 5.0, c.y - 5.0), pos2(c.x - 5.0, c.y + 5.0)], Stroke::new(1.2_f32, ink));
            }
        }
    }
}

/// One of the window's own three buttons (the title bar is ours).
pub fn window_button(ui: &mut Ui, sign: Sign) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(46.0, 34.0), Sense::click());
    let t = hover(ui, &resp);
    if t > 0.0 {
        let fill = if sign == Sign::Close { mix(RAIL, Color32::from_rgb(0xc4, 0x2b, 0x1c), t) } else { mix(RAIL, CARD_HI, t) };
        ui.painter().rect_filled(rect, Rounding::ZERO, fill);
    }
    sign.draw(ui, rect.center(), mix(DIM, TEXT, t));
    resp
}

/// The emblem: a box seen from its corner, its lid in the accent colour.
pub fn emblem(ui: &Ui, center: Pos2, radius: f32) {
    let (c, r) = (center, radius);
    let (dx, dy) = (r * 0.866, r * 0.5);
    let top = pos2(c.x, c.y - r);
    let bottom = pos2(c.x, c.y + r);
    let (ur, lr) = (pos2(c.x + dx, c.y - dy), pos2(c.x + dx, c.y + dy));
    let (ul, ll) = (pos2(c.x - dx, c.y - dy), pos2(c.x - dx, c.y + dy));
    let p = ui.painter();
    p.add(Shape::convex_polygon(vec![top, ur, c, ul], ACCENT, Stroke::NONE));
    p.add(Shape::convex_polygon(vec![ul, c, bottom, ll], ACCENT_DEEP, Stroke::NONE));
    p.add(Shape::convex_polygon(vec![c, ur, lr, bottom], Color32::from_rgb(0x2c, 0x35, 0x12), Stroke::NONE));
    // The seams, in the ground's colour, so the three faces read as one box.
    let seam = Stroke::new((r * 0.09).max(1.0), BG);
    for edge in [[c, ul], [c, ur], [c, bottom]] {
        p.line_segment(edge, seam);
    }
}

/// One letter of the wordmark as strokes in a box one unit high.
fn glyph(c: char) -> (f32, &'static [&'static [(f32, f32)]]) {
    match c {
        'R' => (0.72, &[&[(0.0, 1.0), (0.0, 0.0), (0.56, 0.0), (0.7, 0.14), (0.7, 0.38), (0.56, 0.52), (0.0, 0.52)], &[(0.34, 0.52), (0.72, 1.0)]]),
        'O' => (0.74, &[&[(0.16, 0.0), (0.58, 0.0), (0.74, 0.16), (0.74, 0.84), (0.58, 1.0), (0.16, 1.0), (0.0, 0.84), (0.0, 0.16), (0.16, 0.0), (0.58, 0.0)]]),
        'X' => (0.72, &[&[(0.0, 0.0), (0.72, 1.0)], &[(0.72, 0.0), (0.0, 1.0)]]),
        'B' => (0.72, &[&[(0.0, 0.5), (0.52, 0.5), (0.66, 0.38), (0.66, 0.13), (0.53, 0.0), (0.0, 0.0), (0.0, 1.0), (0.58, 1.0), (0.72, 0.86), (0.72, 0.63), (0.56, 0.5)]]),
        _ => (0.46, &[&[(0.06, 0.52), (0.4, 0.52)]]),
    }
}

/// How wide [`wordmark`] is at `height`.
#[cfg(test)]
pub fn wordmark_width(height: f32) -> f32 {
    "ROX-BOX".chars().map(|c| glyph(c).0 + 0.26).sum::<f32>() * height - 0.26 * height
}

/// The ROX-BOX wordmark, drawn: geometric strokes, its second half in the accent.
pub fn wordmark(ui: &Ui, left_center: Pos2, height: f32) {
    let mut x = left_center.x;
    let top = left_center.y - height / 2.0;
    for (i, c) in "ROX-BOX".chars().enumerate() {
        let (advance, strokes) = glyph(c);
        let ink = if i >= 3 { ACCENT } else { TEXT };
        for line in strokes {
            let points = line.iter().map(|(gx, gy)| pos2(x + gx * height, top + gy * height)).collect();
            ui.painter().add(Shape::line(points, Stroke::new(height * 0.17, ink)));
        }
        x += (advance + 0.26) * height;
    }
}

/// The window's icon: the emblem, drawn into pixels by the same geometry.
pub fn icon() -> egui::IconData {
    const N: i32 = 64;
    let (cx, cy, r) = (32.0f32, 32.0f32, 28.0f32);
    let (dx, dy) = (r * 0.866, r * 0.5);
    // Inside the hexagon, and which of its three faces a point is on.
    let face = |x: f32, y: f32| -> Option<Color32> {
        let (px, py) = (x - cx, y - cy);
        if px.abs() > dx || py.abs() > r - px.abs() * (dy / dx) {
            return None;
        }
        // The lid lies above the two seams that run from the centre to the upper corners.
        Some(if py <= -px.abs() * (dy / dx) {
            ACCENT
        } else if px < 0.0 {
            ACCENT_DEEP
        } else {
            Color32::from_rgb(0x2c, 0x35, 0x12)
        })
    };
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            match face(x as f32 + 0.5, y as f32 + 0.5) {
                Some(c) => rgba.extend_from_slice(&[c.r(), c.g(), c.b(), 255]),
                None => rgba.extend_from_slice(&[0, 0, 0, 0]),
            }
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
}

/// A point of a rectangle's edge the pointer is on, as a resize direction.
pub fn resize_direction(rect: Rect, at: Pos2, grip: f32) -> Option<egui::ResizeDirection> {
    use egui::ResizeDirection as D;
    let (l, r) = (at.x - rect.left() < grip, rect.right() - at.x < grip);
    let (t, b) = (at.y - rect.top() < grip, rect.bottom() - at.y < grip);
    match (l, r, t, b) {
        (true, _, true, _) => Some(D::NorthWest),
        (_, true, true, _) => Some(D::NorthEast),
        (true, _, _, true) => Some(D::SouthWest),
        (_, true, _, true) => Some(D::SouthEast),
        (true, ..) => Some(D::West),
        (_, true, ..) => Some(D::East),
        (_, _, true, _) => Some(D::North),
        (_, _, _, true) => Some(D::South),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_the_emblem() {
        let icon = icon();
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);
        let at = |x: usize, y: usize| &icon.rgba[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
        assert_eq!(at(0, 0)[3], 0, "the corners are clear");
        assert_eq!(at(32, 12), [ACCENT.r(), ACCENT.g(), ACCENT.b(), 255], "the lid is the accent");
        assert_eq!(at(16, 44), [ACCENT_DEEP.r(), ACCENT_DEEP.g(), ACCENT_DEEP.b(), 255], "the left face is its shade");
        assert_ne!(at(48, 44), at(16, 44), "the right face is another");
    }

    #[test]
    fn edges_and_corners_resize() {
        use egui::ResizeDirection as D;
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        assert_eq!(resize_direction(rect, pos2(2.0, 2.0), 6.0), Some(D::NorthWest));
        assert_eq!(resize_direction(rect, pos2(798.0, 598.0), 6.0), Some(D::SouthEast));
        assert_eq!(resize_direction(rect, pos2(400.0, 597.0), 6.0), Some(D::South));
        assert_eq!(resize_direction(rect, pos2(797.0, 300.0), 6.0), Some(D::East));
        assert_eq!(resize_direction(rect, pos2(400.0, 300.0), 6.0), None);
    }

    #[test]
    fn colours_mix_and_the_wordmark_has_a_width() {
        assert_eq!(mix(Color32::BLACK, Color32::WHITE, 0.0), Color32::BLACK);
        assert_eq!(mix(Color32::BLACK, Color32::WHITE, 1.0), Color32::WHITE);
        assert_eq!(mix(Color32::BLACK, Color32::WHITE, 0.5), Color32::from_rgb(128, 128, 128));
        assert!(wordmark_width(20.0) > 100.0 && wordmark_width(20.0) < 160.0, "{}", wordmark_width(20.0));
    }
}
