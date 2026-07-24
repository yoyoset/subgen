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

/// 右侧详情区(从):`app.selected` 有值就钉住显示那一行;否则自动跟随正在处理的文件
/// (`active_job_index`)——单 GPU 串行处理,任何时刻至多一个文件在跑,这是天然的默认
/// 关注对象。既没有钉住的选择、也没有任务在跑时,给一个空状态提示而不是空白一片。
pub(crate) fn render_detail_pane(app: &mut SubgenApp, ui: &mut egui::Ui) {
    let active = active_job_index(&app.jobs);
    let target = app.selected.or(active);

    let Some(i) = target.filter(|&i| i < app.jobs.len()) else {
        ui.centered_and_justified(|ui| {
            if app.jobs.is_empty() {
                ui.weak("把视频文件拖到窗口里,或点左侧“选择视频文件”按钮开始");
            } else {
                ui.weak("点左侧一个文件查看详情");
            }
        });
        return;
    };

    // 单文件操作会改动 app.jobs(移除)或需要 &mut app(重新处理/切回自动跟随),渲染只读
    // job.status 期间先收集意图,渲染结束后再统一应用——避免同时持有 job 的不可变借用
    // 和对 app 的可变操作。
    let mut remove_clicked = false;
    let mut reprocess_clicked = false;
    let mut open_folder_path: Option<PathBuf> = None;
    let mut toggle_preview = false;
    let mut jump_to_active = false;

    {
        let job = &app.jobs[i];
        let is_pinned = app.selected == Some(i);

        ui.horizontal(|ui| {
            let name = job
                .video_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            ui.heading(name);
            // 钉住了别的文件、但批次还在跑另一个时,给一条能一键跳回去的路——不然用户
            // 查完一个失败文件后,还得自己去左边找回正在跑的那一行。
            if is_pinned && active.is_some() && active != Some(i)
                && ui.small_button("▸ 回到正在处理的文件").clicked() {
                    jump_to_active = true;
                }
        });
        ui.add_space(4.0);

        render_stage_stepper(ui, &job.status, app.config.translate);
        ui.add_space(2.0);

        ui.horizontal(|ui| {
            if matches!(job.status, JobStatus::Running(..)) {
                ui.spinner();
            }
            let (text, color) = job_status_line(job, app.cancelling);
            ui.colored_label(color, text);
        });
        if let JobStatus::Failed(_, _, Some(log_path)) = &job.status
            && ui.small_button("查看日志").clicked() {
                let _ = std::process::Command::new("notepad").arg(log_path).spawn();
            }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        ui.add_enabled_ui(!app.processing, |ui| {
            ui.horizontal(|ui| {
                if ui.button("移除").clicked() {
                    remove_clicked = true;
                }
                if job.status.is_terminal() && ui.button("重新处理").clicked() {
                    reprocess_clicked = true;
                }
                if ui.button("打开文件夹").clicked() {
                    open_folder_path = Some(reveal_path_for(job));
                }
                if matches!(job.status, JobStatus::Done(_) | JobStatus::DoneWithWarning(..)) {
                    let label = if job.preview_open { "收起预览" } else { "预览" };
                    if ui.button(label).clicked() {
                        toggle_preview = true;
                    }
                }
            });
        });

        if job.preview_open {
            ui.add_space(6.0);
            render_subtitle_preview(ui, job);
        }
    }

    if jump_to_active {
        app.selected = None;
    }
    if remove_clicked {
        app.jobs.remove(i);
        app.selected = reconcile_selection(app.selected, i, app.jobs.len());
    } else {
        if reprocess_clicked
            && let Some(j) = app.jobs.get_mut(i) {
                j.force_reprocess = true;
            }
        if toggle_preview
            && let Some(j) = app.jobs.get_mut(i) {
                j.preview_open = !j.preview_open;
            }
    }
    if let Some(path) = open_folder_path {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }
}
