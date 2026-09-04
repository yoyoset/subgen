use crate::app::SubgenApp;
use crate::paths;
use crate::ui::model_manifest;
use eframe::egui;

/// "组件/存放位置"是独立于设置的按需浮窗——模型存放目录这类"环境"信息,处理中也可能
/// 想确认(这次到底用的是哪个目录),而设置项处理中打开是只读摘要,两者的处理中行为
/// 不一样,拆成两个入口比揉进一个窗口里再各自加条件判断更清楚。这个窗口现在同时是
/// 模型清单(逐个模型确认状态,见 model_manifest.rs)的宿主。
pub(crate) fn render_components_window(app: &mut SubgenApp, ctx: &egui::Context) {
    if !app.components_open {
        return;
    }
    let mut open = app.components_open;
    egui::Window::new("组件与存放位置")
        .open(&mut open)
        .resizable(true)
        .default_size([560.0, 420.0])
        .show(ctx, |ui| {
            ui.add_space(crate::ui::theme::PANEL_PADDING);
            // 常驻显示 + 可改,不止首次下载那一次机会——用了一段时间之后想搬家也应该
            // 随时能改,不应该被困在当初选的位置。处理中禁用:一批任务跑到一半时,
            // 后面文件的翻译阶段会用当时刚查到的模型路径去启动 llama-server,这时候
            // 换目录容易踩"模型文件还没搬过去"的空。
            ui.label(format!("模型位置: {}", paths::models_dir().display()));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!app.processing, egui::Button::new("更改..."))
                    .clicked()
                {
                    app.begin_change_models_dir();
                }
                // 排查问题时的常用入口:日志(含每个文件的识别/翻译过程日志和崩溃记录)
                // 都在 exe 同目录的 logs/ 下,直接一键打开比让用户自己去找方便得多。
                if ui.button("打开日志文件夹").clicked() {
                    let _ = std::fs::create_dir_all(paths::logs_dir());
                    let _ = std::process::Command::new("explorer")
                        .arg(paths::logs_dir())
                        .spawn();
                }
            });
            // GPU 状态 + 翻译模型显存适配提示:探测(缓存)一次 `--list-devices`,把总/空闲
            // 显存和当前要跑的翻译模型摆在一起,让"翻译会很慢/启动失败"这类问题在开工前
            // 就能看到,而不是等翻译阶段才暴露。翻译引擎还没装时探测不出结果,显示占位文案
            // 并在装好后自动重探(见 translate::server::invalidate_gpu_mem_cache)。
            match crate::translate::probe_gpu_mem_cached() {
                Some(Some(info)) => {
                    let mem = match (info.total_mib, info.free_mib) {
                        (Some(t), Some(f)) => format!(
                            "{} MB / {} MB",
                            crate::format::format_download_size(f * 1024 * 1024),
                            crate::format::format_download_size(t * 1024 * 1024)
                        ),
                        _ => "显存信息未知".to_string(),
                    };
                    ui.horizontal(|ui| {
                        ui.colored_label(crate::ui::theme::SUCCESS, "✓ GPU");
                        ui.weak(format!("{} · {}", info.name, mem));
                    });
                    // 翻译模型的文件大小和空闲显存对比:文件装进显存后的占用只会比文件本身大
                    // (权重 + KV cache + 临时缓冲),所以"空闲 < 文件大小"基本可以断定放不下,
                    // 给出明确建议而不是等翻译阶段 llama-server 启动失败。
                    if app.config.translate {
                        for tm in crate::translate::MODELS {
                            let needed = tm.size_bytes as f64;
                            if let Some(free_mib) = info.free_mib {
                                let free_bytes = free_mib as f64 * 1024.0 * 1024.0;
                                if needed > free_bytes {
                                    ui.colored_label(
                                        crate::ui::theme::WARN,
                                        format!(
                                            "⚠ {} 约 {} ,当前空闲显存约 {} ,可能放不下,\
                                             翻译会变慢或启动失败;可换更小的模型或关闭翻译",
                                            tm.label,
                                            crate::format::format_download_size(tm.size_bytes),
                                            crate::format::format_download_size(free_bytes as u64),
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
                Some(None) => {
                    ui.horizontal(|ui| {
                        ui.colored_label(crate::ui::theme::WARN, "⚠ 未检测到可用 GPU");
                        ui.weak("识别与翻译将使用 CPU(较慢)");
                    });
                }
                None => {
                    ui.weak("GPU 状态:待翻译引擎安装后检测…");
                }
            }
            if let Some(e) = &app.models_dir_error {
                ui.colored_label(crate::ui::theme::ERROR, e);
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            model_manifest::render_manifest_body(app, ui);
        });
    app.components_open = open;
}
