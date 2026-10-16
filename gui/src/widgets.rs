use std::f32::consts::PI;

use eframe::egui::{
    self, Align, Color32, Id, Layout, Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind,
    Ui, UiBuilder, Vec2, pos2, vec2,
};

use crate::theme;

/// Content narrower than this gets one column.
const TWO_COLUMNS: f32 = 860.0;
const COLUMN_GAP: f32 = 28.0;

pub fn switch(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let size = ui.spacing().interact_size.y * vec2(1.9, 1.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, label)
    });
    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(response.id, *on);
        let visuals = ui.style().interact_selectable(&response, *on);
        let rect = rect.expand(visuals.expansion);
        let radius = 0.5 * rect.height();
        let fill = if *on {
            theme::ACCENT
        } else if ui.visuals().dark_mode {
            visuals.bg_fill
        } else {
            // The light theme's widget fill barely shows on a white background.
            Color32::from_gray(190)
        };
        ui.painter()
            .rect(rect, radius, fill, visuals.bg_stroke, StrokeKind::Inside);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        ui.painter()
            .circle_filled(pos2(x, rect.center().y), 0.72 * radius, Color32::WHITE);
    }
    response
}

/// An effect with a switch on the right. `options`, when given, shows under it once the
/// chevron or title opens the row, and is only editable while the effect is on.
pub fn effect_row(
    ui: &mut Ui,
    id: &str,
    title: &str,
    description: &str,
    on: &mut bool,
    options: Option<&mut dyn FnMut(&mut Ui)>,
) {
    // An explicit id keeps sliders inside working while banners or the action bar come and go.
    ui.scope_builder(UiBuilder::new().id(Id::new(("row", id))), |ui| {
        let open_id = ui.id().with("open");
        let mut open = ui.data(|d| d.get_temp(open_id)).unwrap_or(false);
        let chevron = 22.0;
        let switch_width = ui.spacing().interact_size.y * 1.9;
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if options.is_some() {
                let (rect, response) =
                    ui.allocate_exact_size(vec2(chevron - 6.0, 22.0), Sense::click());
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::CollapsingHeader,
                        ui.is_enabled(),
                        open,
                        title,
                    )
                });
                let color = ui.style().interact(&response).fg_stroke.color;
                paint_chevron(ui, rect, open, color);
                if response.clicked() {
                    open = !open;
                }
            } else {
                ui.allocate_exact_size(vec2(chevron - 6.0, 22.0), Sense::hover());
            }
            let text_width = (ui.available_width() - switch_width - 12.0).max(80.0);
            ui.vertical(|ui| {
                ui.set_width(text_width);
                let title = egui::Label::new(RichText::new(title).size(15.5).strong());
                let response = if options.is_some() {
                    ui.add(title.sense(Sense::click()))
                } else {
                    ui.add(title)
                };
                if options.is_some() && response.clicked() {
                    open = !open;
                }
                ui.label(RichText::new(description).color(theme::MUTED));
            });
            ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                switch(ui, on, title);
            });
        });
        if let (true, Some(options)) = (open, options) {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(chevron);
                ui.vertical(|ui| {
                    ui.add_enabled_ui(*on, |ui| options(ui));
                });
            });
        }
        ui.data_mut(|d| d.insert_temp(open_id, open));
        ui.add_space(10.0);
        let line = theme::surface(ui).line;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter()
            .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, line));
    });
}

fn paint_chevron(ui: &Ui, rect: Rect, open: bool, color: Color32) {
    let c = rect.center();
    let s = 4.5;
    let points = if open {
        vec![
            pos2(c.x - s, c.y - s / 2.0),
            pos2(c.x, c.y + s / 2.0),
            pos2(c.x + s, c.y - s / 2.0),
        ]
    } else {
        vec![
            pos2(c.x - s / 2.0, c.y - s),
            pos2(c.x + s / 2.0, c.y),
            pos2(c.x - s / 2.0, c.y + s),
        ]
    };
    ui.painter()
        .add(egui::Shape::line(points, Stroke::new(1.8, color)));
}

/// An accent-colored heading over a group of rows.
pub fn section_title(ui: &mut Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(
        RichText::new(title.to_uppercase())
            .size(16.0)
            .strong()
            .color(theme::accent_text(ui)),
    );
    ui.add_space(2.0);
}

pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let fill = theme::surface(ui).card;
    let inner = egui::Frame::new()
        .fill(fill)
        .corner_radius(6)
        .inner_margin(14)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner;
    ui.add_space(10.0);
    inner
}

/// The left and right columns, or `None` when the content is too narrow for two.
pub fn split(ui: &Ui) -> Option<(Rect, Rect)> {
    let area = ui.available_rect_before_wrap();
    if area.width() < TWO_COLUMNS {
        return None;
    }
    let left_width = ((area.width() - COLUMN_GAP) * 0.5).clamp(400.0, 600.0);
    let left = Rect::from_min_size(area.min, vec2(left_width, area.height()));
    let right = Rect::from_min_max(pos2(left.max.x + COLUMN_GAP, area.min.y), area.max);
    Some((left, right))
}

/// A column scrolling on its own.
pub fn pane(ui: &mut Ui, rect: Rect, id: &str, add: impl FnOnce(&mut Ui)) {
    ui.scope_builder(UiBuilder::new().max_rect(rect).id_salt(id), |ui| {
        egui::ScrollArea::vertical()
            .id_salt(id)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width() - 12.0);
                add(ui);
                ui.add_space(8.0);
            });
    });
}

#[derive(Clone, Copy)]
pub enum Icon {
    Audio,
    Video,
    Settings,
}

pub fn paint_icon(ui: &Ui, icon: Icon, center: Pos2, size: f32, color: Color32) {
    let painter = ui.painter();
    let stroke = Stroke::new(size * 0.09, color);
    let r = size / 2.0;
    match icon {
        Icon::Audio => {
            let points = (0..=24)
                .map(|i| {
                    #[allow(clippy::cast_precision_loss)]
                    let angle = PI + PI * i as f32 / 24.0;
                    center + Vec2::angled(angle) * r * 0.8 + vec2(0.0, r * 0.15)
                })
                .collect();
            painter.add(egui::Shape::line(points, stroke));
            for side in [-1.0, 1.0] {
                let cup = Rect::from_center_size(
                    center + vec2(side * r * 0.68, r * 0.42),
                    vec2(r * 0.42, r * 0.75),
                );
                painter.rect_filled(cup, r * 0.15, color);
            }
        }
        Icon::Video => {
            let body =
                Rect::from_min_size(center + vec2(-r * 0.95, -r * 0.5), vec2(r * 1.25, r * 1.0));
            painter.rect_filled(body, r * 0.15, color);
            let lens = vec![
                center + vec2(r * 0.4, -r * 0.05),
                center + vec2(r * 0.95, -r * 0.45),
                center + vec2(r * 0.95, r * 0.45),
                center + vec2(r * 0.4, r * 0.05),
            ];
            painter.add(egui::Shape::convex_polygon(lens, color, Stroke::NONE));
        }
        Icon::Settings => {
            for i in 0..8 {
                #[allow(clippy::cast_precision_loss)]
                let dir = Vec2::angled(PI / 4.0 * i as f32);
                painter.line_segment(
                    [center + dir * r * 0.55, center + dir * r * 0.92],
                    Stroke::new(size * 0.16, color),
                );
            }
            painter.circle_stroke(center, r * 0.58, Stroke::new(size * 0.14, color));
            painter.circle_stroke(center, r * 0.2, Stroke::new(size * 0.09, color));
        }
    }
}

/// The most urgent of several status colors: bad, then warning, then good.
pub fn worst(colors: impl IntoIterator<Item = Color32>) -> Option<Color32> {
    let rank = |c: &Color32| match *c {
        theme::BAD => 3,
        theme::WARN => 2,
        theme::GOOD => 1,
        _ => 0,
    };
    colors.into_iter().max_by_key(rank)
}
