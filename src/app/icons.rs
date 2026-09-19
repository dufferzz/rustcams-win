//! Phosphor icons as an egui font fallback (toolbar, sidebar, PTZ chrome).

use eframe::egui;

pub use egui_phosphor::regular::*;

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}

pub fn labeled(icon: &str, text: &str) -> String {
    format!("{icon}  {text}")
}
