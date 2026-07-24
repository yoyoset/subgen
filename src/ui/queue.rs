use crate::app::SubgenApp;
use crate::engine;
use crate::format::*;
use crate::job::{self, Job, JobStatus};
use crate::paths;
use eframe::egui;
use std::path::PathBuf;

/// 展开的字幕预览区:优先读中文翻译字幕(`Video.zh.srt`),没有就读原文字幕
/// (`Video.srt`),取前 10 条逐行显示"时间戳 + 文本"。这是验证翻译/识别质量最低成本的
/// 方式——不用离开软件、不用打开资源管理器找文件,10 秒内就能看出这次结果靠不靠谱。
/// 读不到文件(比如用户手动删了、移动了)显示一句人话而不是静默空白或 panic。
pub(crate) fn render_subtitle_preview(ui: &mut egui::Ui, job: &Job) {
    let zh = paths::sidecar_srt_lang_path(&job.video_path, "zh");
    let path = if zh.is_file() {
        Some(zh)
    } else {
        let srt = paths::sidecar_srt_path(&job.video_path);
        if srt.is_file() {
            Some(srt)
        } else {
            None
        }
    };

    egui::Frame::default()
        .inner_margin(egui::Margin::symmetric(12, 8))
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| match path.and_then(|p| std::fs::read_to_string(&p).ok()) {
            Some(content) => {
                let blocks = engine::parse_srt_blocks(&content);
                if blocks.is_empty() {
                    ui.label("(字幕为空,没有识别出对白)");
                } else {
                    for (start, _end, text) in blocks.iter().take(10) {
                        ui.label(format!("{start}  {text}"));
                    }
                    if blocks.len() > 10 {
                        ui.weak(format!("……(共 {} 条,仅预览前 10 条)", blocks.len()));
                    }
                }
            }
            None => {
                ui.label("字幕文件已不在原位置(可能被移动或删除了)");
            }
        });
}

/// 画流水线步骤条(比如"✓提取 › ●识别 › ○翻译")。`translate_enabled` 决定要不要画"翻译"
/// 这一步——没开翻译的话流水线只有两步,画第三步没有意义,只会让人以为漏了什么。
/// Pending/Skipped/Cancelled 这几种状态没有明确的"当前处于哪个阶段"可言,不画步骤条,
/// 靠下面单独一行的状态文案说明就够了。
pub(crate) fn render_stage_stepper(ui: &mut egui::Ui, status: &JobStatus, translate_enabled: bool) {
    let stages: &[job::Stage] = if translate_enabled {
        &[
            job::Stage::ExtractAudio,
            job::Stage::Transcribe,
            job::Stage::Translate,
        ]
    } else {
        &[job::Stage::ExtractAudio, job::Stage::Transcribe]
    };

    let (current_ord, is_failed, is_all_done) = match status {
        JobStatus::Running(stage, _, _) => (Some(stage.ordinal()), false, false),
        JobStatus::Failed(stage, _, _) => (Some(stage.ordinal()), true, false),
        JobStatus::Done(_) | JobStatus::DoneWithWarning(..) => (None, false, true),
        _ => return,
    };

    const DONE_COLOR: egui::Color32 = egui::Color32::from_rgb(60, 170, 60);
    const RUNNING_COLOR: egui::Color32 = egui::Color32::from_rgb(90, 150, 230);
    const FAIL_COLOR: egui::Color32 = egui::Color32::RED;
    const PENDING_COLOR: egui::Color32 = egui::Color32::GRAY;

    ui.horizontal(|ui| {
        for (i, stage) in stages.iter().enumerate() {
            let ord = stage.ordinal();
            let (glyph, color) = if is_all_done {
                ("✓", DONE_COLOR)
            } else {
                match current_ord {
                    Some(cur) if ord < cur => ("✓", DONE_COLOR),
                    Some(cur) if ord == cur && is_failed => ("✗", FAIL_COLOR),
                    Some(cur) if ord == cur => ("●", RUNNING_COLOR),
                    _ => ("○", PENDING_COLOR),
                }
            };
            ui.colored_label(color, format!("{glyph}{}", stage.label()));
            if i + 1 < stages.len() {
                ui.label("›");
            }
        }
    });
}

/// 中央任务队列:每行文件名+步骤条/状态行/单文件操作按钮/展开预览。
pub(crate) fn render_job_list(app: &mut SubgenApp, ui: &mut egui::Ui) {
        // 单文件操作(移除/重新处理/打开文件夹/展开预览)在渲染循环里只读取 app.jobs,
        // 真正的修改(可能触发 Vec::remove 导致下标错位)收集到这几个变量里,渲染循环结束
        // 后统一应用——不在遍历 app.jobs 的同时又去修改它。
        let mut remove_index: Option<usize> = None;
        let mut reprocess_index: Option<usize> = None;
        let mut open_folder_path: Option<PathBuf> = None;
        let mut toggle_preview_index: Option<usize> = None;

        egui::CentralPanel::default().show(ui, |ui| {
            if app.jobs.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("把视频文件拖到这个窗口,或点下方“选择视频文件”按钮");
                });
                return;
            }
            let translate_enabled = app.config.translate;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, job) in app.jobs.iter().enumerate() {
                    ui.vertical(|ui| {
                        // 第一行:文件名(挤不下时省略号截断,hover 能看到完整名字)+ 步骤条。
                        // 步骤条先画(right_to_left 从右边开始分配空间,把它的宽度先占住),
                        // 文件名最后画、用 truncate() 吃掉剩余空间——不管文件名或步骤条多长,
                        // 两者都不会再互相覆盖(之前的实现两边都不限宽,长文件名+长状态文案
                        // 撞在一起时界面会花掉,这里从根上改了布局顺序解决)。
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                render_stage_stepper(ui, &job.status, translate_enabled);
                                let name = job
                                    .video_path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                ui.add(egui::Label::new(name.clone()).truncate())
                                    .on_hover_text(name);
                            });
                        });
                        // 第二行:详细状态文案,单独占一行,不用再和文件名/步骤条抢空间。
                        // 失败且有对应日志文件时加一个"查看日志"按钮——技术性的报错细节
                        // (退出码、原始 stderr)留在日志文件里,状态栏只给一行人话摘要,
                        // 想深挖的用户一键用记事本打开,不用去 logs/ 文件夹里翻文件名。
                        ui.horizontal(|ui| {
                            // 正在跑的任务前面加个转圈动画——纯文字状态在长时间不变的等待期
                            // (比如提取音轨、翻译模型加载)看着和"卡死了"没有视觉区别,
                            // 一个持续在动的图形元素能立刻打消这个疑虑,不用非得等百分比更新。
                            if matches!(job.status, JobStatus::Running(..)) {
                                ui.spinner();
                            }
                            let (text, color) = job_status_line(job, app.cancelling);
                            ui.colored_label(color, text);
                            if let JobStatus::Failed(_, _, Some(log_path)) = &job.status {
                                if ui.small_button("查看日志").clicked() {
                                    let _ = std::process::Command::new("notepad")
                                        .arg(log_path)
                                        .spawn();
                                }
                            }
                        });
                        // 第三行:单文件操作按钮。处理中全部禁用——"移除"会让 Vec 下标
                        // 错位,worker 线程发回来的 JobUpdate{index, ..} 是按批次开始时
                        // 固定下来的下标算的,处理中改动列表会让后续状态更新对错行;
                        // "重新处理"/预览虽然本身不涉及下标错位,但处理中点这些于用户
                        // 而言意图也不明确(现在改会不会影响正在跑的这一批?),干脆和
                        // "移除"一起统一禁用,行为更容易预期。
                        ui.add_enabled_ui(!app.processing, |ui| {
                            ui.horizontal(|ui| {
                                if ui.small_button("移除").clicked() {
                                    remove_index = Some(i);
                                }
                                if job.status.is_terminal() {
                                    if ui.small_button("重新处理").clicked() {
                                        reprocess_index = Some(i);
                                    }
                                }
                                if ui.small_button("打开文件夹").clicked() {
                                    open_folder_path = Some(reveal_path_for(job));
                                }
                                if matches!(job.status, JobStatus::Done(_) | JobStatus::DoneWithWarning(..))
                                {
                                    let label = if job.preview_open { "收起预览" } else { "预览" };
                                    if ui.small_button(label).clicked() {
                                        toggle_preview_index = Some(i);
                                    }
                                }
                            });
                        });
                        if job.preview_open {
                            render_subtitle_preview(ui, job);
                        }
                    });
                    ui.separator();
                }
            });
        });

        if let Some(i) = remove_index {
            app.jobs.remove(i);
        }
        if let Some(i) = reprocess_index {
            if let Some(j) = app.jobs.get_mut(i) {
                j.force_reprocess = true;
            }
        }
        if let Some(path) = open_folder_path {
            let _ = std::process::Command::new("explorer")
                .arg(format!("/select,{}", path.display()))
                .spawn();
        }
        if let Some(i) = toggle_preview_index {
            if let Some(j) = app.jobs.get_mut(i) {
                j.preview_open = !j.preview_open;
            }
        }

}
