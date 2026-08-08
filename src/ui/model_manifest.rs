use crate::app::SubgenApp;
use crate::format::{format_download_size, model_status_line};
use crate::models::{FileKind, ModelStatus};
use crate::setup::{status_for, ComponentKey};
use crate::ui::theme;
use eframe::egui;

/// 清单一行:名称、用途、状态,以及"定位…"/"下载"需要的全部信息。故意不带"选中的那个"
/// 这种概念——用户要看的是"这几个模型各自处于什么状态",不是当前配置选了哪个,那是设置
/// 窗口的职责。
struct Row {
    label: String,
    purpose: &'static str,
    status: ModelStatus,
    binding_key: String,
    component_key: ComponentKey,
    /// `install_component` 的 `model` 参数——只有 `ComponentKey::Model`(whisper)真正用到,
    /// Vad/TransModel 忽略它,统一传空字符串。
    whisper_model_arg: String,
    expected_kind: FileKind,
    expected_size: u64,
    /// 没有权威哈希可比对时是 `None`——目前三种模型都已经有哈希了(见项目计划 B1b),
    /// 但保留这个分支:没有哈希就不该提供"定位…"入口,没法验证的绑定比没绑定风险更大。
    expected_sha256: Option<&'static str>,
    extension_filter: &'static str,
}

/// 枚举所有已知模型(不止当前配置选中的那个)算出各自状态。用 `setup::status_for` 而不是
/// 重新实现一遍路径解析——那是唯一一份"给定模型 key 该怎么判定状态"的逻辑,清单只是它的
/// 第三个消费方(`is_present`/详情面板之外)。
fn collect_rows() -> Vec<Row> {
    let mut rows = Vec::new();
    for wm in crate::models::whisper::MODELS {
        if let Some(status) = status_for(ComponentKey::Model, wm.key) {
            rows.push(Row {
                label: format!("{} · {}", wm.key, wm.label),
                purpose: "语音识别",
                status,
                binding_key: crate::models::whisper_binding_key(wm.key),
                component_key: ComponentKey::Model,
                whisper_model_arg: wm.key.to_string(),
                expected_kind: FileKind::Ggml,
                expected_size: wm.size_bytes,
                expected_sha256: wm.sha256,
                extension_filter: "bin",
            });
        }
    }
    if let Some(status) = status_for(ComponentKey::Vad, "") {
        rows.push(Row {
            label: "语音活动检测(VAD)".to_string(),
            purpose: "语音识别(辅助)",
            status,
            binding_key: crate::models::VAD_BINDING_KEY.to_string(),
            component_key: ComponentKey::Vad,
            whisper_model_arg: String::new(),
            expected_kind: FileKind::Ggml,
            expected_size: crate::models::VAD_SIZE_BYTES,
            expected_sha256: Some(crate::setup::VAD_SHA256),
            extension_filter: "bin",
        });
    }
    for tm in crate::translate::MODELS {
        if let Some(status) = status_for(ComponentKey::TransModel(tm.key), "") {
            rows.push(Row {
                label: tm.label.to_string(),
                purpose: "翻译",
                status,
                binding_key: crate::models::trans_binding_key(tm.key),
                component_key: ComponentKey::TransModel(tm.key),
                whisper_model_arg: String::new(),
                expected_kind: FileKind::Gguf,
                expected_size: tm.size_bytes,
                expected_sha256: Some(tm.sha256),
                extension_filter: "gguf",
            });
        }
    }
    rows
}

/// 清单主体:一个模型一行,名称/用途/状态/操作。`render_manifest_body` 不自己开窗口——
/// 同一份渲染逻辑被 `components.rs` 的浮窗和首次运行整屏引导页两个宿主复用(见项目计划
/// B6),避免"逐个确认"和"首次批量下载"分裂成两套代码。
pub(crate) fn render_manifest_body(app: &mut SubgenApp, ui: &mut egui::Ui) {
    let rows = collect_rows();
    let pending_verify_key = app.pending_verify.as_ref().map(|pv| pv.binding_key.clone());
    let installing_key = app.inline_install.as_ref().map(|i| i.key);
    let install_progress = app.inline_install.as_ref().map(|i| (i.downloaded, i.total));
    let cleaning_key = app.pending_cleanup.as_ref().map(|pc| pc.binding_key.clone());
    // 处理中(批次在跑)不允许改动存放目录里的文件——llama-server/whisper-cli 可能正
    // 持有着这些文件,这时候写入是自找麻烦。同一时刻也只允许一项定位/下载/清理在跑,
    // 避免多个后台线程同时写 config.model_files 或同时挪动文件相互干扰。
    let actions_enabled = !app.processing
        && app.pending_verify.is_none()
        && app.inline_install.is_none()
        && app.pending_cleanup.is_none();
    // 扫描进行中时,dup_results 还是上一次扫描的结果——"清理重复"要按结果挪文件,
    // 拿过期结果动手会把文件搬错地方,所以扫描期间禁用清理(定位/下载不挪已有文件,不受影响)。
    let scanning = app.scan_rx.is_some();

    let mut locate_clicked: Option<usize> = None;
    let mut download_clicked: Option<usize> = None;
    let mut use_candidate_clicked: Option<usize> = None;
    let mut cleanup_clicked: Option<usize> = None;
    let mut cancel_verify_clicked = false;
    let mut cancel_install_clicked = false;
    let mut cancel_cleanup_clicked = false;

    ui.horizontal(|ui| {
        if ui.add_enabled(app.scan_rx.is_none(), egui::Button::new("扫描本机已有模型")).clicked() {
            app.begin_scan_for_models();
        }
        if app.scan_rx.is_some() {
            ui.spinner();
            ui.weak("正在扫描…");
        } else if let Some(found) = &app.scan_results {
            ui.weak(format!("上次扫描发现 {} 个候选", found.len()));
        }
    });
    ui.add_space(4.0);

    egui::ScrollArea::vertical()
        .max_height(320.0)
        .id_salt("model_manifest_scroll")
        .show(ui, |ui| {
            egui::Grid::new("model_manifest_grid")
                .num_columns(4)
                .striped(true)
                .spacing([16.0, 6.0])
                .show(ui, |ui| {
                    ui.strong("模型");
                    ui.strong("用途");
                    ui.strong("状态");
                    ui.strong("操作");
                    ui.end_row();

                    for (i, row) in rows.iter().enumerate() {
                        ui.label(&row.label);
                        ui.label(row.purpose);

                        let is_verifying = pending_verify_key.as_deref() == Some(row.binding_key.as_str());
                        let is_installing = installing_key == Some(row.component_key);
                        let is_cleaning = cleaning_key.as_deref() == Some(row.binding_key.as_str());
                        let busy = is_verifying || is_installing || is_cleaning;
                        // 只在这一行还没就绪、且不是正在忙的时候才提示扫描候选——已经 Ready
                        // 了就不需要再建议一个候选(哪怕碰巧发现了别的同名文件也不该干扰)。
                        let candidate = if !row.status.is_ready() && !busy {
                            app.scan_results.as_ref().and_then(|m| m.get(&row.binding_key))
                        } else {
                            None
                        };
                        // 重复文件和"是不是 Ready"无关——恰恰是 Ready 的那份之外,还多出来
                        // 的副本才是清理的对象。
                        let duplicates: &[std::path::PathBuf] = if !busy {
                            app.dup_results.as_ref().and_then(|m| m.get(&row.binding_key)).map(|v| v.as_slice()).unwrap_or(&[])
                        } else {
                            &[]
                        };

                        if is_verifying {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.weak("正在校验…");
                            });
                        } else if is_installing {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                match install_progress {
                                    Some((d, t)) if t > 0 => ui.weak(format!(
                                        "正在安装… {} / {}",
                                        format_download_size(d),
                                        format_download_size(t)
                                    )),
                                    _ => ui.weak("正在安装…"),
                                }
                            });
                        } else if is_cleaning {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.weak("正在清理重复文件…");
                            });
                        } else {
                            ui.vertical(|ui| {
                                let (text, color) = model_status_line(&row.status);
                                ui.colored_label(color, text);
                                if let Some(p) = candidate {
                                    ui.weak(format!("发现候选: {}", p.display()));
                                }
                                if !duplicates.is_empty() {
                                    let total: u64 = duplicates
                                        .iter()
                                        .filter_map(|p| std::fs::metadata(p).ok())
                                        .map(|m| m.len())
                                        .sum();
                                    ui.colored_label(
                                        theme::WARN,
                                        format!(
                                            "发现 {} 份重复副本(共 {}),占了双倍磁盘空间",
                                            duplicates.len(),
                                            format_download_size(total)
                                        ),
                                    );
                                }
                            });
                        }

                        ui.horizontal(|ui| {
                            let path = row.status.path();
                            if ui
                                .add_enabled(path.is_some() && !busy, egui::Button::new("在文件夹中显示"))
                                .clicked()
                                && let Some(p) = path
                            {
                                let _ = std::process::Command::new("explorer")
                                    .arg(format!("/select,{}", p.display()))
                                    .spawn();
                            }
                            if ui
                                .add_enabled(path.is_some() && !busy, egui::Button::new("复制路径"))
                                .clicked()
                                && let Some(p) = path
                            {
                                ui.ctx().copy_text(p.display().to_string());
                            }
                            if is_verifying {
                                if ui.button("取消").clicked() {
                                    cancel_verify_clicked = true;
                                }
                            } else if is_installing {
                                if ui.button("取消").clicked() {
                                    cancel_install_clicked = true;
                                }
                            } else if is_cleaning {
                                if ui.button("取消").clicked() {
                                    cancel_cleanup_clicked = true;
                                }
                            } else {
                                if candidate.is_some()
                                    && row.expected_sha256.is_some()
                                    && ui.add_enabled(actions_enabled, egui::Button::new("使用此文件")).clicked()
                                {
                                    use_candidate_clicked = Some(i);
                                }
                                if row.expected_sha256.is_some()
                                    && ui.add_enabled(actions_enabled, egui::Button::new("定位…")).clicked()
                                {
                                    locate_clicked = Some(i);
                                }
                                if !row.status.is_ready()
                                    && ui.add_enabled(actions_enabled, egui::Button::new("下载")).clicked()
                                {
                                    download_clicked = Some(i);
                                }
                                if !duplicates.is_empty()
                                    && row.expected_sha256.is_some()
                                    && ui
                                        .add_enabled(
                                            actions_enabled && !scanning,
                                            egui::Button::new("清理重复"),
                                        )
                                        .clicked()
                                {
                                    cleanup_clicked = Some(i);
                                }
                            }
                        });
                        ui.end_row();
                    }
                });
        });

    if cancel_verify_clicked
        && let Some(pv) = &app.pending_verify
    {
        pv.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if cancel_install_clicked
        && let Some(inst) = &app.inline_install
    {
        inst.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if cancel_cleanup_clicked
        && let Some(pc) = &app.pending_cleanup
    {
        pc.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(i) = locate_clicked {
        let row = &rows[i];
        // expected_sha256 在按钮可点击的前提下必然是 Some(见上面的启用条件),这里
        // unwrap 是安全的——但仍然用 if let 而不是裸 unwrap,防止以后谁动了启用条件
        // 却忘了同步这里,panic 总比"悄悄绑定一个没校验过的文件"更容易被发现。
        if let Some(sha256) = row.expected_sha256 {
            app.begin_locate_model(
                row.binding_key.clone(),
                row.label.clone(),
                row.expected_kind,
                row.expected_size,
                sha256,
                row.extension_filter,
            );
        }
    }
    if let Some(i) = download_clicked {
        let row = &rows[i];
        app.begin_inline_install(row.component_key, row.label.clone(), row.whisper_model_arg.clone());
    }
    if let Some(i) = use_candidate_clicked {
        let row = &rows[i];
        // 重新从 scan_results 里取路径,而不是在渲染循环里就把它存下来——避免克隆一份
        // Option<&PathBuf> 到闭包外的生命周期麻烦,反正这里再查一次表是 O(1) 的开销。
        let found = app.scan_results.as_ref().and_then(|m| m.get(&row.binding_key).cloned());
        if let (Some(path), Some(sha256)) = (found, row.expected_sha256) {
            app.begin_verify_candidate(
                row.binding_key.clone(),
                row.label.clone(),
                path,
                row.expected_kind,
                row.expected_size,
                sha256,
            );
        }
    }
    if let Some(i) = cleanup_clicked {
        let row = &rows[i];
        // 重新从 dup_results 里取路径,原因同 use_candidate_clicked。
        let found = app.dup_results.as_ref().and_then(|m| m.get(&row.binding_key).cloned());
        if let (Some(paths), Some(sha256)) = (found, row.expected_sha256)
            && !paths.is_empty()
        {
            app.begin_cleanup_duplicates(row.binding_key.clone(), row.label.clone(), paths, sha256);
        }
    }
}
