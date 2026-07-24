use crate::app::SubgenApp;
use crate::paths;
use eframe::egui;

/// "组件/存放位置"是独立于设置的按需浮窗——模型存放目录这类"环境"信息,处理中也可能
/// 想确认(这次到底用的是哪个目录),而设置项处理中打开是只读摘要,两者的处理中行为
/// 不一样,拆成两个入口比揉进一个窗口里再各自加条件判断更清楚。
pub(crate) fn render_components_window(app: &mut SubgenApp, ctx: &egui::Context) {
    if !app.components_open {
        return;
    }
    let mut open = app.components_open;
    egui::Window::new("组件与存放位置")
        .open(&mut open)
        .resizable(false)
        .show(ctx, |ui| {
            // 常驻显示 + 可改,不止首次下载那一次机会——用了一段时间之后想搬家也应该
            // 随时能改,不应该被困在当初选的位置。处理中禁用:一批任务跑到一半时,
            // 后面文件的翻译阶段会用当时刚查到的模型路径去启动 llama-server,这时候
            // 换目录容易踩"模型文件还没搬过去"的空。
            ui.label(format!("模型位置: {}", paths::models_dir().display()));
            if ui
                .add_enabled(!app.processing, egui::Button::new("更改..."))
                .clicked()
            {
                app.begin_change_models_dir();
            }
            if let Some(e) = &app.models_dir_error {
                ui.colored_label(egui::Color32::RED, e);
            }
        });
    app.components_open = open;
}
