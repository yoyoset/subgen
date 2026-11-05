//! 全局视觉主题:唯一的语义色板 + egui 风格定制。
//!
//! 之前界面颜色散落在 format.rs / queue.rs / detail.rs / batch_bar.rs / setup_screens.rs
//! / model_manifest.rs 里各自硬编码 RGB,同一个"警告"色在 5 个文件里有 5 份字面量,想统一
//! 调整(比如换一个更专业的配色)要改一圈还容易漏。这里收成一份语义色板:
//! - 语义色板只表达"含义"(成功/警告/错误/进行中/中性),不表达具体 RGB——界面代码用
//!   `theme::SUCCESS` 而不是 `Color32::from_rgb(60, 170, 60)`,换配色只改这一个文件。
//! - `apply_theme` 在程序启动时设置一次 egui 的 Visuals/Style(圆角、间距、窗口填充色等),
//!   让整界面的观感一致,而不是各面板各自拼凑。
//!
//! 本文件必须是纯颜色/样式定义,不依赖任何业务状态——任何模块都可以安全地引用它。

use eframe::egui;

/// 品牌主色(蓝色系):主操作按钮、"正在进行的步骤"。
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(66, 120, 200);
/// 进行中/信息提示的浅蓝:在深色背景上是"正在跑"的视觉锚点(队列状态点、步骤条当前步)。
pub const ACCENT_LIGHT: egui::Color32 = egui::Color32::from_rgb(90, 150, 230);
/// 完成/就绪的绿色。
pub const SUCCESS: egui::Color32 = egui::Color32::from_rgb(60, 170, 60);
/// 警告/建议核对的琥珀色。
pub const WARN: egui::Color32 = egui::Color32::from_rgb(230, 150, 20);
/// 错误/失败的红色(比纯 RED 稍微偏暖,和琥珀色的区分度更高,深色背景下也不刺眼)。
pub const ERROR: egui::Color32 = egui::Color32::from_rgb(220, 80, 80);
/// 中性/等待/跳过/终止态的灰色。
pub const NEUTRAL: egui::Color32 = egui::Color32::from_rgb(150, 150, 150);
/// 次要文字(路径、辅助说明、空状态提示)——比主文字弱一档,降低视觉噪音。
pub const TEXT_SECONDARY: egui::Color32 = egui::Color32::from_rgb(140, 140, 140);

/// 窗口/面板内边距常量,统一各处留白,避免每个面板各自拍一个数字。
pub const PANEL_PADDING: f32 = 10.0;
pub const SECTION_SPACING: f32 = 8.0;

/// 主按钮(开始处理/开始下载这类用户最希望一眼看到、点击次数最高的操作)的样式:
/// 用品牌色填充 + 描边,和普通按钮形成明确的主次关系。
pub fn primary_button(label: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    egui::Button::new(label)
        .fill(ACCENT)
        .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.6)))
}

/// 居中卡片页:首次下载/下载进度/搬迁/错误页和主界面空状态共用的外框。
///
/// 坑(2026-11-05 实测):之前这几处都是 `ui.centered_and_justified(|ui| Frame::show(..))`。
/// centered_and_justified 是给"单个控件撑满并居中"用的,卡片里的子 ui 继承了这个布局,
/// 于是第一个控件(标题)被拉伸占满整块区域、居中显示,后面的说明、组件清单、"更改..."、
/// "开始下载"全被挤出窗口——用户在全新目录首次启动,看到的是一张只有一行标题的空卡片,
/// 无从下手。开发机上的日常副本组件早就装齐、永远进不到这个页面,所以一直没被发现。
/// 现在:外层可滚动(窗口最小 560x420,首次下载页内容更高)、水平居中,卡片内部显式
/// 重置为自上而下、左对齐的布局。
pub fn card_page<R>(
    ui: &mut egui::Ui,
    max_width: f32,
    fill: egui::Color32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    const MARGIN: i8 = 18;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                // 内容短时不贴着顶边,长时也只占一点点高度。
                ui.add_space((ui.available_height() * 0.08).clamp(12.0, 48.0));
                let outer = max_width.min(ui.available_width() - 16.0).max(240.0);
                egui::Frame::default()
                    .inner_margin(egui::Margin::same(MARGIN))
                    .corner_radius(egui::CornerRadius::same(8))
                    .fill(fill)
                    .show(ui, |ui| {
                        ui.set_width(outer - 2.0 * f32::from(MARGIN));
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), add_contents)
                            .inner
                    })
                    .inner
            })
            .inner
        })
        .inner
}

/// 程序启动时调用一次:应用统一的深色视觉风格。
///
/// 固定深色主题、不跟随系统亮/暗设置——界面语义色(成功/警告/进行中/完成)是围绕深色
/// 背景调校的"深底发光"色,系统切到亮色会糊成一团;媒体处理工具固定深色也是常见选择。
///
/// 目前只用语义色板里的品牌色微调 egui 默认深色主题(控件圆角、按钮内边距、窗口角),
/// 不把每个控件的颜色都重写一遍——默认主题本身已经协调,过度定制反而引入新的不一致。
pub fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let radius = egui::CornerRadius::same(4);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        for wv in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            wv.corner_radius = radius;
        }
    });
    let mut visuals = egui::Visuals::dark();
    visuals.window_corner_radius = egui::CornerRadius::same(6);
    visuals.selection.bg_fill = ACCENT;
    ctx.set_visuals(visuals);
}
