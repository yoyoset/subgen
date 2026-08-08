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

    const DONE_COLOR: egui::Color32 = crate::ui::theme::SUCCESS;
    const RUNNING_COLOR: egui::Color32 = crate::ui::theme::ACCENT_LIGHT;
    const FAIL_COLOR: egui::Color32 = crate::ui::theme::ERROR;
    const PENDING_COLOR: egui::Color32 = crate::ui::theme::NEUTRAL;

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
        if app.jobs.is_empty() {
            render_empty_detail(app, ui);
        } else {
            ui.centered_and_justified(|ui| {
                ui.colored_label(
                    crate::ui::theme::TEXT_SECONDARY,
                    "点左侧一个文件查看详情",
                );
            });
        }
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
    let mut generate_report_clicked = false;

    {
        let job = &app.jobs[i];
        let is_pinned = app.selected == Some(i);

        ui.horizontal(|ui| {
            let name = job
                .video_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // heading 用可截断的 Label 而不是 ui.heading():长文件名(真实剧集名经常几十
            // 字符)会撑爆整行,把右边的按钮挤出屏幕外;截断 + hover 看完整名。
            ui.add(
                egui::Label::new(egui::RichText::new(&name).heading()).truncate(),
            )
            .on_hover_text(&name);
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

        ui.horizontal_wrapped(|ui| {
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
            // 5 个按钮一行在窄窗口放不下,换行而不是溢出裁掉。
            ui.horizontal_wrapped(|ui| {
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
                    if app.config.translate && ui.button("生成复核报告").clicked() {
                        generate_report_clicked = true;
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
        // review.job_index 也是指向 app.jobs 的下标,和 selected 一样会被 remove 打乱
        // (被移除项之后的下标全部要减一,指向被移除项本身的要清空)——不处理的话报告
        // 面板下次渲染时可能悄悄挂在错误的任务上。
        if let Some(job_index) = app.review.as_ref().map(|r| r.job_index) {
            match reconcile_selection(Some(job_index), i, app.jobs.len()) {
                Some(new_idx) => {
                    if let Some(r) = app.review.as_mut() {
                        r.job_index = new_idx;
                    }
                }
                None => app.review = None,
            }
        }
    } else {
        if reprocess_clicked
            && let Some(j) = app.jobs.get_mut(i) {
                j.force_reprocess = true;
                // "重新处理"只登记为"下一批包含它并强制重跑",点击后当前行外观不变——
                // 不给一句反馈的话,用户会以为点了没反应。批次还没开始,所以这里直接说清
                // 它会在下一次"开始处理"时生效。
                app.transient_notice = Some((
                    "已标记,下次点“开始处理”时会重新处理该文件".to_string(),
                    std::time::Instant::now(),
                ));
            }
        if toggle_preview
            && let Some(j) = app.jobs.get_mut(i) {
                j.preview_open = !j.preview_open;
            }
        if generate_report_clicked {
            app.generate_review_report(i);
        }
    }
    if let Some(path) = open_folder_path {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }
    if !remove_clicked {
        render_review_section(app, ui, i);
    }
}

/// 空状态的引导卡:首次打开程序时右侧详情区是一片空白,用户不知道下一步该做什么——
/// 这里给一个明确的操作入口(选择视频文件)和一句最重要的说明(本地运行、不上传)。
fn render_empty_detail(app: &mut SubgenApp, ui: &mut egui::Ui) {
    let mut pick_requested = false;
    ui.centered_and_justified(|ui| {
        egui::Frame::default()
            .inner_margin(egui::Margin::symmetric(24, 20))
            .corner_radius(egui::CornerRadius::same(8))
            .fill(ui.visuals().faint_bg_color)
            .show(ui, |ui| {
                ui.set_max_width(360.0);
                ui.heading("给视频生成字幕");
                ui.add_space(6.0);
                ui.label("把视频文件拖进窗口,或点下面的按钮选择文件。");
                ui.label("可以一次添加多个文件,程序会逐个处理。");
                ui.add_space(14.0);
                if ui.add(crate::ui::theme::primary_button("选择视频文件...")).clicked() {
                    pick_requested = true;
                }
                ui.add_space(14.0);
                ui.colored_label(
                    crate::ui::theme::TEXT_SECONDARY,
                    "所有处理都在本机完成,视频和字幕不会上传到任何服务器。",
                );
            });
    });
    if pick_requested {
        let mut dialog = rfd::FileDialog::new()
            .add_filter("视频", SubgenApp::VIDEO_EXTENSIONS)
            .set_title("选择要生成字幕的视频文件");
        if let Some(dir) = &app.config.last_dir {
            dialog = dialog.set_directory(dir);
        }
        if let Some(files) = dialog.pick_files() {
            let (added, _ignored) = app.add_files(files);
            if added == 0 {
                // 和左侧队列面板的"选择视频文件..."保持同一句反馈:全都没加进去时
                // (选中非视频/都已在列表)不弹一句,用户会以为操作没成功、反复重试。
                app.transient_notice = Some((
                    "没有添加任务:选中的文件里没有可识别的视频,或都在列表中了".to_string(),
                    std::time::Instant::now(),
                ));
            }
        }
    }
}

/// 翻译复核报告区:展示上一次为 `job_index` 生成的报告(见 `SubgenApp::generate_review_report`),
/// 提供复制到剪贴板、可框选查看的文本区,以及粘贴修正回灌的入口。`app.review` 为 None 或
/// 属于别的任务时不渲染任何东西——避免切换到别的文件时还残留上一个任务的报告。
fn render_review_section(app: &mut SubgenApp, ui: &mut egui::Ui, job_index: usize) {
    let (text, saved_path, suspect_count, correction_result) = match &app.review {
        Some(r) if r.job_index == job_index => (
            r.text.clone(),
            r.saved_path.clone(),
            r.suspect_indices.len(),
            r.correction_result.clone(),
        ),
        _ => return,
    };

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);
    ui.heading("翻译复核报告");
    ui.label(format!(
        "标记 {suspect_count} 处疑似问题,已保存到 {}",
        saved_path.display()
    ));

    let mut copy_clicked = false;
    let mut regenerate_clicked = false;
    ui.horizontal(|ui| {
        if ui.button("复制到剪贴板").clicked() {
            copy_clicked = true;
        }
        if ui.button("重新生成").clicked() {
            regenerate_clicked = true;
        }
    });
    if copy_clicked {
        ui.ctx().copy_text(text.clone());
    }

    egui::ScrollArea::vertical()
        .max_height(240.0)
        .id_salt("review_report_scroll")
        .show(ui, |ui| {
            ui.add(egui::Label::new(&text).selectable(true));
        });

    if suspect_count > 0 {
        ui.add_space(6.0);
        ui.label("粘贴修正(格式:“编号. 修正后的译文”,每行一条,只接受上面标记过的编号):");
        if let Some(review) = app.review.as_mut() {
            ui.add(
                egui::TextEdit::multiline(&mut review.correction_input)
                    .desired_rows(4)
                    .hint_text("129. 修正后的译文"),
            );
        }
        let mut apply_clicked = false;
        ui.horizontal(|ui| {
            if ui.button("应用修正").clicked() {
                apply_clicked = true;
            }
            if let Some(msg) = &correction_result {
                ui.weak(msg);
            }
        });
        if apply_clicked {
            app.apply_review_corrections(job_index);
        }
    }

    if regenerate_clicked {
        app.generate_review_report(job_index);
    }
}
