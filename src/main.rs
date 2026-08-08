// Rust 默认把 Windows 可执行文件编译成"控制台子系统"程序,启动 GUI 时会顺带弹出一个空 cmd 窗口。
// 只在 release 构建时抑制它(debug 构建保留控制台,方便开发时看 println!/panic 输出,不用另开终端)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod engine;
mod format;
mod job;
mod job_guard;
mod models;
mod paths;
mod report;
mod setup;
mod translate;
mod ui;

use app::SubgenApp;
use config::Config;
use eframe::egui;
use std::path::PathBuf;

fn main() -> eframe::Result<()> {
    if !acquire_single_instance_lock() {
        rfd::MessageDialog::new()
            .set_title("字幕生成器 subgen")
            .set_description("subgen 已经在运行了,请不要重复打开。")
            .set_level(rfd::MessageLevel::Warning)
            .show();
        return Ok(());
    }
    install_panic_logger();
    job_guard::init_child_job_object();
    let config = Config::load();
    // 模型目录覆盖值是进程级状态(见 paths.rs),用配置里读到的值设置一次;用户在界面里
    // 改存放位置时会再调用一次,让后续路径解析立刻用上新值。
    paths::set_models_dir_override(config.models_dir_override.clone().map(PathBuf::from));
    // 模型绑定表同理是进程级状态(见 paths.rs);目前只有启动时读取这一条路径,
    // 还没有 UI 会写入 config.model_files(见项目计划 B7),空表时行为完全不变。
    paths::set_model_bindings(
        config
            .model_files
            .iter()
            .map(|(k, v)| (k.clone(), PathBuf::from(v)))
            .collect(),
    );
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([config.window_w, config.window_h])
        .with_min_inner_size([560.0, 420.0])
        .with_drag_and_drop(true);
    // 窗口图标(任务栏/标题栏,程序运行中看到的那个)——exe 文件本身的图标由 build.rs 通过
    // winresource 单独嵌入,这里是运行时窗口的图标,两处都要设置才能在所有场景下看到一致
    // 的图标而不是默认灰色齿轮。解码失败(比如以后换图标时不小心存成了非法 PNG)不该阻断
    // 启动,跳过设置图标、其余照常运行即可。
    match eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => eprintln!("加载窗口图标失败(不影响功能): {e}"),
    }
    let options = eframe::NativeOptions {
        viewport,
        // 显式用 glow(见 Cargo.toml 里 eframe 的 feature 说明),不依赖 Renderer 的
        // Default 实现——Cargo.toml 已经把 wgpu 从编译产物里拿掉,这里写明白只是让
        // 意图在代码里也看得见,不用去翻依赖配置才知道用的是哪个后端。
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        &window_title(),
        options,
        Box::new(|cc| {
            setup_chinese_fonts(&cc.egui_ctx);
            Ok(Box::new(SubgenApp::new(config)))
        }),
    )
}

/// 不带批次进度的基础窗口标题(带版本号——用户反馈问题时能直接说清自己用的是哪一版,
/// 不用再额外去翻文件属性)。处理中会在这基础上加"[N/M]"后缀(见 poll_worker),批次
/// 结束后标题恢复成这个基础版本。
fn window_title() -> String {
    format!("字幕生成器 subgen v{}", env!("CARGO_PKG_VERSION"))
}

/// 单实例保护:双开 subgen 会共用同一个 `tmp/` 目录、抢同一个翻译引擎端口(见 translate.rs
/// 的 `PORT`),第二个实例遇到的报错会被误导成"显存不足"或莫名其妙的文件冲突,完全看不出
/// 真实原因是"已经开了一个"。对 exe 同目录下的 `subgen.lock` 加进程级排他锁,锁不到说明
/// 已有实例在跑。用标准库 `File::try_lock`(Rust 1.89 起稳定)而不是 fs4 crate——虽然
/// Cargo.toml 里已经有 fs4(setup.rs 用它查磁盘剩余空间),但文件锁这块标准库原生就有,
/// 不用再多绕一层。
///
/// 锁文件句柄用 `Box::leak` 让它活到进程结束——同一个进程内不会有第二次加锁的需求,不需要
/// 精细管理生命周期;进程退出(不管正常退出、崩溃还是被任务管理器强杀)时操作系统会自动
/// 回收文件句柄,锁随之释放,不依赖任何应用层清理代码(和 job_guard.rs 里 Job Object 的
/// 设计哲学一致)。
///
/// 返回 `true` 表示拿到了锁、可以继续启动;`false` 表示已有实例在跑。加锁过程本身出错(比如
/// exe 所在目录只读、建不出锁文件)时按"能启动"处理,不该因为这类边缘情况拦住正常用户。
fn acquire_single_instance_lock() -> bool {
    let lock_path = paths::app_dir().join("subgen.lock");
    // 这个文件只用来拿排他锁,从来不写入内容——truncate/append 选哪个都不影响行为,
    // 显式 truncate(true) 只是消除"create+write 却不说清截断策略"的歧义(clippy
    // suspicious_open_options 提示过),不是修复真实 bug。
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(_) => return true,
    };

    match file.try_lock() {
        Ok(()) => {
            Box::leak(Box::new(file));
            true
        }
        Err(std::fs::TryLockError::WouldBlock) => false,
        Err(std::fs::TryLockError::Error(_)) => true,
    }
}

/// 注册全局 panic hook,把 panic 信息(消息 + 代码位置)追加写进 logs/crash.log。
///
/// 背景:release 构建用 `windows_subsystem = "windows"` 关掉了控制台(见文件顶部),这意味着
/// panic 默认打印到的 stderr 根本没有地方可看——程序一旦在正式使用中 panic,除了"窗口消失了"
/// 之外什么线索都留不下。已经实测遇到过一次:主界面消失、但它 spawn 出来的 whisper-cli 子
/// 进程还在正常运行(说明 panic 发生在主线程/渲染这边,不在子进程里),但当时完全拿不到
/// panic 的具体消息,只能靠间接证据(有没有生成文件、Windows 有没有崩溃事件记录)去反推,
/// 排查效率很低。这里加个 hook 之后,下次同类情况能直接看到 panic 消息和发生位置。
///
/// 用 `take_hook` + 链式调用而不是完全替换:保留默认行为(debug 构建下控制台还是能看到),
/// 只是额外多写一份到文件里。
fn install_panic_logger() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let _ = std::fs::create_dir_all(paths::logs_dir());
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = format!("\n=== 崩溃(unix 时间戳 {timestamp})===\n{info}\n");
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths::logs_dir().join("crash.log"))
        {
            let _ = file.write_all(entry.as_bytes());
        }
    }));
}

/// egui 默认字体不含中文字形,不加载系统字体的话中文会全部显示成方块/问号。
fn setup_chinese_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            break;
        }
    }
    ctx.set_fonts(fonts);
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_logger_writes_message_and_location_to_crash_log() {
        let crash_log = paths::logs_dir().join("crash.log");
        let _ = std::fs::remove_file(&crash_log);

        // cargo test 自己也注册了一个 panic hook(用来在不杀掉整个测试进程的前提下,把 panic
        // 当成"这个测试失败了"报出来),先把它存起来,测试完再原样恢复——不然会影响同一个
        // 进程里并行跑的其它测试。
        let original_hook = std::panic::take_hook();
        install_panic_logger();
        let result = std::panic::catch_unwind(|| {
            panic!("测试用的故意触发的 panic,不是真的出问题");
        });
        std::panic::set_hook(original_hook);

        assert!(result.is_err(), "catch_unwind 应该捕获到这次故意触发的 panic");

        let content = std::fs::read_to_string(&crash_log).expect("crash.log 应该已经被写出来");
        assert!(
            content.contains("测试用的故意触发的 panic"),
            "crash.log 应该包含 panic 消息: {content}"
        );
        assert!(content.contains("main.rs"), "crash.log 应该包含发生位置: {content}");

        let _ = std::fs::remove_file(&crash_log);
    }

    /// 验证单实例锁依赖的底层机制:同一个文件路径,两个独立的文件句柄(模拟两个进程各自
    /// open 同一个 subgen.lock),第一个 try_lock 成功后,第二个必须失败并且是
    /// `WouldBlock`(不是别的错误类型)——这正是 `acquire_single_instance_lock` 用来判断
    /// "已有实例在跑"的依据。不直接调用 `acquire_single_instance_lock` 本身,因为它写死了
    /// `paths::app_dir()` 下的固定路径、且用 `Box::leak` 让锁活到进程结束,在 cargo test
    /// 这种同进程跑多个测试的场景下会产生跨测试的副作用,换成独立临时文件路径验证同一套
    /// 机制,行为等价但不干扰其它测试。
    #[test]
    fn second_handle_to_locked_file_fails_with_would_block() {
        let path = std::env::temp_dir().join(format!(
            "subgen_test_lock_{:x}.lock",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let first = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .expect("应该能创建/打开锁文件");
        first.try_lock().expect("第一个句柄应该能拿到排他锁");

        let second = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .expect("应该能打开同一个文件(open 本身不受锁影响)");
        match second.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => {}
            other => panic!("第二个句柄应该拿锁失败并报 WouldBlock,实际: {other:?}"),
        }

        drop(first);
        let _ = std::fs::remove_file(&path);
    }
}
