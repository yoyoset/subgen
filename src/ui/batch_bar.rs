use crate::app::SubgenApp;
use crate::format::*;
use crate::job;
use crate::ui::theme;
use eframe::egui;
use std::sync::atomic::Ordering;

/// 顶部批次条(常驻):开始/取消 + 第几个/共几个 + 预计剩余 + 已用时,批次进度以前只在
/// 窗口标题上能看到,这次是第一次上屏。汇总文案/GPU 警告/临时提示也在这——它们都是
/// "整批"级别的信息,不属于某个具体文件,和批次控制放在一起是同一个关注点。
pub(crate) fn render_batch_bar(app: &mut SubgenApp, ui: &mut egui::Ui) {
    egui::Panel::top("batch_bar").show(ui, |ui| {
        ui.add_space(6.0);
        // horizontal_wrapped 而不是 horizontal:窄窗口下按钮放不下时换行而不是溢出裁掉,
        // 保证窄屏也全部可见可点。
        ui.horizontal_wrapped(|ui| {
            let can_start = !app.jobs.is_empty() && !app.processing;
            // "开始处理"是用户最想一眼看到、点击次数最高的操作,用主按钮样式强调。
            if ui
                .add_enabled(can_start, theme::primary_button("▶ 开始处理"))
                .clicked()
            {
                app.start_processing();
            }
            // 取消按钮点击后立即变灰、文案改成"正在取消…"——worker 线程要等当前这一步
            // (比如正在跑的 whisper-cli 子进程)结束才会真正检查取消标志,这中间有肉眼
            // 可见的延迟。不给即时反馈的话,用户会以为点击没生效,进而连点甚至直接关窗。
            let cancel_label = if app.cancelling { "正在取消…" } else { "⏹ 取消" };
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

            ui.add_space(16.0);
            if app.processing && !app.batch_indices.is_empty() {
                let (done, total) = batch_progress_counts(&app.jobs, &app.batch_indices);
                let current = (done + 1).min(total);
                let elapsed = app
                    .batch_started_at
                    .map(|t| t.elapsed())
                    .unwrap_or_default();
                ui.label(format_batch_progress_line(current, total, elapsed, done));
            } else if let Some(summary) = &app.last_summary {
                ui.label(summary);
            }

            // 设置/组件是按需弹出的浮窗入口,靠右放,不干扰左边的批次控制。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⛭ 组件").clicked() {
                    app.components_open = !app.components_open;
                }
                if ui.button("⚙ 设置").clicked() {
                    app.settings_open = !app.settings_open;
                }
            });
        });

        // 批次进度条:和上面的"第 N/M 个"文字互为补充——文字给数字和 ETA,进度条给一
        // 眼扫过去的整体位置感(尤其一批 20 集时,数字不如一根有明确长度的条直观)。
        if app.processing && !app.batch_indices.is_empty() {
            let (done, total) = batch_progress_counts(&app.jobs, &app.batch_indices);
            let fraction = done as f32 / total.max(1) as f32;
            ui.add_space(2.0);
            ui.add(egui::ProgressBar::new(fraction).desired_width(f32::INFINITY));
            ui.add_space(2.0);
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
                theme::WARN,
                format!(
                    "⚠ {stages_text}阶段未能使用显卡加速,处理会慢很多。请到 NVIDIA 官网\
                     更新显卡驱动后重试;如果这台电脑没有 NVIDIA 显卡,只能使用较慢的模式。"
                ),
            );
        }
        if let Some((text, _)) = &app.transient_notice {
            ui.colored_label(theme::WARN, text);
        }
        // 启动时配置恢复的说明(配置文件损坏被重置)。用可关闭的横幅而不是 transient_notice
        // ——transient_notice 几秒后自动消失,这种"你的设置被重置了"的信息值得一直显示到
        // 用户明确看过。配置损坏是罕见的紧急情况,正常用户永远不会看到这条横幅。
        let mut dismiss_warning = false;
        if let Some(warning) = &app.startup_warning {
            egui::Frame::default()
                .inner_margin(egui::Margin::symmetric(8, 6))
                .fill(theme::WARN.gamma_multiply(0.15))
                .corner_radius(egui::CornerRadius::same(4))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(theme::WARN, warning);
                        if ui.small_button("知道了").clicked() {
                            dismiss_warning = true;
                        }
                    });
                });
        }
        if dismiss_warning {
            app.startup_warning = None;
        }
        ui.add_space(6.0);
    });
}
