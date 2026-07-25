use crate::format::model_status_line;
use crate::models::ModelStatus;
use crate::setup::{status_for, ComponentKey};
use eframe::egui;

/// 清单一行:名称、用途、状态。故意不带"选中的那个"这种概念——用户要看的是"这几个模型
/// 各自处于什么状态",不是当前配置选了哪个,那是设置窗口的职责。
struct Row {
    label: String,
    purpose: &'static str,
    status: ModelStatus,
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
            });
        }
    }
    if let Some(status) = status_for(ComponentKey::Vad, "") {
        rows.push(Row {
            label: "语音活动检测(VAD)".to_string(),
            purpose: "语音识别(辅助)",
            status,
        });
    }
    for tm in crate::translate::MODELS {
        if let Some(status) = status_for(ComponentKey::TransModel(tm.key), "") {
            rows.push(Row { label: tm.label.to_string(), purpose: "翻译", status });
        }
    }
    rows
}

/// 清单主体:一个模型一行,名称/用途/状态/操作。`render_manifest_body` 不自己开窗口——
/// 同一份渲染逻辑被 `components.rs` 的浮窗和首次运行整屏引导页两个宿主复用(见项目计划
/// B6),避免"逐个确认"和"首次批量下载"分裂成两套代码。
pub(crate) fn render_manifest_body(ui: &mut egui::Ui) {
    let rows = collect_rows();
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

                    for row in &rows {
                        ui.label(&row.label);
                        ui.label(row.purpose);
                        let (text, color) = model_status_line(&row.status);
                        ui.colored_label(color, text);
                        ui.horizontal(|ui| {
                            let path = row.status.path();
                            if ui
                                .add_enabled(path.is_some(), egui::Button::new("在文件夹中显示"))
                                .clicked()
                                && let Some(p) = path
                            {
                                let _ = std::process::Command::new("explorer")
                                    .arg(format!("/select,{}", p.display()))
                                    .spawn();
                            }
                            if ui.add_enabled(path.is_some(), egui::Button::new("复制路径")).clicked()
                                && let Some(p) = path
                            {
                                ui.ctx().copy_text(p.display().to_string());
                            }
                        });
                        ui.end_row();
                    }
                });
        });
}
