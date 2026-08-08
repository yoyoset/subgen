use crate::app::SubgenApp;
use crate::format::*;
use crate::ui::theme;
use eframe::egui;

/// 清空列表是"一次点错丢一整个队列"的操作,虽然不影响已生成的字幕文件,但任务列表
/// 本身值得一个确认——用 egui 内存里的临时标志位(而不是往 SubgenApp 加字段)记录弹窗
/// 是否打开,保持这个 UI 状态只属于队列视图本身。
const CLEAR_CONFIRM_ID: &str = "clear_list_confirm_open";

/// 左侧任务队列(主):添加/清空 + 每行一条"状态点 + 文件名 + 当前步/进度"。完整的步骤条、
/// 状态文案、结果说明、预览、单文件操作全部搬去了 `ui::detail`——这里只负责"一眼扫清楚
/// 这一季有哪些文件、大致到哪了",详细信息按需点开一行才看。
pub(crate) fn render_queue_panel(app: &mut SubgenApp, ui: &mut egui::Ui) {
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!app.processing, egui::Button::new("选择视频文件..."))
            .clicked()
        {
            let mut dialog = rfd::FileDialog::new()
                .add_filter("视频", SubgenApp::VIDEO_EXTENSIONS)
                .set_title("选择要生成字幕的视频文件");
            if let Some(dir) = &app.config.last_dir {
                dialog = dialog.set_directory(dir);
            }
            if let Some(files) = dialog.pick_files() {
                let (added, _ignored) = app.add_files(files);
                if added == 0 {
                    // 选择框的过滤器是引导性的,用户仍可能选中非视频/已在列表中的文件——
                    // 全都没加进去时给一句反馈,而不是让界面毫无反应。
                    app.transient_notice = Some((
                        "没有添加任务:选中的文件里没有可识别的视频,或都在列表中了".to_string(),
                        std::time::Instant::now(),
                    ));
                }
            }
        }
        // 处理中直接置灰禁用,而不是"按钮外观正常、点了却静默无效"——那种写法会让用户
        // 以为按钮坏了,反复点击也得不到任何反馈。
        if ui
            .add_enabled(!app.processing, egui::Button::new("清空列表"))
            .clicked()
            && !app.jobs.is_empty()
        {
            ui.ctx()
                .data_mut(|d| d.insert_temp(egui::Id::new(CLEAR_CONFIRM_ID), true));
        }
    });
    ui.add_space(4.0);
    ui.separator();

    render_clear_confirm_dialog(app, ui);

    if app.jobs.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.colored_label(
                theme::TEXT_SECONDARY,
                "把视频文件拖到窗口里,或点上面的按钮",
            );
            ui.add_space(4.0);
            ui.colored_label(theme::TEXT_SECONDARY, "批量添加后点“开始处理”");
        });
        return;
    }

    // 任务总数:一批几十个文件时,一眼知道列表里有多少个,不用自己数。
    ui.add_space(2.0);
    ui.strong(format!("任务列表({} 个)", app.jobs.len()));
    ui.add_space(2.0);

    let active = active_job_index(&app.jobs);
    let mut clicked: Option<usize> = None;

    egui::ScrollArea::vertical().show(ui, |ui| {
        for (i, job) in app.jobs.iter().enumerate() {
            let is_pinned = app.selected == Some(i);
            let is_focused = is_pinned || (app.selected.is_none() && active == Some(i));

            let frame = egui::Frame::default()
                .inner_margin(egui::Margin::symmetric(4, 3))
                .fill(if is_focused {
                    theme::ACCENT_LIGHT.gamma_multiply(0.18)
                } else {
                    egui::Color32::TRANSPARENT
                });
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

/// "清空列表"的确认弹窗:一次误点损失整个队列的编排成本比删除单个文件高得多,值得拦一下。
/// 弹窗本身用一个临时标志位驱动,所有按钮的意图先收集、后应用,避免在闭包里改 app。
fn render_clear_confirm_dialog(app: &mut SubgenApp, ui: &mut egui::Ui) {
    let open = ui
        .ctx()
        .data(|d| d.get_temp::<bool>(egui::Id::new(CLEAR_CONFIRM_ID)))
        .unwrap_or(false);
    if !open {
        return;
    }
    let count = app.jobs.len();
    let mut decided: Option<bool> = None; // Some(true) = 确认清空, Some(false) = 取消
    egui::Window::new("清空列表?")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ui.ctx(), |ui| {
            ui.label(format!(
                "将移除列表中的 {count} 个任务(已生成的字幕文件不受影响,只会从列表拿掉)。"
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    decided = Some(false);
                }
                if ui.button("清空").clicked() {
                    decided = Some(true);
                }
            });
        });
    match decided {
        Some(true) => {
            app.jobs.clear();
            app.selected = None;
            // 复核报告持有的是任务列表下标,列表清空后这些下标要么失效、要么被新加入的
            // 文件重新占用——不清理的话,下次生成报告会基于清空前的残留状态,或者更糟,
            // 新的第 0 个文件显示上一个文件的旧报告。
            app.review = None;
            ui.ctx()
                .data_mut(|d| d.insert_temp(egui::Id::new(CLEAR_CONFIRM_ID), false));
        }
        Some(false) => {
            ui.ctx()
                .data_mut(|d| d.insert_temp(egui::Id::new(CLEAR_CONFIRM_ID), false));
        }
        None => {}
    }
}
