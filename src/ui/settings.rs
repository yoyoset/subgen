use crate::app::SubgenApp;
use crate::format::*;
use crate::models::whisper;
use crate::translate;
use eframe::egui;

/// "设置"是按需弹出的浮窗,不再常驻顶部——处理中这些选项对本批完全不生效,常驻只会
/// 制造"改了却没用"的困惑。处理中打开浮窗看到的是只读摘要(这批实际用的是什么),
/// 不是禁用的表单;不处理中打开才是可编辑的完整表单。
pub(crate) fn render_settings_window(app: &mut SubgenApp, ctx: &egui::Context) {
    if !app.settings_open {
        return;
    }
    let mut open = app.settings_open;
    egui::Window::new("设置")
        .open(&mut open)
        .resizable(false)
        .show(ctx, |ui| {
            if app.processing {
                render_readonly_summary(app, ui);
            } else {
                render_editable_form(app, ui);
            }
        });
    app.settings_open = open;
}

fn render_readonly_summary(app: &SubgenApp, ui: &mut egui::Ui) {
    ui.label("这批正在使用的设置(改动要等下一批才会生效):");
    ui.add_space(4.0);
    ui.label(format!("· 模型:{}", app.config.model));
    ui.label(format!("· 语言:{}", lang_display(&app.config.language)));
    if app.config.translate {
        let label = if app.config.translate_model == "auto" {
            "自动(按视频语言选择)".to_string()
        } else {
            translate::model_by_key(&app.config.translate_model)
                .map(|m| m.label.to_string())
                .unwrap_or_else(|| app.config.translate_model.clone())
        };
        ui.label(format!("· 翻译:开启 · {label}"));
    } else {
        ui.label("· 翻译:关闭");
    }
}

fn render_editable_form(app: &mut SubgenApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label("模型:");
        let mut model_changed = false;
        egui::ComboBox::from_id_salt("model")
            .selected_text(app.config.model.clone())
            .show_ui(ui, |ui| {
                for m in whisper::MODELS {
                    let selected = app.config.model == m.key;
                    if ui
                        .selectable_label(selected, format!("{} - {}", m.key, m.label))
                        .clicked()
                        && !selected
                    {
                        app.config.model = m.key.to_string();
                        model_changed = true;
                    }
                }
            });
        if model_changed {
            app.config.save();
        }
    });

    ui.horizontal(|ui| {
        ui.label("语言:");
        let mut lang_changed = false;
        // 下拉框显示中文名(lang_display,和完成状态行回显用的是同一个函数,保持
        // 界面上语言名称的说法前后一致),存进 config 的还是 ISO 码,不影响存储格式。
        egui::ComboBox::from_id_salt("lang")
            .selected_text(lang_display(&app.config.language))
            .show_ui(ui, |ui| {
                for lang in ["auto", "en", "zh", "ja", "ko", "fr", "de", "es"] {
                    if ui
                        .selectable_label(app.config.language == lang, lang_display(lang))
                        .clicked()
                    {
                        app.config.language = lang.to_string();
                        lang_changed = true;
                    }
                }
            });
        if lang_changed {
            app.config.save();
        }
    });

    let mut overwrite = app.config.overwrite_existing;
    if ui.checkbox(&mut overwrite, "已有同名字幕时覆盖").changed() {
        app.config.overwrite_existing = overwrite;
        app.config.save();
    }

    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);

    let mut config_dirty = false;
    let mut translate = app.config.translate;
    // "本地模型"是给开发者看的实现细节;新手真正关心的是"要不要联网、内容会不会被
    // 传出去",直接把这个卖点说出来比暗示"用了本地模型"更直接。
    if ui.checkbox(&mut translate, "翻译成中文(在本机运行,不上传内容)").changed() {
        app.config.translate = translate;
        config_dirty = true;
    }

    if app.config.translate {
        ui.horizontal(|ui| {
            ui.label("翻译模型:");
            let current_label = if app.config.translate_model == "auto" {
                "自动(按视频语言选择)".to_string()
            } else {
                translate::model_by_key(&app.config.translate_model)
                    .map(|m| m.label.to_string())
                    .unwrap_or_else(|| app.config.translate_model.clone())
            };
            egui::ComboBox::from_id_salt("translate_model")
                .selected_text(current_label)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(app.config.translate_model == "auto", "自动(按视频语言选择)")
                        .clicked()
                    {
                        app.config.translate_model = "auto".to_string();
                        config_dirty = true;
                    }
                    for m in translate::MODELS {
                        if ui
                            .selectable_label(app.config.translate_model == m.key, m.label)
                            .clicked()
                        {
                            app.config.translate_model = m.key.to_string();
                            config_dirty = true;
                        }
                    }
                });
        });

        let mut bilingual = app.config.bilingual;
        if ui.checkbox(&mut bilingual, "双语字幕(中文+原文)").changed() {
            app.config.bilingual = bilingual;
            app.config.save();
        }
    }

    if config_dirty {
        app.config.save();
    }
}
