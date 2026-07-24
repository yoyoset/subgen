use crate::app::SubgenApp;
use eframe::egui;

impl SubgenApp {
    /// 主从式工作队列:顶部批次条(常驻)+ 左侧任务队列(主)+ 右侧详情区(从,自动跟随
    /// 正在处理的文件,手动点选则钉住)。设置/组件是按需弹出的浮窗,不占常驻空间。
    pub fn show_main_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        crate::ui::batch_bar::render_batch_bar(self, ui);

        egui::Panel::left("queue")
            .resizable(true)
            .default_size(260.0)
            .show(ui, |ui| {
                crate::ui::queue::render_queue_panel(self, ui);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            crate::ui::detail::render_detail_pane(self, ui);
        });

        crate::ui::settings::render_settings_window(self, &ctx);
        crate::ui::components::render_components_window(self, &ctx);
    }
}
