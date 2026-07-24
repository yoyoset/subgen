use crate::app::SubgenApp;
use eframe::egui;

impl SubgenApp {
    pub fn show_main_ui(&mut self, ui: &mut egui::Ui) {
        crate::ui::settings::render_settings_panel(self, ui);
        crate::ui::actions::render_action_bar(self, ui);
        crate::ui::queue::render_job_list(self, ui);
    }
}
