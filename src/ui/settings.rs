use crate::app::{SetupPhase, SubgenApp};
use crate::config::Config;
use crate::format::*;
use crate::paths;
use crate::translate;
use eframe::egui;

/// 顶部设置条:模型/语言/覆盖策略/翻译开关及其子选项 + 模型存放位置。
pub(crate) fn render_settings_panel(app: &mut SubgenApp, ui: &mut egui::Ui) {
        egui::Panel::top("controls").show(ui, |ui| {
            ui.add_space(6.0);
            // 处理中这一整块设置(模型/语言/覆盖策略/翻译开关及其子选项)全部禁用——之前
            // 界面上看着能改,改了也确实存进了 config,但当前这一批任务完全不会用上新设置,
            // 用户会以为"改了=生效了",跑完发现结果和界面显示的设置对不上,是很隐蔽的
            // "设置是摆设"体验。禁用后 hover 会有一层浅灰遮罩,直观地表明"现在不能碰"。
            ui.add_enabled_ui(!app.processing, |ui| {
            ui.horizontal(|ui| {
                ui.label("模型:");
                let mut model_changed = false;
                egui::ComboBox::from_id_salt("model")
                    .selected_text(app.config.model.clone())
                    .show_ui(ui, |ui| {
                        for (key, desc) in Config::AVAILABLE_MODELS {
                            let selected = app.config.model == *key;
                            if ui
                                .selectable_label(selected, format!("{key} - {desc}"))
                                .clicked()
                                && !selected
                            {
                                app.config.model = key.to_string();
                                model_changed = true;
                            }
                        }
                    });
                if model_changed {
                    app.config.save();
                    app.setup_phase = SetupPhase::Checking;
                }

                ui.add_space(16.0);
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

                ui.add_space(16.0);
                let mut overwrite = app.config.overwrite_existing;
                if ui.checkbox(&mut overwrite, "已有同名字幕时覆盖").changed() {
                    app.config.overwrite_existing = overwrite;
                    app.config.save();
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // 开关/模型切换需要重新检测组件是否已下载(可能要装 llama.cpp + 模型文件),
                // 双语/仅中文纯粹是输出格式,不影响要下载什么,所以单独存 save 不触发重新检测。
                let mut needs_recheck = false;
                let mut translate = app.config.translate;
                // "本地模型"是给开发者看的实现细节;新手真正关心的是"要不要联网、内容
                // 会不会被传出去",直接把这个卖点说出来比暗示"用了本地模型"更直接。
                if ui.checkbox(&mut translate, "翻译成中文(在本机运行,不上传内容)").changed() {
                    app.config.translate = translate;
                    needs_recheck = true;
                }

                if app.config.translate {
                    ui.add_space(16.0);
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
                                .selectable_label(
                                    app.config.translate_model == "auto",
                                    "自动(按视频语言选择)",
                                )
                                .clicked()
                            {
                                app.config.translate_model = "auto".to_string();
                                needs_recheck = true;
                            }
                            for m in translate::MODELS {
                                if ui
                                    .selectable_label(app.config.translate_model == m.key, m.label)
                                    .clicked()
                                {
                                    app.config.translate_model = m.key.to_string();
                                    needs_recheck = true;
                                }
                            }
                        });

                    ui.add_space(16.0);
                    let mut bilingual = app.config.bilingual;
                    if ui.checkbox(&mut bilingual, "双语字幕(中文+原文)").changed() {
                        app.config.bilingual = bilingual;
                        app.config.save();
                    }
                }

                if needs_recheck {
                    app.config.save();
                    app.setup_phase = SetupPhase::Checking;
                }
            });
            }); // add_enabled_ui(!app.processing) 到此结束
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // 常驻显示 + 可改,不止首次下载那一次机会——用了一段时间之后想搬家也应该
                // 随时能改,不应该被困在当初选的位置。处理中禁用:一批任务跑到一半时,
                // 后面文件的翻译阶段会用当时刚查到的模型路径去启动 llama-server,这时候
                // 换目录容易踩"模型文件还没搬过去"的空。
                ui.label(format!("模型位置: {}", paths::models_dir().display()));
                if ui
                    .add_enabled(!app.processing, egui::Button::new("更改..."))
                    .clicked()
                {
                    app.begin_change_models_dir();
                }
                if let Some(e) = &app.models_dir_error {
                    ui.colored_label(egui::Color32::RED, e);
                }
            });
            ui.add_space(6.0);
        });

}
