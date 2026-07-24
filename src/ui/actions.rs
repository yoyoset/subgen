use crate::app::SubgenApp;
use crate::job;
use eframe::egui;
use std::sync::atomic::Ordering;

/// 底部批次控制条:选文件/清空/开始/取消 + 汇总文案 + GPU 警告 + 临时提示。
pub(crate) fn render_action_bar(app: &mut SubgenApp, ui: &mut egui::Ui) {
        egui::Panel::bottom("actions").show(ui, |ui| {
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
                // 处理中直接置灰禁用,而不是"按钮外观正常、点了却静默无效"——那种写法
                // 会让用户以为按钮坏了,反复点击也得不到任何反馈。
                if ui
                    .add_enabled(!app.processing, egui::Button::new("清空列表"))
                    .clicked()
                {
                    app.jobs.clear();
                }
                ui.add_space(16.0);

                let can_start = !app.jobs.is_empty() && !app.processing;
                if ui
                    .add_enabled(can_start, egui::Button::new("开始处理"))
                    .clicked()
                {
                    app.start_processing();
                }
                // 取消按钮点击后立即变灰、文案改成"正在取消…"——worker 线程要等当前这一步
                // (比如正在跑的 whisper-cli 子进程)结束才会真正检查取消标志,这中间有肉眼
                // 可见的延迟。不给即时反馈的话,用户会以为点击没生效,进而连点甚至直接关窗。
                let cancel_label = if app.cancelling { "正在取消…" } else { "取消" };
                if ui
                    .add_enabled(
                        app.processing && !app.cancelling,
                        egui::Button::new(cancel_label),
                    )
                    .clicked()
                {
                    app.worker_cancel.store(true, Ordering::Relaxed);
                    app.cancelling = true;
                }
            });
            if let Some(summary) = &app.last_summary {
                ui.label(summary);
            }
            if !app.gpu_warning_stages.is_empty() {
                // 按流水线顺序(识别在前、翻译在后)拼出受影响的阶段名,而不是笼统一句
                // "没用上 GPU"——识别和翻译是分别独立检测的,用户应该知道具体是哪一步慢。
                // "CUDA 环境"对不知道自己有没有独立显卡的用户是天书,给出的建议要么可执行
                // (更新驱动),要么直接告诉他们"你可能就是没有这个硬件",别让人对着一句
                // 看不懂的话不知道下一步该做什么。
                let mut stages: Vec<job::Stage> = app.gpu_warning_stages.iter().copied().collect();
                stages.sort_by_key(|s| s.ordinal());
                let stages_text = stages
                    .iter()
                    .map(|s| s.label())
                    .collect::<Vec<_>>()
                    .join("和");
                ui.colored_label(
                    egui::Color32::from_rgb(230, 150, 20),
                    format!(
                        "⚠ {stages_text}阶段未能使用显卡加速,处理会慢很多。请到 NVIDIA 官网\
                         更新显卡驱动后重试;如果这台电脑没有 NVIDIA 显卡,只能使用较慢的模式。"
                    ),
                );
            }
            if let Some((text, _)) = &app.transient_notice {
                ui.colored_label(egui::Color32::from_rgb(230, 150, 20), text);
            }
            ui.add_space(6.0);
        });

}
