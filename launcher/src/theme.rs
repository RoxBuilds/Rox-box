//! The look: a dark matte instrument panel. Square corners, hard one-pixel
//! lines, one typeface (monospace), one accent colour. No images: the icon
//! is drawn here, so the launcher ships no artwork of any kind.

use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Rounding, Stroke, TextStyle, Vec2};

pub const BG: Color32 = Color32::from_rgb(0x0d, 0x0e, 0x10);
pub const PANEL: Color32 = Color32::from_rgb(0x14, 0x16, 0x19);
pub const WELL: Color32 = Color32::from_rgb(0x08, 0x09, 0x0a);
pub const LINE: Color32 = Color32::from_rgb(0x2a, 0x2e, 0x33);
pub const TEXT: Color32 = Color32::from_rgb(0xd7, 0xda, 0xd3);
pub const DIM: Color32 = Color32::from_rgb(0x7c, 0x81, 0x87);
pub const ACCENT: Color32 = Color32::from_rgb(0xc8, 0xf0, 0x31);
pub const WARN: Color32 = Color32::from_rgb(0xff, 0xb0, 0x00);
pub const BAD: Color32 = Color32::from_rgb(0xff, 0x4d, 0x3d);

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mono = |size: f32| FontId::new(size, FontFamily::Monospace);
    style.text_styles = [
        (TextStyle::Small, mono(11.0)),
        (TextStyle::Body, mono(13.0)),
        (TextStyle::Monospace, mono(12.5)),
        (TextStyle::Button, mono(13.0)),
        (TextStyle::Heading, mono(15.0)),
    ]
    .into();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 6.0);
    style.spacing.interact_size.y = 28.0;

    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = WELL;
    v.faint_bg_color = PANEL;
    v.window_rounding = Rounding::ZERO;
    v.menu_rounding = Rounding::ZERO;
    v.window_stroke = Stroke::new(1.0_f32, LINE);
    v.selection.bg_fill = Color32::from_rgb(0x2b, 0x33, 0x12);
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    v.hyperlink_color = ACCENT;
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.rounding = Rounding::ZERO;
        w.bg_stroke = Stroke::new(1.0_f32, LINE);
        w.expansion = 0.0;
    }
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.inactive.weak_bg_fill = PANEL;
    v.widgets.inactive.bg_fill = PANEL;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x1d, 0x20, 0x24);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x1d, 0x20, 0x24);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x2b, 0x33, 0x12);
    v.widgets.active.bg_fill = Color32::from_rgb(0x2b, 0x33, 0x12);
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.open.weak_bg_fill = PANEL;
    ctx.set_style(style);
}

fn framed(fill: Color32, margin: f32) -> egui::Frame {
    egui::Frame::none().fill(fill).stroke(Stroke::new(1.0_f32, LINE)).inner_margin(margin)
}

/// The top and bottom bars.
pub fn bar() -> egui::Frame {
    framed(PANEL, 8.0)
}

/// The side panel.
pub fn panel() -> egui::Frame {
    framed(PANEL, 12.0)
}

/// The console's well.
pub fn well() -> egui::Frame {
    framed(WELL, 8.0)
}

/// A section's heading: its name over a full-width line.
pub fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text).color(DIM).text_style(TextStyle::Heading));
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, Rounding::ZERO, LINE);
    ui.add_space(4.0);
}

/// A small outlined word: a status, a badge.
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::none().stroke(Stroke::new(1.0_f32, color)).inner_margin(Vec2::new(6.0, 2.0)).show(ui, |ui| {
        ui.label(RichText::new(text).color(color).small());
    });
}

/// An ordinary button.
pub fn button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(enabled, egui::Button::new(text))
}

/// A button without a box (the bars' controls).
pub fn flat(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(DIM)).frame(false))
}

/// The one button that starts a title.
pub fn primary(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let label = RichText::new(text).color(BG).strong().size(16.0);
    ui.add_enabled(enabled, egui::Button::new(label).fill(ACCENT).min_size(Vec2::new(180.0, 44.0)))
}

/// A button that ends or removes something.
pub fn danger(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let label = RichText::new(text).color(BAD);
    ui.add_enabled(enabled, egui::Button::new(label).stroke(Stroke::new(1.0_f32, BAD)).min_size(Vec2::new(0.0, 44.0)))
}

/// The window's icon, drawn: a dark square in an accent frame with an
/// accent bar across it.
pub fn icon() -> egui::IconData {
    const N: u32 = 32;
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let edge = x < 3 || y < 3 || x >= N - 3 || y >= N - 3;
            let bar = (13..19).contains(&y) && (8..24).contains(&x);
            let c = if edge || bar { ACCENT } else { BG };
            rgba.extend_from_slice(&[c.r(), c.g(), c.b(), 255]);
        }
    }
    egui::IconData { rgba, width: N, height: N }
}
