//! Diseño plano, oscuro y minimalista. Sin sombras, gradientes ni animaciones: en reposo no se
//! repinta. Tipografía del sistema (se carga una vez al arrancar; si no hay, la de egui).

use eframe::egui::{self, Color32, CornerRadius, Stroke};

pub const BG: Color32 = Color32::from_rgb(0x1b, 0x1a, 0x18);
pub const SURFACE: Color32 = Color32::from_rgb(0x24, 0x23, 0x20);
pub const BORDER: Color32 = Color32::from_rgb(0x3a, 0x38, 0x32);
/// Fondo de la pista de la barra de progreso (debe distinguirse del fondo del panel).
pub const TRACK: Color32 = Color32::from_rgb(0x31, 0x30, 0x2c);
pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xe6, 0xe1);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xa6, 0xa3, 0x9b);
pub const ACCENT: Color32 = Color32::from_rgb(0x4c, 0x8b, 0xf5);
pub const OK: Color32 = Color32::from_rgb(0x58, 0xb8, 0x89);
pub const WARN: Color32 = Color32::from_rgb(0xe3, 0xad, 0x55);
pub const ERR: Color32 = Color32::from_rgb(0xe0, 0x6c, 0x65);
/// Radio de esquina pequeño (diseño plano).
pub const RADIUS: u8 = 4;

pub fn apply(ctx: &egui::Context) {
    install_system_font(ctx);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    // La barra de progreso usa `extreme_bg_color` como pista; si fuera igual al fondo del
    // panel, en 0 % solo se vería el punto inicial. TRACK es un gris un poco más claro.
    v.extreme_bg_color = TRACK;
    v.faint_bg_color = SURFACE;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    v.hyperlink_color = ACCENT;
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    let w = &mut v.widgets;
    for (state, fill) in [
        (&mut w.noninteractive, BG),
        (&mut w.inactive, SURFACE),
        (&mut w.hovered, Color32::from_rgb(0x2d, 0x2c, 0x28)),
        (&mut w.active, Color32::from_rgb(0x33, 0x31, 0x2c)),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0_f32, BORDER);
        state.fg_stroke = Stroke::new(1.0_f32, TEXT);
        state.corner_radius = CornerRadius::same(RADIUS);
    }
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 7.0);
    });
}

/// Intenta cargar una fuente del sistema UNA vez al arrancar (no por frame).
fn install_system_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\segoeui.ttf",
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "system".to_owned(),
                egui::FontData::from_owned(bytes).into(),
            );
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                f.insert(0, "system".to_owned());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}
