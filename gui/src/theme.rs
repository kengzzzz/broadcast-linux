use eframe::egui::{self, Color32};

pub const GOOD: Color32 = Color32::from_rgb(70, 170, 90);
pub const WARN: Color32 = Color32::from_rgb(220, 150, 40);
pub const BAD: Color32 = Color32::from_rgb(215, 75, 65);
pub const MUTED: Color32 = Color32::from_gray(140);

pub fn install(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.spacing.slider_width = 180.0;
    });
}

pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let size = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size * 0.6, size), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size * 0.25, color);
}
