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
            ui.add_space(crate::ui::theme::PANEL_PADDING);
            if app.processing {
                render_readonly_summary(app, ui);
            } else {
                render_editable_form(app, ui);
            }
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            render_about_footer(ui);
        });
    app.settings_open = open;
}

/// 底部"关于/帮助"脚注:版本号 + 排障信息去哪找。版本号同时出现在窗口标题里,但那里
/// 处理中会被"第 N/M 个"后缀挤掉,这里是随时能确认自己用的是哪一版的地方——报障时
/// "先报版本号"是最基本的第一步。手动安装组件/重装/卸载的说明在 README,这里只指路,
/// 不复述内容(README 才是唯一事实来源)。
fn render_about_footer(ui: &mut egui::Ui) {
    ui.label(format!(
        "字幕生成器 subgen v{} · 全部处理在本机完成,不上传内容",
        env!("CARGO_PKG_VERSION")
    ));
    ui.weak(
        "组件下载失败需要手动安装时,打开 exe 同目录下的 README.md 按说明操作;\
         运行日志和崩溃记录在 exe 同目录的 logs/ 文件夹里。",
    );
}

fn render_readonly_summary(app: &SubgenApp, ui: &mut egui::Ui) {
    ui.label("这批正在使用的设置(改动要等下一批才会生效):");
    ui.add_space(4.0);
    // 只读摘要里模型也用友好档位名(和可编辑表单/首次下载页的选项一致),别只显示
    // "large-v3" 这种内部 key——处理中用户想确认"这批用的是什么模型"时,名字比 key 好懂。
    let model_label = whisper::by_key(&app.config.model)
        .map(|m| format!("{} - {}", m.key, m.label))
        .unwrap_or_else(|| app.config.model.clone());
    ui.label(format!("· 模型:{model_label}"));
    ui.label(format!("· 语言:{}", lang_display(&app.config.language)));
    ui.label(format!(
        "· 已有同名字幕:{}",
        if app.config.overwrite_existing { "覆盖" } else { "跳过" }
    ));
    if app.config.translate {
        let label = if app.config.translate_model == "auto" {
            "自动(按视频语言选择)".to_string()
        } else {
            translate::model_by_key(&app.config.translate_model)
                .map(|m| m.label.to_string())
                .unwrap_or_else(|| app.config.translate_model.clone())
        };
        ui.label(format!("· 翻译:开启 · {label}"));
        ui.label(format!(
            "· 输出:{}",
            if app.config.bilingual {
                "双语(中文+原文)"
            } else {
                "仅中文"
            }
        ));
    } else {
        ui.label("· 翻译:关闭");
    }
    ui.label(format!(
        "· 字幕折行:{}",
        if app.config.subtitle_max_width == 0 {
            "关闭".to_string()
        } else {
            format!("每行不超过 {} 列", app.config.subtitle_max_width)
        }
    ));
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
                // 选项列表和中文名都来自 format::LANGUAGES 这一份常量,加/删语言只改那里。
                for (lang, name) in LANGUAGES {
                    if ui
                        .selectable_label(app.config.language == *lang, *name)
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

    // 单条字幕的显示宽度上限:同一时刻屏幕上字太多靠折行缓解,不碰识别/翻译结果。
    // 0 = 关闭折行(把旧行为交给想保持的用户)。
    ui.horizontal(|ui| {
        ui.label("字幕每行宽度:");
        let mut width = app.config.subtitle_max_width;
        let resp = ui
            .add(egui::DragValue::new(&mut width).range(0..=200).suffix(" 列"))
            .on_hover_text("按显示列计,CJK/全角字符算 2 列(即中文每行 21 字);0 = 关闭折行");
        if resp.changed() {
            app.config.subtitle_max_width = width;
            app.config.save();
        }
    });

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
