use crate::app::SubgenApp;
use crate::format::model_status_line;
use crate::models::{FileKind, ModelStatus};
use crate::setup::{status_for, ComponentKey};
use eframe::egui;

/// 清单一行:名称、用途、状态,以及"定位…"需要的全部信息。故意不带"选中的那个"这种
/// 概念——用户要看的是"这几个模型各自处于什么状态",不是当前配置选了哪个,那是设置窗口
/// 的职责。
struct Row {
    label: String,
    purpose: &'static str,
    status: ModelStatus,
    binding_key: String,
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
    let pending_key = app.pending_verify.as_ref().map(|pv| pv.binding_key.clone());
    let mut locate_clicked: Option<usize> = None;
    let mut cancel_clicked = false;

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

                        let is_verifying = pending_key.as_deref() == Some(row.binding_key.as_str());
                        if is_verifying {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.weak("正在校验…");
                            });
                        } else {
                            let (text, color) = model_status_line(&row.status);
                            ui.colored_label(color, text);
                        }

                        ui.horizontal(|ui| {
                            let path = row.status.path();
                            if ui
                                .add_enabled(path.is_some() && !is_verifying, egui::Button::new("在文件夹中显示"))
                                .clicked()
                                && let Some(p) = path
                            {
                                let _ = std::process::Command::new("explorer")
                                    .arg(format!("/select,{}", p.display()))
                                    .spawn();
                            }
                            if ui
                                .add_enabled(path.is_some() && !is_verifying, egui::Button::new("复制路径"))
                                .clicked()
                                && let Some(p) = path
                            {
                                ui.ctx().copy_text(p.display().to_string());
                            }
                            if is_verifying {
                                if ui.button("取消").clicked() {
                                    cancel_clicked = true;
                                }
                            } else if row.expected_sha256.is_some()
                                && ui.button("定位…").clicked()
                            {
                                locate_clicked = Some(i);
                            }
                        });
                        ui.end_row();
                    }
                });
        });

    if cancel_clicked
        && let Some(pv) = &app.pending_verify
    {
        pv.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
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
}
