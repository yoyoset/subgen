//! 把 assets/icon.ico 嵌进 Windows exe 资源——File Explorer、任务栏(程序没运行时)、
//! 快捷方式看到的图标都来自这里,不设置的话是 Rust/Cargo 的默认灰色齿轮图标,一眼就能
//! 看出"这是个开发者随手写的工具",不像正经软件。
//!
//! 图标是占位设计(深蓝圆角方块 + 白色"字"字,assets/icon.png 是同一份图的 PNG 版本,
//! 供运行时窗口图标用),后续有设计稿可以直接替换 assets/icon.ico/icon.png,不用改这个
//! 文件。
fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    if let Err(e) = res.compile() {
        // 图标嵌入失败不该让整个构建失败——比如非 Windows 交叉编译环境缺少 windres 工具链
        // 时,宁可产出一个没有自定义图标的 exe,也不要因为这个非核心资源阻断构建。
        println!("cargo:warning=嵌入图标失败(不影响功能,只是 exe 图标会是默认样式): {e}");
    }
}
