use crate::app::SubgenApp;
use crate::format::*;
use crate::paths;
use crate::setup::{self, ComponentKey};
use eframe::egui;
use std::sync::atomic::Ordering;

pub(crate) fn render_checking(ui: &mut egui::Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.centered_and_justified(|ui| ui.label("正在检查运行环境..."));
    });
}

pub(crate) fn render_needs_download(app: &mut SubgenApp, ui: &mut egui::Ui, missing: &[ComponentKey]) {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("首次使用需要下载必要组件");
                    ui.add_space(8.0);
                    ui.label("以下组件缺失,需联网下载:");
                    ui.add_space(4.0);
                    // 每项标大小、下面给总计——用户在不知情的情况下点"开始下载"其实是在
                    // 做一个"可能几十分钟、几个 GB 流量"的决定,不该让他们蒙在鼓里。
                    let mut total_bytes: u64 = 0;
                    for k in missing {
                        let size = setup::estimated_size_bytes(*k, &app.config.model);
                        total_bytes += size;
                        ui.label(format!("· {}({})", k.label(), format_download_size(size)));
                    }
                    ui.add_space(4.0);
                    ui.label(format!(
                        "共需下载约 {},只需下载这一次。",
                        format_download_size(total_bytes)
                    ));
                    // 这句话也是这个软件最大的卖点,之前只写在 README 里,首次见面就该
                    // 让用户知道:视频和字幕不会离开这台电脑。
                    ui.label("所有组件均在你自己的电脑上运行,视频和字幕不会上传到任何服务器。");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(format!("模型将保存到: {}", paths::models_dir().display()));
                        if ui.button("更改...").clicked() {
                            app.begin_change_models_dir();
                        }
                    });
                    if let Some(e) = &app.models_dir_error {
                        ui.colored_label(egui::Color32::RED, e);
                    }
                    if let Some((needed, avail)) =
                        setup::check_disk_space(&missing, &app.config.model, &paths::models_dir())
                    {
                        if avail < needed {
                            ui.colored_label(
                                egui::Color32::from_rgb(230, 150, 20),
                                format!(
                                    "⚠ 目标磁盘剩余空间可能不够:预计需要约 {:.1} GB,当前剩余约 {:.1} GB。\
                                     可以点上面的“更改...”换一个空间更充足的位置。",
                                    needed as f64 / 1_073_741_824.0,
                                    avail as f64 / 1_073_741_824.0,
                                ),
                            );
                        }
                    }
                    ui.add_space(12.0);
                    if ui.button("开始下载").clicked() {
                        app.start_downloading();
                    }
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
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("正在下载组件...");
                    ui.add_space(8.0);
                    ui.label(format!(
                        "当前: {current_label}(第 {current_index}/{total_count} 项)"
                    ));
                    let progress = if total > 0 {
                        downloaded as f32 / total as f32
                    } else {
                        0.0
                    };
                    ui.add(egui::ProgressBar::new(progress).show_percentage());
                    ui.label(format!(
                        "{:.1} MB / {:.1} MB",
                        downloaded as f64 / 1_048_576.0,
                        total as f64 / 1_048_576.0
                    ));
                    ui.add_space(8.0);
                    if ui.button("取消").clicked() {
                        app.setup_cancel.store(true, Ordering::Relaxed);
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

pub(crate) fn render_migrating(
    app: &mut SubgenApp,
    ui: &mut egui::Ui,
    current_label: String,
    current_index: usize,
    total_count: usize,
    copied: u64,
    total: u64,
) {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("正在搬迁模型文件到新目录...");
                    ui.add_space(8.0);
                    if total_count > 0 {
                        ui.label(format!(
                            "当前: {current_label}(第 {current_index}/{total_count} 个)"
                        ));
                        let progress = if total > 0 {
                            copied as f32 / total as f32
                        } else {
                            0.0
                        };
                        ui.add(egui::ProgressBar::new(progress).show_percentage());
                        ui.label(format!(
                            "{:.1} MB / {:.1} MB",
                            copied as f64 / 1_048_576.0,
                            total as f64 / 1_048_576.0
                        ));
                    } else {
                        ui.label("正在准备...");
                    }
                    ui.add_space(8.0);
                    if ui.button("取消").clicked() {
                        app.migrate_cancel.store(true, Ordering::Relaxed);
                    }
                    ui.add_space(12.0);
                    ui.separator();
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

pub(crate) fn render_error(app: &mut SubgenApp, ui: &mut egui::Ui, e: String) {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("组件下载失败");
                    ui.add_space(8.0);
                    ui.colored_label(egui::Color32::RED, &e);
                    ui.add_space(12.0);
                    ui.label("请检查网络后重试,或参考 README 手动下载放置对应文件。");
                    if ui.button("重试").clicked() {
                        app.begin_setup_check();
                    }
                });
}
