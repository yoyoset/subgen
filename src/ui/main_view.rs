use crate::app::{SetupPhase, SubgenApp};
use crate::config::Config;
use crate::engine;
use crate::format::*;
use crate::job::{self, Job, JobStatus};
use crate::paths;
use crate::translate;
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

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

impl SubgenApp {
    pub fn show_main_ui(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("controls").show(ui, |ui| {
            ui.add_space(6.0);
            // 处理中这一整块设置(模型/语言/覆盖策略/翻译开关及其子选项)全部禁用——之前
            // 界面上看着能改,改了也确实存进了 config,但当前这一批任务完全不会用上新设置,
            // 用户会以为"改了=生效了",跑完发现结果和界面显示的设置对不上,是很隐蔽的
            // "设置是摆设"体验。禁用后 hover 会有一层浅灰遮罩,直观地表明"现在不能碰"。
            ui.add_enabled_ui(!self.processing, |ui| {
            ui.horizontal(|ui| {
                ui.label("模型:");
                let mut model_changed = false;
                egui::ComboBox::from_id_salt("model")
                    .selected_text(self.config.model.clone())
                    .show_ui(ui, |ui| {
                        for (key, desc) in Config::AVAILABLE_MODELS {
                            let selected = self.config.model == *key;
                            if ui
                                .selectable_label(selected, format!("{key} - {desc}"))
                                .clicked()
                                && !selected
                            {
                                self.config.model = key.to_string();
                                model_changed = true;
                            }
                        }
                    });
                if model_changed {
                    self.config.save();
                    self.setup_phase = SetupPhase::Checking;
                }

                ui.add_space(16.0);
                ui.label("语言:");
                let mut lang_changed = false;
                // 下拉框显示中文名(lang_display,和完成状态行回显用的是同一个函数,保持
                // 界面上语言名称的说法前后一致),存进 config 的还是 ISO 码,不影响存储格式。
                egui::ComboBox::from_id_salt("lang")
                    .selected_text(lang_display(&self.config.language))
                    .show_ui(ui, |ui| {
                        for lang in ["auto", "en", "zh", "ja", "ko", "fr", "de", "es"] {
                            if ui
                                .selectable_label(self.config.language == lang, lang_display(lang))
                                .clicked()
                            {
                                self.config.language = lang.to_string();
                                lang_changed = true;
                            }
                        }
                    });
                if lang_changed {
                    self.config.save();
                }

                ui.add_space(16.0);
                let mut overwrite = self.config.overwrite_existing;
                if ui.checkbox(&mut overwrite, "已有同名字幕时覆盖").changed() {
                    self.config.overwrite_existing = overwrite;
                    self.config.save();
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // 开关/模型切换需要重新检测组件是否已下载(可能要装 llama.cpp + 模型文件),
                // 双语/仅中文纯粹是输出格式,不影响要下载什么,所以单独存 save 不触发重新检测。
                let mut needs_recheck = false;
                let mut translate = self.config.translate;
                // "本地模型"是给开发者看的实现细节;新手真正关心的是"要不要联网、内容
                // 会不会被传出去",直接把这个卖点说出来比暗示"用了本地模型"更直接。
                if ui.checkbox(&mut translate, "翻译成中文(在本机运行,不上传内容)").changed() {
                    self.config.translate = translate;
                    needs_recheck = true;
                }

                if self.config.translate {
                    ui.add_space(16.0);
                    ui.label("翻译模型:");
                    let current_label = if self.config.translate_model == "auto" {
                        "自动(按视频语言选择)".to_string()
                    } else {
                        translate::model_by_key(&self.config.translate_model)
                            .map(|m| m.label.to_string())
                            .unwrap_or_else(|| self.config.translate_model.clone())
                    };
                    egui::ComboBox::from_id_salt("translate_model")
                        .selected_text(current_label)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(
                                    self.config.translate_model == "auto",
                                    "自动(按视频语言选择)",
                                )
                                .clicked()
                            {
                                self.config.translate_model = "auto".to_string();
                                needs_recheck = true;
                            }
                            for m in translate::MODELS {
                                if ui
                                    .selectable_label(self.config.translate_model == m.key, m.label)
                                    .clicked()
                                {
                                    self.config.translate_model = m.key.to_string();
                                    needs_recheck = true;
                                }
                            }
                        });

                    ui.add_space(16.0);
                    let mut bilingual = self.config.bilingual;
                    if ui.checkbox(&mut bilingual, "双语字幕(中文+原文)").changed() {
                        self.config.bilingual = bilingual;
                        self.config.save();
                    }
                }

                if needs_recheck {
                    self.config.save();
                    self.setup_phase = SetupPhase::Checking;
                }
            });
            }); // add_enabled_ui(!self.processing) 到此结束
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // 常驻显示 + 可改,不止首次下载那一次机会——用了一段时间之后想搬家也应该
                // 随时能改,不应该被困在当初选的位置。处理中禁用:一批任务跑到一半时,
                // 后面文件的翻译阶段会用当时刚查到的模型路径去启动 llama-server,这时候
                // 换目录容易踩"模型文件还没搬过去"的空。
                ui.label(format!("模型位置: {}", paths::models_dir().display()));
                if ui
                    .add_enabled(!self.processing, egui::Button::new("更改..."))
                    .clicked()
                {
                    self.begin_change_models_dir();
                }
                if let Some(e) = &self.models_dir_error {
                    ui.colored_label(egui::Color32::RED, e);
                }
            });
            ui.add_space(6.0);
        });

        egui::Panel::bottom("actions").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("选择视频文件...").clicked() {
                    if let Some(files) = rfd::FileDialog::new()
                        .add_filter("视频", Self::VIDEO_EXTENSIONS)
                        .set_title("选择要生成字幕的视频文件")
                        .pick_files()
                    {
                        self.add_files(files);
                    }
                }
                // 处理中直接置灰禁用,而不是"按钮外观正常、点了却静默无效"——那种写法
                // 会让用户以为按钮坏了,反复点击也得不到任何反馈。
                if ui
                    .add_enabled(!self.processing, egui::Button::new("清空列表"))
                    .clicked()
                {
                    self.jobs.clear();
                }
                ui.add_space(16.0);

                let can_start = !self.jobs.is_empty() && !self.processing;
                if ui
                    .add_enabled(can_start, egui::Button::new("开始处理"))
                    .clicked()
                {
                    self.start_processing();
                }
                // 取消按钮点击后立即变灰、文案改成"正在取消…"——worker 线程要等当前这一步
                // (比如正在跑的 whisper-cli 子进程)结束才会真正检查取消标志,这中间有肉眼
                // 可见的延迟。不给即时反馈的话,用户会以为点击没生效,进而连点甚至直接关窗。
                let cancel_label = if self.cancelling { "正在取消…" } else { "取消" };
                if ui
                    .add_enabled(
                        self.processing && !self.cancelling,
                        egui::Button::new(cancel_label),
                    )
                    .clicked()
                {
                    self.worker_cancel.store(true, Ordering::Relaxed);
                    self.cancelling = true;
                }
            });
            if let Some(summary) = &self.last_summary {
                ui.label(summary);
            }
            if !self.gpu_warning_stages.is_empty() {
                // 按流水线顺序(识别在前、翻译在后)拼出受影响的阶段名,而不是笼统一句
                // "没用上 GPU"——识别和翻译是分别独立检测的,用户应该知道具体是哪一步慢。
                // "CUDA 环境"对不知道自己有没有独立显卡的用户是天书,给出的建议要么可执行
                // (更新驱动),要么直接告诉他们"你可能就是没有这个硬件",别让人对着一句
                // 看不懂的话不知道下一步该做什么。
                let mut stages: Vec<job::Stage> = self.gpu_warning_stages.iter().copied().collect();
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
            if let Some((text, _)) = &self.transient_notice {
                ui.colored_label(egui::Color32::from_rgb(230, 150, 20), text);
            }
            ui.add_space(6.0);
        });

        // 单文件操作(移除/重新处理/打开文件夹/展开预览)在渲染循环里只读取 self.jobs,
        // 真正的修改(可能触发 Vec::remove 导致下标错位)收集到这几个变量里,渲染循环结束
        // 后统一应用——不在遍历 self.jobs 的同时又去修改它。
        let mut remove_index: Option<usize> = None;
        let mut reprocess_index: Option<usize> = None;
        let mut open_folder_path: Option<PathBuf> = None;
        let mut toggle_preview_index: Option<usize> = None;

        egui::CentralPanel::default().show(ui, |ui| {
            if self.jobs.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("把视频文件拖到这个窗口,或点下方“选择视频文件”按钮");
                });
                return;
            }
            let translate_enabled = self.config.translate;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, job) in self.jobs.iter().enumerate() {
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
                            let (text, color) = job_status_line(job, self.cancelling);
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
                        ui.add_enabled_ui(!self.processing, |ui| {
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
            self.jobs.remove(i);
        }
        if let Some(i) = reprocess_index {
            if let Some(j) = self.jobs.get_mut(i) {
                j.force_reprocess = true;
            }
        }
        if let Some(path) = open_folder_path {
            let _ = std::process::Command::new("explorer")
                .arg(format!("/select,{}", path.display()))
                .spawn();
        }
        if let Some(i) = toggle_preview_index {
            if let Some(j) = self.jobs.get_mut(i) {
                j.preview_open = !j.preview_open;
            }
        }
    }
}
