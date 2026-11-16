//! Custom DAW UI theme, viewport icon, and Painter-drawn audio controls.

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Response, Sense, Stroke, Vec2};
use std::ops::RangeInclusive;

pub const BG_DEEP: Color32 = Color32::from_rgb(15, 17, 22);
pub const BG_PANEL: Color32 = Color32::from_rgb(21, 24, 31);
pub const BG_ELEVATED: Color32 = Color32::from_rgb(29, 33, 42);
pub const BG_WIDGET: Color32 = Color32::from_rgb(40, 45, 56);
pub const BG_WIDGET_HOVER: Color32 = Color32::from_rgb(54, 60, 74);
pub const BORDER_SUBTLE: Color32 = Color32::from_rgb(48, 54, 66);
pub const BORDER_STRONG: Color32 = Color32::from_rgb(72, 80, 98);
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(228, 232, 240);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(142, 150, 166);
pub const ACCENT_GREEN: Color32 = Color32::from_rgb(104, 214, 114);
pub const ACCENT_AMBER: Color32 = Color32::from_rgb(235, 172, 66);
pub const ACCENT_RED: Color32 = Color32::from_rgb(228, 78, 74);
pub const ACCENT_BLUE: Color32 = Color32::from_rgb(86, 168, 238);

pub fn apply_studio_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        let mut v = egui::Visuals::dark();

        v.override_text_color = Some(TEXT_PRIMARY);
        v.window_fill = BG_PANEL;
        v.panel_fill = BG_DEEP;
        v.faint_bg_color = BG_ELEVATED;
        v.extreme_bg_color = Color32::from_rgb(10, 12, 16);
        v.window_stroke = Stroke::new(1.0, BORDER_STRONG);
        v.window_corner_radius = CornerRadius::same(6);
        v.menu_corner_radius = CornerRadius::same(5);
        v.popup_shadow = egui::epaint::Shadow {
            offset: [0, 4],
            blur: 12,
            spread: 0,
            color: Color32::from_black_alpha(120),
        };

        v.selection.bg_fill = Color32::from_rgb(44, 116, 56);
        v.selection.stroke = Stroke::new(1.0, ACCENT_GREEN);

        let cr = CornerRadius::same(4);
        v.widgets.noninteractive.bg_fill = BG_PANEL;
        v.widgets.noninteractive.weak_bg_fill = BG_PANEL;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_MUTED);
        v.widgets.noninteractive.corner_radius = cr;

        v.widgets.inactive.bg_fill = BG_WIDGET;
        v.widgets.inactive.weak_bg_fill = BG_ELEVATED;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
        v.widgets.inactive.corner_radius = cr;

        v.widgets.hovered.bg_fill = BG_WIDGET_HOVER;
        v.widgets.hovered.weak_bg_fill = BG_WIDGET;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
        v.widgets.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
        v.widgets.hovered.corner_radius = cr;

        v.widgets.active.bg_fill = Color32::from_rgb(48, 112, 58);
        v.widgets.active.weak_bg_fill = Color32::from_rgb(42, 92, 50);
        v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT_GREEN);
        v.widgets.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
        v.widgets.active.corner_radius = cr;

        v.widgets.open.bg_fill = BG_WIDGET_HOVER;
        v.widgets.open.weak_bg_fill = BG_WIDGET;
        v.widgets.open.bg_stroke = Stroke::new(1.0, ACCENT_GREEN);
        v.widgets.open.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        v.widgets.open.corner_radius = cr;

        style.visuals = v;
        style.spacing.item_spacing = egui::vec2(6.0, 5.0);
        style.spacing.button_padding = egui::vec2(7.0, 3.0);
        style.spacing.menu_margin = egui::Margin::same(6);
        style.spacing.window_margin = egui::Margin::same(8);
        style.spacing.indent = 14.0;
        style.spacing.scroll = egui::style::ScrollStyle {
            bar_width: 7.0,
            handle_min_length: 18.0,
            ..Default::default()
        };
    });
}

pub fn dock_style(style: &egui::Style) -> egui_dock::Style {
    let mut ds = egui_dock::Style::from_egui(style);
    ds.dock_area_padding = Some(egui::Margin::same(4));
    ds.main_surface_border_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    ds.main_surface_border_rounding = CornerRadius::same(4);
    ds.separator.width = 4.0;
    ds.separator.color_idle = BG_DEEP;
    ds.separator.color_hovered = BORDER_STRONG;
    ds.separator.color_dragged = ACCENT_GREEN;

    ds.tab_bar.bg_fill = BG_DEEP;
    ds.tab_bar.hline_color = BORDER_SUBTLE;
    ds.tab_bar.height = 26.0;
    ds.tab_bar.corner_radius = CornerRadius::ZERO;

    ds.tab.tab_body.bg_fill = BG_PANEL;
    ds.tab.tab_body.stroke = Stroke::new(1.0, BORDER_SUBTLE);
    ds.tab.tab_body.corner_radius = CornerRadius::same(4);
    ds.tab.tab_body.inner_margin = egui::Margin::same(6);

    ds.tab.active.bg_fill = BG_PANEL;
    ds.tab.active.text_color = TEXT_PRIMARY;
    ds.tab.active.outline_color = BORDER_STRONG;
    ds.tab.focused.bg_fill = BG_ELEVATED;
    ds.tab.focused.text_color = ACCENT_GREEN;
    ds.tab.focused.outline_color = ACCENT_GREEN;
    ds.tab.inactive.bg_fill = BG_DEEP;
    ds.tab.inactive.text_color = TEXT_MUTED;
    ds.tab.inactive.outline_color = Color32::TRANSPARENT;
    ds.tab.hovered.bg_fill = BG_ELEVATED;
    ds.tab.hovered.text_color = TEXT_PRIMARY;
    ds.tab.hovered.outline_color = BORDER_SUBTLE;
    ds
}

/// Generate a crisp 64x64 RGBA window icon matching `assets/icon.svg`.
pub fn build_app_icon() -> egui::IconData {
    let size = 64usize;
    let mut rgba = vec![0u8; size * size * 4];
    let bolt: [(f32, f32); 6] = [
        (35.0, 10.0),
        (21.0, 36.0),
        (30.0, 36.0),
        (28.0, 54.0),
        (43.0, 27.0),
        (33.0, 27.0),
    ];

    let in_poly = |x: f32, y: f32| -> bool {
        let mut inside = false;
        let mut j = bolt.len() - 1;
        for i in 0..bolt.len() {
            let (xi, yi) = bolt[i];
            let (xj, yj) = bolt[j];
            if ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi + 1e-6) + xi) {
                inside = !inside;
            }
            j = i;
        }
        inside
    };

    for y in 0..size {
        for x in 0..size {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let rx = (fx - 32.0).abs() - 18.0;
            let ry = (fy - 32.0).abs() - 18.0;
            let dist = rx.max(0.0).hypot(ry.max(0.0)) + rx.max(ry).min(0.0) - 10.0;
            let idx = (y * size + x) * 4;
            if dist > 0.5 {
                continue;
            }
            let alpha = ((0.5 - dist).clamp(0.0, 1.0) * 255.0) as u8;
            let border = dist > -2.5;
            let is_bolt = in_poly(fx, fy);
            let is_step = (48.0..=51.0).contains(&fy)
                && ((11.0..=18.0).contains(&fx)
                    || (20.0..=27.0).contains(&fx)
                    || (29.0..=36.0).contains(&fx)
                    || (38.0..=45.0).contains(&fx)
                    || (47.0..=53.0).contains(&fx));
            let active_step = is_step && (29.0..=36.0).contains(&fx);
            let col = if border || is_bolt || active_step {
                [110, 220, 120, alpha]
            } else if is_step {
                [58, 64, 76, alpha]
            } else {
                [24, 27, 33, alpha]
            };
            rgba[idx..idx + 4].copy_from_slice(&col);
        }
    }

    egui::IconData {
        rgba,
        width: size as u32,
        height: size as u32,
    }
}

pub fn norm_of(value: f32, range: &RangeInclusive<f32>) -> f32 {
    let span = (*range.end() - *range.start()).max(1e-6);
    ((value - *range.start()) / span).clamp(0.0, 1.0)
}

pub fn value_from_norm(norm: f32, range: &RangeInclusive<f32>) -> f32 {
    let n = norm.clamp(0.0, 1.0);
    *range.start() + n * (*range.end() - *range.start())
}

/// Compact rotary DAW knob: drag vertically, hold Shift for fine control, double-click to reset.
pub fn daw_knob(
    u: &mut egui::Ui,
    value: &mut f32,
    range: RangeInclusive<f32>,
    default: f32,
    label: &str,
    accent: Color32,
) -> Response {
    let diameter: f32 = 26.0;
    let total_h = if label.is_empty() {
        diameter
    } else {
        diameter + 13.0
    };
    let width = diameter.max(if label.is_empty() { 26.0 } else { 36.0 });
    let (rect, mut resp) =
        u.allocate_exact_size(Vec2::new(width, total_h), Sense::click_and_drag());

    if resp.double_clicked() {
        *value = default.clamp(*range.start(), *range.end());
        resp.mark_changed();
    } else if resp.dragged() {
        let dy = u.ctx().input(|i| i.pointer.delta().y);
        if dy.abs() > 0.0 {
            let fine = if u.ctx().input(|i| i.modifiers.shift) {
                0.22
            } else {
                1.0
            };
            let span = (*range.end() - *range.start()).max(1e-6);
            *value = (*value - (dy / 110.0) * span * fine).clamp(*range.start(), *range.end());
            resp.mark_changed();
        }
    }

    let norm = norm_of(*value, &range);
    let center = Pos2::new(rect.center().x, rect.min.y + diameter * 0.5);
    let radius = diameter * 0.46;
    let painter = u.painter_at(rect);

    let start_ang = std::f32::consts::PI * 0.75;
    let sweep = std::f32::consts::PI * 1.5;
    let arc_pts = |a0: f32, a1: f32, r: f32, steps: usize| -> Vec<Pos2> {
        (0..=steps)
            .map(|k| {
                let t = k as f32 / steps.max(1) as f32;
                let a = a0 + (a1 - a0) * t;
                Pos2::new(center.x + a.cos() * r, center.y + a.sin() * r)
            })
            .collect()
    };

    painter.add(egui::Shape::line(
        arc_pts(start_ang, start_ang + sweep, radius, 20),
        Stroke::new(2.2, Color32::from_gray(44)),
    ));
    let cur_ang = start_ang + sweep * norm;
    if norm > 0.005 {
        painter.add(egui::Shape::line(
            arc_pts(start_ang, cur_ang, radius, 16),
            Stroke::new(2.4, accent),
        ));
    }

    let body_r = radius * 0.72;
    let body_col = if resp.dragged() || resp.hovered() {
        BG_WIDGET_HOVER
    } else {
        BG_WIDGET
    };
    painter.circle_filled(center, body_r, body_col);
    painter.circle_stroke(center, body_r, Stroke::new(1.0, BORDER_STRONG));

    let p_in = Pos2::new(
        center.x + cur_ang.cos() * (body_r * 0.25),
        center.y + cur_ang.sin() * (body_r * 0.25),
    );
    let p_out = Pos2::new(
        center.x + cur_ang.cos() * (body_r * 0.88),
        center.y + cur_ang.sin() * (body_r * 0.88),
    );
    painter.line_segment([p_in, p_out], Stroke::new(1.8, TEXT_PRIMARY));

    if !label.is_empty() {
        painter.text(
            Pos2::new(rect.center().x, rect.max.y - 6.0),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(10.0),
            if resp.hovered() || resp.dragged() {
                TEXT_PRIMARY
            } else {
                TEXT_MUTED
            },
        );
    }

    resp
}

pub struct FaderSpec {
    pub range: RangeInclusive<f32>,
    pub default: f32,
    pub peak: f32,
    pub rms: f32,
    pub accent: Color32,
    pub height: f32,
}

/// Vertical channel fader paired with an integrated Peak/RMS level bar.
pub fn daw_fader_meter(u: &mut egui::Ui, value: &mut f32, spec: FaderSpec) -> Response {
    let width = 34.0;
    let (rect, mut resp) =
        u.allocate_exact_size(Vec2::new(width, spec.height), Sense::click_and_drag());
    let pad_y = 7.0;
    let travel = (rect.height() - pad_y * 2.0).max(16.0);
    let track_top = rect.min.y + pad_y;
    let track_bot = rect.max.y - pad_y;

    if resp.double_clicked() {
        *value = spec.default.clamp(*spec.range.start(), *spec.range.end());
        resp.mark_changed();
    } else if resp.dragged()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let n = 1.0 - ((pos.y - track_top) / travel).clamp(0.0, 1.0);
        *value = value_from_norm(n, &spec.range);
        resp.mark_changed();
    }

    let painter = u.painter_at(rect);
    let meter_rect = Rect::from_min_max(
        Pos2::new(rect.min.x + 2.0, track_top),
        Pos2::new(rect.min.x + 9.0, track_bot),
    );
    painter.rect_filled(meter_rect, 2.0, Color32::from_gray(14));

    let pk = spec.peak.clamp(0.0, 1.0);
    if pk > 0.004 {
        let h = meter_rect.height() * pk;
        let mcol = if pk > 0.92 {
            ACCENT_RED
        } else if pk > 0.7 {
            ACCENT_AMBER
        } else {
            ACCENT_GREEN
        };
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(meter_rect.min.x + 1.0, meter_rect.max.y - h),
                Pos2::new(meter_rect.max.x - 1.0, meter_rect.max.y),
            ),
            1.0,
            mcol,
        );
    }
    let rm = spec.rms.clamp(0.0, 1.0);
    if rm > 0.004 {
        let h = meter_rect.height() * rm;
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(meter_rect.min.x + 2.0, meter_rect.max.y - h),
                Pos2::new(meter_rect.max.x - 2.0, meter_rect.max.y),
            ),
            0.0,
            Color32::from_rgba_unmultiplied(86, 168, 238, 175),
        );
    }

    // Fader slot + tick marks.
    let fader_cx = rect.min.x + 22.0;
    for t in [0.0_f32, 0.25, 0.5, 0.67, 0.85, 1.0] {
        let ty = track_bot - t * travel;
        painter.line_segment(
            [Pos2::new(fader_cx - 7.0, ty), Pos2::new(fader_cx - 3.5, ty)],
            Stroke::new(1.0, BORDER_SUBTLE),
        );
    }
    let slot_rect =
        Rect::from_center_size(Pos2::new(fader_cx, rect.center().y), Vec2::new(4.0, travel));
    painter.rect_filled(slot_rect, 2.0, Color32::from_gray(13));

    let norm = norm_of(*value, &spec.range);
    let knob_y = track_bot - norm * travel;
    if norm > 0.005 {
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(fader_cx - 1.5, knob_y),
                Pos2::new(fader_cx + 1.5, track_bot),
            ),
            1.0,
            spec.accent,
        );
    }
    let cap = Rect::from_center_size(Pos2::new(fader_cx, knob_y), Vec2::new(16.0, 10.0));
    painter.rect_filled(
        cap,
        2.0,
        if resp.hovered() || resp.dragged() {
            Color32::from_rgb(84, 92, 110)
        } else {
            Color32::from_rgb(64, 70, 84)
        },
    );
    painter.rect_stroke(
        cap,
        2.0,
        Stroke::new(1.0, BORDER_STRONG),
        egui::StrokeKind::Inside,
    );
    painter.line_segment(
        [
            Pos2::new(cap.min.x + 3.0, cap.center().y),
            Pos2::new(cap.max.x - 3.0, cap.center().y),
        ],
        Stroke::new(1.2, TEXT_PRIMARY),
    );

    resp
}

pub fn led_toggle(
    u: &mut egui::Ui,
    on: bool,
    label: &str,
    active_col: Color32,
    size: Vec2,
) -> Response {
    let (rect, resp) = u.allocate_exact_size(size, Sense::click());
    let p = u.painter_at(rect);
    let fill = if on {
        active_col.gamma_multiply(0.32)
    } else if resp.hovered() {
        BG_WIDGET_HOVER
    } else {
        BG_WIDGET
    };
    let stroke = if on {
        active_col
    } else if resp.hovered() {
        BORDER_STRONG
    } else {
        BORDER_SUBTLE
    };
    p.rect_filled(rect, 3.0, fill);
    p.rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        if on { Color32::WHITE } else { TEXT_MUTED },
    );
    resp
}

pub fn draw_step_pad(
    painter: &egui::Painter,
    rect: Rect,
    active: bool,
    playhead: bool,
    beat_alt: bool,
    hovered: bool,
    accent: Color32,
) {
    let base = if active {
        accent.gamma_multiply(if playhead { 1.0 } else { 0.85 })
    } else if playhead {
        Color32::from_rgb(76, 62, 28)
    } else if beat_alt {
        Color32::from_rgb(45, 50, 62)
    } else {
        Color32::from_rgb(31, 35, 44)
    };
    let stroke = if playhead {
        ACCENT_AMBER
    } else if active {
        accent
    } else if hovered {
        BORDER_STRONG
    } else {
        BORDER_SUBTLE
    };
    painter.rect_filled(rect, 3.0, base);
    painter.rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );

    // Small LED indicator line near top of each step pad.
    let led_rect = Rect::from_min_max(
        Pos2::new(rect.min.x + 4.0, rect.min.y + 3.0),
        Pos2::new(rect.max.x - 4.0, rect.min.y + 5.5),
    );
    let led_col = if active {
        Color32::from_rgba_unmultiplied(245, 255, 245, 210)
    } else {
        Color32::from_rgba_unmultiplied(15, 18, 24, 150)
    };
    painter.rect_filled(led_rect, 1.0, led_col);
}

pub fn draw_peak_meter_h(u: &mut egui::Ui, peak: f32, width: f32, height: f32) {
    let (rect, _) = u.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let p = u.painter_at(rect);
    p.rect_filled(rect, 3.0, Color32::from_gray(12));
    p.rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, BORDER_SUBTLE),
        egui::StrokeKind::Inside,
    );
    let pk = peak.clamp(0.0, 1.0);
    if pk > 0.004 {
        let fill_w = (rect.width() - 2.0) * pk;
        let col = if pk > 0.92 {
            ACCENT_RED
        } else if pk > 0.72 {
            ACCENT_AMBER
        } else {
            ACCENT_GREEN
        };
        p.rect_filled(
            Rect::from_min_size(
                Pos2::new(rect.min.x + 1.0, rect.min.y + 1.0),
                Vec2::new(fill_w, (rect.height() - 2.0).max(1.0)),
            ),
            2.0,
            col,
        );
    }
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{:.0}%", pk * 100.0),
        egui::FontId::monospace(10.0),
        TEXT_PRIMARY,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_64x64_rgba_with_transparent_corners_and_green_bolt() {
        let icon = build_app_icon();
        assert_eq!(icon.width, 64);
        assert_eq!(icon.height, 64);
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);
        // Top-left corner outside rounded rect must be transparent.
        assert_eq!(icon.rgba[3], 0);
        // Center pixel sits on the green lightning bolt.
        let center_idx = (32 * 64 + 32) * 4;
        assert!(icon.rgba[center_idx + 1] > 180);
        assert_eq!(icon.rgba[center_idx + 3], 255);
    }

    #[test]
    fn norm_roundtrip_clamps_range() {
        let r = -1.0..=1.0;
        assert!((norm_of(0.0, &r) - 0.5).abs() < 1e-6);
        assert!((value_from_norm(1.5, &r) - 1.0).abs() < 1e-6);
        assert!((value_from_norm(-0.5, &r) - (-1.0)).abs() < 1e-6);
    }
}
