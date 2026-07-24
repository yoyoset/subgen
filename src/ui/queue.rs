use crate::app::SubgenApp;
use crate::format::*;
use eframe::egui;

/// 左侧任务队列(主):添加/清空 + 每行一条"状态点 + 文件名 + 当前步/进度"。完整的步骤条、
/// 状态文案、结果说明、预览、单文件操作全部搬去了 `ui::detail`——这里只负责"一眼扫清楚
/// 这一季有哪些文件、大致到哪了",详细信息按需点开一行才看。
pub(crate) fn render_queue_panel(app: &mut SubgenApp, ui: &mut egui::Ui) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button("选择视频文件...").clicked() {
            if let Some(files) = rfd::FileDialog::new()
                .add_filter("视频", SubgenApp::VIDEO_EXTENSIONS)
                .set_title("选择要生成字幕的视频文件")
                .pick_files()
            {
                app.add_files(files);
            }
        }
        // 处理中直接置灰禁用,而不是"按钮外观正常、点了却静默无效"——那种写法会让用户
        // 以为按钮坏了,反复点击也得不到任何反馈。
        if ui
            .add_enabled(!app.processing, egui::Button::new("清空列表"))
            .clicked()
        {
            app.jobs.clear();
            app.selected = None;
        }
    });
    ui.add_space(4.0);
    ui.separator();

    if app.jobs.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.weak("把视频文件拖到窗口里,或点上面的按钮");
        });
        return;
    }

    let active = active_job_index(&app.jobs);
    let mut clicked: Option<usize> = None;

    egui::ScrollArea::vertical().show(ui, |ui| {
        for (i, job) in app.jobs.iter().enumerate() {
            let is_pinned = app.selected == Some(i);
            let is_focused = is_pinned || (app.selected.is_none() && active == Some(i));

            let frame = egui::Frame::default().inner_margin(egui::Margin::symmetric(4, 3)).fill(
                if is_focused {
                    ui.visuals().selection.bg_fill.gamma_multiply(0.35)
                } else {
                    egui::Color32::TRANSPARENT
                },
            );
            let row = frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(status_dot_color(&job.status), "●");
                    let name = job
                        .video_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    ui.add(egui::Label::new(&name).truncate()).on_hover_text(&name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(compact_status_text(job));
                    });
                });
            });
            // 整行都可点选(不止状态点/文件名那几个像素)——用整个 Frame 的矩形区域接收
            // 点击,比逐个给每个小控件单独加 Sense::click() 简单,且不会有点在行内空白处
            // 没反应的死区。
            if ui
                .interact(row.response.rect, ui.id().with(("job_row", i)), egui::Sense::click())
                .clicked()
            {
                clicked = Some(i);
            }
        }
    });

    if let Some(i) = clicked {
        app.selected = Some(i);
    }
}
