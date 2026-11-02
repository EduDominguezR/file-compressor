//! Componentes planos: bordes finos, sin sombras.

use eframe::egui::{
    self, Color32, CornerRadius, CursorIcon, Margin, Response, RichText, Sense, Stroke,
};

use super::theme;

pub fn dim(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(theme::TEXT_DIM).size(12.5)
}

pub fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(
        RichText::new(text)
            .color(theme::TEXT_DIM)
            .size(12.0)
            .strong(),
    );
}

/// Caja plana con borde fino.
pub fn flat_box<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn primary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> Response {
    let w = ui.available_width();
    ui.add_enabled(
        enabled,
        egui::Button::new(
            RichText::new(text)
                .size(16.0)
                .strong()
                .color(Color32::WHITE),
        )
        .fill(if enabled {
            theme::ACCENT
        } else {
            theme::BORDER
        })
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .min_size(egui::vec2(w, 44.0)),
    )
}

pub fn secondary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).size(13.5))
            .corner_radius(CornerRadius::same(theme::RADIUS)),
    )
}

pub fn full_width_secondary(ui: &mut egui::Ui, text: &str) -> Response {
    let w = ui.available_width();
    ui.add(
        egui::Button::new(RichText::new(text).size(14.0).strong())
            .corner_radius(CornerRadius::same(theme::RADIUS))
            .min_size(egui::vec2(w, 34.0)),
    )
}

/// Opción seleccionable (modo de compresión): borde de acento cuando está activa.
pub fn option_box(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    desc: &str,
    selected: bool,
    enabled: bool,
) -> Response {
    let stroke = if selected {
        Stroke::new(1.5_f32, theme::ACCENT)
    } else {
        Stroke::new(1.0_f32, theme::BORDER)
    };
    let fill = if selected {
        Color32::from_rgb(0x22, 0x2b, 0x3d)
    } else {
        theme::SURFACE
    };
    let framed = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let title_color = if enabled {
                theme::TEXT
            } else {
                theme::TEXT_DIM
            };
            ui.label(RichText::new(title).strong().color(title_color));
            ui.label(dim(desc));
        });
    let resp = ui.interact(framed.response.rect, ui.id().with(id), Sense::click());
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Celda de estadística: etiqueta pequeña + valor destacado.
/// La etiqueta nunca se parte letra a letra: el `Grid` que la contiene fija el ancho de columna.
pub fn stat(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    ui.vertical(|ui| {
        ui.add(
            egui::Label::new(dim(label))
                .wrap_mode(egui::TextWrapMode::Extend)
                .selectable(false),
        );
        ui.add(
            egui::Label::new(RichText::new(value).size(17.0).strong().color(color))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    });
}
