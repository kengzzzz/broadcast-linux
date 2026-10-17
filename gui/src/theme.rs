use eframe::egui::{self, Color32, FontId, Stroke, TextStyle, Theme};

pub const ACCENT: Color32 = Color32::from_rgb(20, 184, 166);
pub const GOOD: Color32 = Color32::from_rgb(70, 170, 90);
pub const WARN: Color32 = Color32::from_rgb(220, 150, 40);
pub const BAD: Color32 = Color32::from_rgb(215, 75, 65);
pub const MUTED: Color32 = Color32::from_gray(140);

/// Darker on light backgrounds, for contrast.
pub fn accent_text(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        ACCENT
    } else {
        Color32::from_rgb(15, 118, 110)
    }
}

pub struct Surface {
    pub rail: Color32,
    pub header: Color32,
    pub card: Color32,
    pub line: Color32,
    pub preview: Color32,
}

pub fn surface(ui: &egui::Ui) -> Surface {
    if ui.visuals().dark_mode {
        Surface {
            rail: Color32::from_rgb(22, 23, 25),
            header: Color32::from_rgb(38, 39, 43),
            card: Color32::from_rgb(38, 39, 43),
            line: Color32::from_rgb(52, 54, 59),
            preview: Color32::from_rgb(44, 45, 49),
        }
    } else {
        Surface {
            rail: Color32::from_rgb(228, 231, 235),
            header: Color32::from_rgb(255, 255, 255),
            card: Color32::from_rgb(255, 255, 255),
            line: Color32::from_rgb(218, 222, 227),
            preview: Color32::from_rgb(222, 225, 230),
        }
    }
}

pub fn install(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.slider_width = 200.0;
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.5)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Heading, FontId::proportional(22.0)),
            (TextStyle::Monospace, FontId::monospace(12.5)),
        ]
        .into();
        style.visuals.hyperlink_color = ACCENT;
        style.visuals.slider_trailing_fill = true;
        style.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    });
    ctx.style_mut_of(Theme::Dark, |style| {
        let visuals = &mut style.visuals;
        visuals.panel_fill = Color32::from_rgb(30, 31, 34);
        visuals.window_fill = Color32::from_rgb(38, 39, 43);
        visuals.extreme_bg_color = Color32::from_rgb(20, 21, 23);
        visuals.selection.bg_fill = Color32::from_rgb(13, 148, 136);
    });
    ctx.style_mut_of(Theme::Light, |style| {
        let visuals = &mut style.visuals;
        visuals.panel_fill = Color32::from_rgb(243, 244, 246);
        visuals.selection.bg_fill = Color32::from_rgb(13, 148, 136);
    });
}

pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let size = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size * 0.6, size), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size * 0.25, color);
}
