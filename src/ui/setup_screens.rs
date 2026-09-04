use crate::app::SubgenApp;
use crate::format::*;
use crate::paths;
use crate::setup::{self, ComponentKey};
use crate::ui::theme;
use eframe::egui;
use std::sync::atomic::Ordering;

/// 启动时配置恢复的说明(配置文件损坏被重置)。用可关闭的横幅而不是 transient_notice
/// ——transient_notice 几秒后自动消失,这种"你的设置被重置了"的信息值得一直显示到用户
/// 明确看过。配置损坏是罕见的紧急情况,正常用户永远不会看到这条横幅。
///
/// 从 batch_bar 里抽出来复用:损坏配置的重灾场景恰恰是"models_dir_override 被重置回默认、
/// 组件看起来全缺"——用户会落在首次下载页(SetupPhase::NeedsDownload)而不是主界面,如果
/// 只在主界面 batch_bar 渲染,这个最需要解释"为什么又要我下载十几 GB"的时刻反而看不到
/// 横幅。所以 render_phase 每个页面顶部和主界面 batch_bar 都要画。
/// 画配置恢复横幅本身。返回是否点了"知道了"——由调用方决定怎么关掉它(render_phase 统一
/// 处理),避免"谁调用谁负责关"的职责散落。
pub(crate) fn render_startup_warning(app: &SubgenApp, ui: &mut egui::Ui) -> bool {
    let mut dismiss = false;
    if let Some(warning) = &app.startup_warning {
        egui::Frame::default()
            .inner_margin(egui::Margin::symmetric(8, 6))
            .fill(theme::WARN.gamma_multiply(0.15))
            .corner_radius(egui::CornerRadius::same(4))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(theme::WARN, warning);
                    if ui.small_button("知道了").clicked() {
                        dismiss = true;
                    }
                });
            });
    }
    dismiss
}

/// 按当前 `SetupPhase` 分发到对应页面。从 `eframe::App::ui` 抽出来,让 egui 生命周期方法
/// 只负责轮询和全局交互,页面分发和页面本身同住一个模块(改页面结构不用动 app.rs)。
pub(crate) fn render_phase(app: &mut SubgenApp, ui: &mut egui::Ui) {
    // 配置损坏横幅统一在这里渲染:用一个顶部面板包住,保证它永远出现在"每个阶段的内容面板"
    // 之上、不会被 CentralPanel/左侧面板盖住。批次条(Ready 阶段)不再重复画同一行横幅。
    let mut banner_dismissed = false;
    if app.startup_warning.is_some() {
        egui::Panel::top("startup_warning_banner").show(ui, |ui| {
            banner_dismissed = render_startup_warning(app, ui);
        });
    }
    if banner_dismissed {
        app.startup_warning = None;
    }
    use crate::app::SetupPhase;
    match &app.setup_phase {
        SetupPhase::Checking => render_checking(ui),
        SetupPhase::NeedsDownload(missing) => {
            let missing = missing.clone();
            render_needs_download(app, ui, &missing);
        }
        SetupPhase::Downloading {
            current_label,
            current_index,
            total_count,
            downloaded,
            total,
            ..
        } => {
            let current_label = current_label.clone();
            let current_index = *current_index;
            let total_count = *total_count;
            let downloaded = *downloaded;
            let total = *total;
            render_downloading(
                app,
                ui,
                current_label,
                current_index,
                total_count,
                downloaded,
                total,
            );
        }
        SetupPhase::Migrating {
            current_label,
            current_index,
            total_count,
            copied,
            total,
        } => {
            let current_label = current_label.clone();
            let current_index = *current_index;
            let total_count = *total_count;
            let copied = *copied;
            let total = *total;
            render_migrating(
                app,
                ui,
                current_label,
                current_index,
                total_count,
                copied,
                total,
            );
        }
        SetupPhase::Error(e) => {
            let e = e.clone();
            render_error(app, ui, e);
        }
        SetupPhase::Ready => app.show_main_ui(ui),
    }
}

/// 首次运行/下载/搬迁/错误页统一的外框:居中一张卡片,标题 + 内容 + 主操作按钮。
/// 之前四个页面各写各的裸 CentralPanel,排版细节互相不一致,看起来就像四个不相干的
/// 临时页面;这次是用户第一次见到这个软件,必须是一个观感一致的流程。
fn centered_card(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.centered_and_justified(|ui| {
            egui::Frame::default()
                .inner_margin(egui::Margin::same(18))
                .corner_radius(egui::CornerRadius::same(8))
                .fill(ui.visuals().extreme_bg_color)
                .show(ui, |ui| {
                    ui.set_max_width(480.0);
                    ui.heading(title);
                    ui.add_space(theme::SECTION_SPACING);
                    ui.separator();
                    ui.add_space(theme::SECTION_SPACING);
                    add_contents(ui)
                });
        });
    })
    .inner
}

pub(crate) fn render_checking(ui: &mut egui::Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.centered_and_justified(|ui| {
            ui.vertical(|ui| {
                ui.spinner();
                ui.add_space(8.0);
                ui.weak("正在检查运行环境...");
            });
        });
    });
}

pub(crate) fn render_needs_download(app: &mut SubgenApp, ui: &mut egui::Ui, missing: &[ComponentKey]) {
    centered_card(ui, "首次使用需要下载必要组件", |ui| {
        ui.label("以下组件缺失,需要联网下载一次。下载完成后全部组件都在本机运行:");
        ui.add_space(4.0);
        // 语音识别模型档位选择——首次下载页就能改,不用等下载完 large-v3 之后才在设置里
        // 发现还有 medium/turbo。否则配置一般的电脑被迫先下 2.9GB 的 large-v3,等发现
        // 换轻量模型还得再下一次 1.6GB,白费一次大下载。选完立刻重算缺什么、总大小也
        // 跟着变。
        ui.horizontal(|ui| {
            ui.label("语音识别模型:");
            let mut model_changed = false;
            egui::ComboBox::from_id_salt("setup_model")
                .selected_text(app.config.model.clone())
                .show_ui(ui, |ui| {
                    for wm in crate::models::whisper::MODELS {
                        let selected = app.config.model == wm.key;
                        if ui
                            .selectable_label(
                                selected,
                                format!("{} - {}", wm.key, wm.label),
                            )
                            .clicked()
                            && !selected
                        {
                            app.config.model = wm.key.to_string();
                            model_changed = true;
                        }
                    }
                });
            if model_changed {
                app.config.save();
                // 重新判定缺什么:换模型后要下载的清单/总大小/磁盘提示都变了。
                app.begin_setup_check();
            }
        });
        ui.add_space(4.0);
        // 每项标大小、下面给总计——用户在不知情的情况下点"开始下载"其实是在做一个
        // "可能几十分钟、几个 GB 流量"的决定,不该让他们蒙在鼓里。
        let mut total_bytes: u64 = 0;
        for k in missing {
            let size = setup::estimated_size_bytes(*k, &app.config.model);
            total_bytes += size;
            ui.horizontal(|ui| {
                ui.add(egui::Label::new("·"));
                ui.label(k.label());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(format_download_size(size));
                });
            });
        }
        ui.add_space(4.0);
        ui.strong(format!("共需下载约 {}", format_download_size(total_bytes)));
        ui.label("只需下载这一次。");
        ui.add_space(theme::SECTION_SPACING);
        ui.separator();
        ui.add_space(theme::SECTION_SPACING);

        // 下载前的硬件预检:这台机器有没有 NVIDIA 驱动,决定识别/翻译预计走 GPU 还是 CPU。
        // 用 OS 层面的廉价探测(见 gpu.rs,不依赖任何已下载组件),别让用户下完几 GB 才发现
        // 自己的机器只能慢速 CPU 跑。
        match crate::gpu::has_nvidia_driver() {
            Some(true) => {
                ui.horizontal(|ui| {
                    ui.colored_label(theme::SUCCESS, "✓ 检测到 NVIDIA 显卡驱动");
                    ui.weak("识别与翻译将使用 GPU 加速(速度更快)");
                });
            }
            Some(false) => {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        theme::WARN,
                        "⚠ 未检测到 NVIDIA 显卡驱动",
                    );
                    ui.weak("识别与翻译将使用 CPU 运行(较慢,耗时可能明显更长)");
                });
            }
            None => {
                ui.horizontal(|ui| {
                    ui.weak("显卡状态未知,下载完成后会自动确认 GPU/CPU 模式");
                });
            }
        }

        // "跑错了副本"的提示:有配置文件(这个目录被当作安装目录用过)却三个可执行组件全缺
        // ——真实踩过的坑(2026-09-04):用户从 build 目录(target/release)启动同一个 exe,
        // 组件其实装在 dist/ 那套完整安装里,这个副本却要求重新下载十几 GB。给个排查方向,
        // 别让用户以为组件丢了、白白重复下载。
        if setup::looks_like_bare_copy() {
            ui.horizontal(|ui| {
                ui.colored_label(
                    theme::WARN,
                    "⚠ 本目录没有组件但存在配置文件",
                );
                ui.weak(
                    "如果之前安装过,请确认运行的是完整安装目录(含 engine/ffmpeg/translator \
                     子目录)里的 subgen.exe,而不是从构建目录或单独拷出来的 exe;在这里继续 \
                     下载则会下载到当前 exe 所在目录",
                );
            });
        }

        // 这句话也是这个软件最大的卖点,之前只写在 README 里,首次见面就该让用户知道:
        // 视频和字幕不会离开这台电脑。
        ui.horizontal(|ui| {
            ui.colored_label(theme::SUCCESS, "✓ 本地运行");
            ui.weak("视频和字幕不会上传到任何服务器。");
        });

        ui.add_space(theme::SECTION_SPACING);
        ui.separator();
        ui.add_space(theme::SECTION_SPACING);

        ui.label(format!("模型将保存到: {}", paths::models_dir().display()));
        if ui.button("更改...").clicked() {
            app.begin_change_models_dir();
        }
        if let Some(e) = &app.models_dir_error {
            ui.colored_label(theme::ERROR, e);
        }
        if let Some((needed, avail)) =
            setup::check_disk_space(missing, &app.config.model, &paths::models_dir())
            && avail < needed
        {
            ui.colored_label(
                theme::WARN,
                format!(
                    "⚠ 目标磁盘剩余空间可能不够:预计需要约 {:.1} GB,当前剩余约 {:.1} GB。\
                     可以点上面的“更改...”换一个空间更充足的位置。",
                    needed as f64 / 1_073_741_824.0,
                    avail as f64 / 1_073_741_824.0,
                ),
            );
        }

        ui.add_space(12.0);
        if ui
            .add(theme::primary_button("开始下载"))
            .clicked()
        {
            app.start_downloading();
        }
    });
}

fn render_progress_screen(
    app: &mut SubgenApp,
    ui: &mut egui::Ui,
    title: &str,
    current_line: String,
    downloaded: u64,
    total: u64,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    centered_card(ui, title, |ui| {
        ui.label(current_line);
        if total > 0 {
            ui.add_space(4.0);
            let progress = downloaded as f32 / total as f32;
            ui.add(egui::ProgressBar::new(progress).show_percentage());
            // 大文件(几个 GB 的模型)显示 MB 会变成"2950.0 MB / 3095.0 MB",又长又没
            // 规模感;≥1GB 切成 GB 显示,进度条本身已经有精确百分比了,文本只要好读。
            let size_text = |bytes: u64| {
                if bytes >= 1_073_741_824 {
                    format!("{:.2} GB", bytes as f64 / 1_073_741_824.0)
                } else {
                    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
                }
            };
            ui.label(format!("{} / {}", size_text(downloaded), size_text(total)));
        }
        ui.add_space(theme::SECTION_SPACING);
        // 点击"取消"后立即变灰显示"正在取消…":下载线程可能正卡在两次重试之间的 2 秒
        // sleep 或正在解压 zip(这两处都不检查取消标志),不会马上有反应。不给即时反馈的话
        // 用户会怀疑按钮坏了连点、甚至直接关窗(和批次条"正在取消…"是同一个设计)。
        let cancel_label = if app.setup_cancelling { "正在取消…" } else { "取消" };
        if ui
            .add_enabled(!app.setup_cancelling, egui::Button::new(cancel_label))
            .clicked()
        {
            cancel.store(true, Ordering::Relaxed);
            app.setup_cancelling = true;
        }
        ui.add_space(12.0);
        ui.separator();
        // 默认收起:原始下载日志逐行滚动,信息密度低、看着比较像半成品,
        // 大多数用户根本不需要看这个;需要时(比如报障)自己展开就行。
        egui::CollapsingHeader::new("详细信息")
            .default_open(false)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &app.log_lines {
                            ui.label(line);
                        }
                    });
            });
    });
}

pub(crate) fn render_downloading(
    app: &mut SubgenApp,
    ui: &mut egui::Ui,
    current_label: String,
    current_index: usize,
    total_count: usize,
    downloaded: u64,
    total: u64,
) {
    let cancel = app.setup_cancel.clone();
    render_progress_screen(
        app,
        ui,
        "正在下载组件...",
        format!("当前: {current_label}(第 {current_index}/{total_count} 项)"),
        downloaded,
        total,
        &cancel,
    );
}

pub(crate) fn render_migrating(
    app: &mut SubgenApp,
    ui: &mut egui::Ui,
    current_label: String,
    current_index: usize,
    total_count: usize,
    copied: u64,
    total: u64,
) {
    // 搬迁和下载是两种不同的进度语义,标题/取消按钮各走各的;其余排版共用同一个进度页。
    let cancel = app.migrate_cancel.clone();
    render_progress_screen(
        app,
        ui,
        "正在搬迁模型文件到新目录...",
        if total_count > 0 {
            format!("当前: {current_label}(第 {current_index}/{total_count} 个)")
        } else {
            "正在准备...".to_string()
        },
        copied,
        total,
        &cancel,
    );
}

pub(crate) fn render_error(app: &mut SubgenApp, ui: &mut egui::Ui, e: String) {
    centered_card(ui, "组件下载失败", |ui| {
        ui.colored_label(theme::ERROR, &e);
        ui.add_space(theme::SECTION_SPACING);
        ui.label("请检查网络后重试,或参考 README 手动下载放置对应文件。");
        ui.add_space(12.0);
        if ui.button("重试").clicked() {
            app.begin_setup_check();
        }
    });
}
