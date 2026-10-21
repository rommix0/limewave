//! The Edit menu: selection and trim actions.

use eframe::egui::{self, Button, DragValue};
use limewave_core::edit::LoopMode;

use crate::app::{LimewaveApp, SELECT_ALL_SHORTCUT};

/// The toolbar's Edit menu. Selection items need a selection and loop items a loop; an item
/// that cannot run is greyed out.
pub fn edit_menu(ui: &mut egui::Ui, app: &mut LimewaveApp) {
    let loaded = app.selected_session().is_some();
    let looped = app
        .selected_session()
        .is_some_and(|s| s.shown_edit().loop_mode != LoopMode::Off);
    let selected = loaded && app.ui.selection.is_some();
    let can_play = selected && app.engine.is_some();
    ui.menu_button("Edit", |ui| {
        let shortcut = ui.ctx().format_shortcut(&SELECT_ALL_SHORTCUT);
        let select_all = Button::new("Select all").shortcut_text(shortcut);
        if ui.add_enabled(loaded, select_all).clicked() {
            app.select_all();
        }
        if ui
            .add_enabled(can_play, Button::new("Play selection"))
            .on_hover_text("Play the selection once at the root key")
            .clicked()
        {
            app.play_selection();
        }
        if ui
            .add_enabled(selected, Button::new("Set loop to selection"))
            .clicked()
        {
            app.set_loop_to_selection();
        }
        if ui
            .add_enabled(selected, Button::new("Set Start/End to selection"))
            .clicked()
        {
            app.set_trim_to_selection();
        }
        ui.separator();
        if ui
            .add_enabled(looped, Button::new("Trim after loop end"))
            .on_hover_text("End = loop end + padding")
            .clicked()
        {
            app.trim_after_loop();
        }
        if ui
            .add_enabled(looped, Button::new("Crop to loop"))
            .on_hover_text("Start = loop start, End = loop end + padding")
            .clicked()
        {
            app.crop_to_loop();
        }
        ui.horizontal(|ui| {
            let label = ui.label("Padding");
            ui.add(
                DragValue::new(&mut app.ui.save_dialog.padding)
                    .range(0..=4096)
                    .suffix(" frames"),
            )
            .labelled_by(label.id)
            .on_hover_text("Frames kept after the loop end (also used by Save as…)");
        });
    });
}
