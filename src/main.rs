// Rust 默认把 Windows 可执行文件编译成"控制台子系统"程序,启动 GUI 时会顺带弹出一个空 cmd 窗口。
// 只在 release 构建时抑制它(debug 构建保留控制台,方便开发时看 println!/panic 输出,不用另开终端)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod engine;
mod job;
mod job_guard;
mod paths;
mod setup;
mod translate;

use config::Config;
use eframe::egui;
use job::{Job, JobStatus, WorkerEvent};
use setup::{ComponentKey, SetupEvent};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

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
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([config.window_w, config.window_h])
            .with_min_inner_size([560.0, 420.0])
            .with_drag_and_drop(true),
        // 显式用 glow(见 Cargo.toml 里 eframe 的 feature 说明),不依赖 Renderer 的
        // Default 实现——Cargo.toml 已经把 wgpu 从编译产物里拿掉,这里写明白只是让
        // 意图在代码里也看得见,不用去翻依赖配置才知道用的是哪个后端。
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "字幕生成器 subgen",
        options,
        Box::new(|cc| {
            setup_chinese_fonts(&cc.egui_ctx);
            Ok(Box::new(SubgenApp::new(config)))
        }),
    )
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
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
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

enum SetupPhase {
    Checking,
    NeedsDownload(Vec<ComponentKey>),
    Downloading {
        current_label: String,
        current_index: usize, // 1-based:当前是第几项
        total_count: usize,   // 本次会话总共要下载几项
        downloaded: u64,
        total: u64,
        queue: Vec<ComponentKey>, // 还没开始下载的项(不含当前这项)
    },
    /// 用户更改了模型存放目录、旧目录里有现成文件时,正在把它们搬到新目录(而不是让用户
    /// 重新下载十几 GB)。current_index/total_count 在拿到第一条 Progress 事件前是 0,
    /// 界面上这段时间显示"正在准备..."而不是"第 0/0 个"。
    Migrating {
        current_label: String,
        current_index: usize,
        total_count: usize,
        copied: u64,
        total: u64,
    },
    Ready,
    Error(String),
}

struct SubgenApp {
    config: Config,
    jobs: Vec<Job>,
    setup_phase: SetupPhase,
    setup_rx: Option<mpsc::Receiver<SetupEvent>>,
    setup_cancel: Arc<AtomicBool>,
    worker_rx: Option<mpsc::Receiver<WorkerEvent>>,
    worker_cancel: Arc<AtomicBool>,
    processing: bool,
    /// 用户点了"取消"、但批次还没真正停下来(worker 线程要等当前这一步跑完才会检查取消
    /// 标志)。只用来驱动界面反馈——按钮立刻变灰显示"正在取消…"、当前任务状态行附加提示,
    /// 不然用户会因为点击后界面毫无变化而怀疑按钮没反应,进而连点或直接关窗强退。
    cancelling: bool,
    gpu_warning_shown: bool,
    log_lines: Vec<String>,
    migrate_rx: Option<mpsc::Receiver<setup::MigrateEvent>>,
    migrate_cancel: Arc<AtomicBool>,
    /// 搬迁线程跑完后要提交成生效目录的目标路径;None 说明当前没有搬迁在进行。
    pending_models_dir: Option<PathBuf>,
    /// 更改模型目录时的校验错误(目录不可写等),显示在"更改..."按钮旁边,下次操作前清空。
    models_dir_error: Option<String>,
    /// 处理中用户点了窗口的关闭按钮,弹出"确定退出吗"确认弹窗时为 true。
    close_confirm_open: bool,
    /// 用户在确认弹窗里点了"确定退出"——下一次(或本次)收到关闭请求时不再拦截,真正放行。
    /// 没有这个标志,程序发出的 ViewportCommand::Close 如果也会产生一次 close_requested()
    /// 事件,会被同一段"处理中就拦截"的逻辑再次误拦,变成怎么点都关不掉。
    allow_close: bool,
    /// 处理中拖入文件时给出的提示,连同显示到期时间一起存着,到点了在下一帧渲染前自动清掉
    /// (不需要用户手动关闭这条提示)。
    transient_notice: Option<(String, std::time::Instant)>,
}

impl SubgenApp {
    fn new(config: Config) -> Self {
        Self {
            config,
            jobs: Vec::new(),
            setup_phase: SetupPhase::Checking,
            setup_rx: None,
            setup_cancel: Arc::new(AtomicBool::new(false)),
            worker_rx: None,
            worker_cancel: Arc::new(AtomicBool::new(false)),
            processing: false,
            cancelling: false,
            gpu_warning_shown: false,
            log_lines: Vec::new(),
            migrate_rx: None,
            migrate_cancel: Arc::new(AtomicBool::new(false)),
            pending_models_dir: None,
            models_dir_error: None,
            close_confirm_open: false,
            allow_close: false,
            transient_notice: None,
        }
    }

    fn begin_setup_check(&mut self) {
        let missing = setup::missing_components(
            &self.config.model,
            self.config.translate,
            &self.config.translate_model,
        );
        if missing.is_empty() {
            self.setup_phase = SetupPhase::Ready;
        } else {
            self.setup_phase = SetupPhase::NeedsDownload(missing);
        }
    }

    fn start_downloading(&mut self) {
        let missing = match &self.setup_phase {
            SetupPhase::NeedsDownload(m) => m.clone(),
            _ => return,
        };
        let (tx, rx) = mpsc::channel();
        self.setup_rx = Some(rx);
        self.setup_cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.setup_cancel.clone();
        let model = self.config.model.clone();

        self.setup_phase = SetupPhase::Downloading {
            current_label: missing
                .first()
                .map(|k| k.label().to_string())
                .unwrap_or_default(),
            current_index: 1,
            total_count: missing.len(),
            downloaded: 0,
            total: 0,
            queue: missing.iter().skip(1).cloned().collect(),
        };

        std::thread::spawn(move || {
            for key in missing {
                if cancel.load(Ordering::Relaxed) {
                    let _ = tx.send(SetupEvent::Failed("用户取消".to_string()));
                    return;
                }
                if let Err(e) = setup::install_component(key, &model, &tx, &cancel) {
                    let _ = tx.send(SetupEvent::Failed(e));
                    return;
                }
            }
            let _ = tx.send(SetupEvent::AllReady);
        });
    }

    /// 打开文件夹选择框,让用户指定模型存放目录。模型文件动辄几个 GB 到十几个 GB,不该被迫
    /// 跟着 exe 摆放的位置走(很多人图方便把 exe 放 C 盘,C 盘往往是容量较小的系统盘)。
    fn begin_change_models_dir(&mut self) {
        self.models_dir_error = None;
        let Some(new_dir) = rfd::FileDialog::new()
            .set_title("选择模型存放目录")
            .pick_folder()
        else {
            return; // 用户在选择框里按了取消
        };

        let old_dir = paths::models_dir();
        if new_dir == old_dir {
            return;
        }

        // 校验:先确认能建目录,再写一个探测文件确认真的能写——只判断"目录存在"是不够的,
        // 网络共享/只读挂载这类目录列出来是存在的,但写入会失败,不能等真正下载模型时才发现。
        if let Err(e) = std::fs::create_dir_all(&new_dir) {
            self.models_dir_error = Some(format!("无法使用该目录: {e}"));
            return;
        }
        let probe = new_dir.join(".subgen_write_test");
        if let Err(e) = std::fs::write(&probe, b"test") {
            self.models_dir_error = Some(format!("该目录不可写: {e}"));
            return;
        }
        let _ = std::fs::remove_file(&probe);

        let old_has_files = std::fs::read_dir(&old_dir)
            .map(|rd| rd.flatten().any(|e| e.path().is_file()))
            .unwrap_or(false);

        if !old_has_files {
            // 旧目录里没有已下载的文件可搬(比如全新安装、第一次选目录),直接切换。
            self.commit_models_dir(new_dir);
            return;
        }

        // 旧目录有现成的文件,搬过去而不是让用户重新下载十几 GB。
        self.migrate_cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        self.migrate_rx = Some(rx);
        let cancel = self.migrate_cancel.clone();
        let new_dir_for_thread = new_dir.clone();
        self.pending_models_dir = Some(new_dir);

        self.setup_phase = SetupPhase::Migrating {
            current_label: String::new(),
            current_index: 0,
            total_count: 0,
            copied: 0,
            total: 0,
        };

        std::thread::spawn(move || {
            let issues = setup::migrate_models_dir(&old_dir, &new_dir_for_thread, &tx, &cancel);
            let _ = tx.send(setup::MigrateEvent::AllDone(issues));
        });
    }

    /// 把 `new_dir` 设为生效的模型目录:写进程级覆盖值(paths.rs,立即对后续所有路径解析
    /// 生效)、存进配置文件(下次启动记住)、回到 Checking 让 missing_components() 用新目录
    /// 重新判断还缺什么。
    fn commit_models_dir(&mut self, new_dir: PathBuf) {
        paths::set_models_dir_override(Some(new_dir.clone()));
        self.config.models_dir_override = Some(new_dir.to_string_lossy().to_string());
        self.config.save();
        self.setup_phase = SetupPhase::Checking;
    }

    fn poll_migrate(&mut self, ctx: &egui::Context) {
        let mut finished: Option<Vec<String>> = None;
        if let Some(rx) = &self.migrate_rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    setup::MigrateEvent::Log(l) => push_log(&mut self.log_lines, l),
                    setup::MigrateEvent::Progress {
                        file_label,
                        current_index,
                        total_count,
                        copied,
                        total,
                    } => {
                        if let SetupPhase::Migrating {
                            current_label,
                            current_index: ci,
                            total_count: tc,
                            copied: c,
                            total: t,
                        } = &mut self.setup_phase
                        {
                            *current_label = file_label;
                            *ci = current_index;
                            *tc = total_count;
                            *c = copied;
                            *t = total;
                        }
                    }
                    setup::MigrateEvent::FileDone(label) => {
                        push_log(&mut self.log_lines, format!("{label} 搬迁完成"));
                    }
                    setup::MigrateEvent::AllDone(issues) => finished = Some(issues),
                }
            }
        }
        if let Some(issues) = finished {
            self.migrate_rx = None;
            for issue in &issues {
                push_log(&mut self.log_lines, format!("提示: {issue}"));
            }
            // 不管搬迁是否完全顺利(部分文件可能因为取消/出错留在了旧目录),都把生效目录切
            // 到新位置——已经搬过去的文件才是准确的当前状态,剩下的缺口交给现成的
            // missing_components() 首次下载流程去补齐,不需要为这个中间状态单独处理。
            if let Some(new_dir) = self.pending_models_dir.take() {
                self.commit_models_dir(new_dir);
            } else {
                self.setup_phase = SetupPhase::Checking;
            }
        }
        if self.migrate_rx.is_some() {
            ctx.request_repaint();
        }
    }

    /// 支持的视频扩展名,和"选择视频文件..."按钮的文件选择框过滤器保持一致。
    /// 拖拽进窗口的文件不经过那个过滤器,所以这里需要单独过滤一遍,避免把非视频文件
    /// (比如连同视频一起被拖进来的 .nfo/.jpg)也塞进任务列表。
    const VIDEO_EXTENSIONS: &'static [&'static str] =
        &["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "ts", "m2ts"];

    fn add_files(&mut self, paths: Vec<PathBuf>) {
        for p in paths {
            let is_video = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| {
                    Self::VIDEO_EXTENSIONS
                        .iter()
                        .any(|ext| ext.eq_ignore_ascii_case(e))
                })
                .unwrap_or(false);
            if is_video && !self.jobs.iter().any(|j| j.video_path == p) {
                self.jobs.push(Job::new(p));
            }
        }
    }

    fn start_processing(&mut self) {
        if self.jobs.is_empty() || self.processing {
            return;
        }
        // 只重新处理还没成功完成的任务——重新点"开始处理"不应该把已经出字幕的文件再跑一遍
        // (之前的实现会把所有任务的状态重置成 Pending 再全部重跑)。
        let indices: Vec<usize> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, j)| !matches!(j.status, JobStatus::Done(_) | JobStatus::DoneWithWarning(..)))
            .map(|(i, _)| i)
            .collect();
        if indices.is_empty() {
            return;
        }

        self.processing = true;
        self.cancelling = false;
        self.gpu_warning_shown = false;
        self.worker_cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);

        let batch: Vec<(usize, PathBuf)> = indices
            .iter()
            .map(|&i| (i, self.jobs[i].video_path.clone()))
            .collect();
        for &i in &indices {
            self.jobs[i].status = JobStatus::Pending;
            self.jobs[i].started_at = None;
        }

        job::run_batch(
            batch,
            self.config.model.clone(),
            self.config.language.clone(),
            self.config.overwrite_existing,
            self.config.translate,
            self.config.translate_model.clone(),
            self.config.bilingual,
            tx,
            self.worker_cancel.clone(),
        );
    }

    fn poll_setup(&mut self, ctx: &egui::Context) {
        let mut done = false;
        let mut failed: Option<String> = None;
        if let Some(rx) = &self.setup_rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    SetupEvent::Log(l) => push_log(&mut self.log_lines, l),
                    SetupEvent::Progress {
                        component,
                        downloaded,
                        total,
                    } => {
                        if let SetupPhase::Downloading {
                            current_label,
                            downloaded: d,
                            total: t,
                            ..
                        } = &mut self.setup_phase
                        {
                            *current_label = component;
                            *d = downloaded;
                            *t = total;
                        }
                    }
                    SetupEvent::ComponentReady(label) => {
                        push_log(&mut self.log_lines, format!("{label} 安装完成"));
                        if let SetupPhase::Downloading {
                            current_label,
                            current_index,
                            downloaded,
                            total,
                            queue,
                            ..
                        } = &mut self.setup_phase
                        {
                            if !queue.is_empty() {
                                let next = queue.remove(0);
                                *current_label = next.label().to_string();
                                *current_index += 1;
                                *downloaded = 0;
                                *total = 0;
                            }
                        }
                    }
                    SetupEvent::Failed(e) => failed = Some(e),
                    SetupEvent::AllReady => done = true,
                }
            }
        }
        if done {
            self.setup_phase = SetupPhase::Ready;
            self.setup_rx = None;
        } else if let Some(e) = failed {
            self.setup_phase = SetupPhase::Error(e);
            self.setup_rx = None;
        }
        if self.setup_rx.is_some() {
            ctx.request_repaint();
        }
    }

    fn poll_worker(&mut self, ctx: &egui::Context) {
        let mut batch_done = false;
        let mut gpu_missing = false;
        if let Some(rx) = &self.worker_rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    WorkerEvent::JobUpdate { index, status } => {
                        if let Some(j) = self.jobs.get_mut(index) {
                            if j.started_at.is_none() && matches!(status, JobStatus::Running(..)) {
                                j.started_at = Some(std::time::Instant::now());
                            }
                            j.status = status;
                        }
                    }
                    WorkerEvent::GpuMissing => gpu_missing = true,
                    WorkerEvent::BatchDone => batch_done = true,
                }
            }
        }
        if gpu_missing {
            self.gpu_warning_shown = true;
        }
        if batch_done {
            self.processing = false;
            self.cancelling = false;
            self.worker_rx = None;
        }
        if self.worker_rx.is_some() {
            ctx.request_repaint();
        }
    }
}

impl eframe::App for SubgenApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if matches!(self.setup_phase, SetupPhase::Checking) {
            self.begin_setup_check();
        }

        self.poll_setup(&ctx);
        self.poll_worker(&ctx);
        self.poll_migrate(&ctx);

        // 记录当前窗口尺寸,在 on_exit 里一次性落盘(而不是每帧都写文件——拖拽调整窗口大小
        // 时每帧尺寸都在变,逐帧写盘没必要地增加磁盘 IO)。config.toml 里其实一直有这两个
        // 字段,只是之前从来没人写过它们,改了窗口大小下次启动也不会记住。
        let screen = ctx.input(|i| i.viewport_rect());
        if screen.width() > 0.0 && screen.height() > 0.0 {
            self.config.window_w = screen.width();
            self.config.window_h = screen.height();
        }

        // 处理中点窗口右上角的 ×:直接放行等于把还在跑的批次悄悄腰斩,已经花掉的处理时间
        // 全部作废——这是整个界面里唯一一个"一次误点损失一小时"的操作,必须拦一下。
        // close_requested() 只在被点击的那一帧为 true(不是持续状态),拦截方式是当帧回发
        // CancelClose 抵消掉这次关闭请求,同时弹出确认框;非处理中或已经点过"确定退出"
        // (allow_close)时不拦截,让它正常关闭。
        if !self.allow_close
            && self.processing
            && ctx.input(|i| i.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_confirm_open = true;
        }
        if self.close_confirm_open {
            egui::Window::new("确定要退出吗?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    ui.label("正在处理中,退出将中止全部任务,已经处理完的文件不受影响。");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("继续处理").clicked() {
                            self.close_confirm_open = false;
                        }
                        if ui.button("确定退出").clicked() {
                            self.close_confirm_open = false;
                            self.allow_close = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
        }

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            if self.processing {
                // 静默吞掉拖进来的文件是这里最差的处理方式——用户会以为自己操作失败了、
                // 反复重试。处理中不支持追加排队(明确不做,见项目计划),但至少要让用户
                // 知道"我看到你拖文件了,只是现在不能加"。
                self.transient_notice = Some((
                    "正在处理中,请等本批完成后再添加文件".to_string(),
                    std::time::Instant::now(),
                ));
            } else {
                self.add_files(dropped);
            }
        }
        if let Some((_, shown_at)) = &self.transient_notice {
            if shown_at.elapsed().as_secs_f32() > 3.0 {
                self.transient_notice = None;
            } else {
                ctx.request_repaint(); // 保证过期后能被下一帧检查到、及时消失,不用等下次交互触发重绘
            }
        }

        match &self.setup_phase {
            SetupPhase::Checking => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.centered_and_justified(|ui| ui.label("正在检查运行环境..."));
                });
            }
            SetupPhase::NeedsDownload(missing) => {
                let missing = missing.clone();
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("首次使用需要下载必要组件");
                    ui.add_space(8.0);
                    ui.label("以下组件缺失,需联网下载:");
                    ui.add_space(4.0);
                    // 每项标大小、下面给总计——用户在不知情的情况下点"开始下载"其实是在
                    // 做一个"可能几十分钟、几个 GB 流量"的决定,不该让他们蒙在鼓里。
                    let mut total_bytes: u64 = 0;
                    for k in &missing {
                        let size = setup::estimated_size_bytes(*k, &self.config.model);
                        total_bytes += size;
                        ui.label(format!("· {}({})", k.label(), format_download_size(size)));
                    }
                    ui.add_space(4.0);
                    ui.label(format!(
                        "共需下载约 {},只需下载这一次。",
                        format_download_size(total_bytes)
                    ));
                    // 这句话也是这个软件最大的卖点,之前只写在 README 里,首次见面就该
                    // 让用户知道:视频和字幕不会离开这台电脑。
                    ui.label("所有组件均在你自己的电脑上运行,视频和字幕不会上传到任何服务器。");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(format!("模型将保存到: {}", paths::models_dir().display()));
                        if ui.button("更改...").clicked() {
                            self.begin_change_models_dir();
                        }
                    });
                    if let Some(e) = &self.models_dir_error {
                        ui.colored_label(egui::Color32::RED, e);
                    }
                    if let Some((needed, avail)) =
                        setup::check_disk_space(&missing, &self.config.model, &paths::models_dir())
                    {
                        if avail < needed {
                            ui.colored_label(
                                egui::Color32::from_rgb(230, 150, 20),
                                format!(
                                    "⚠ 目标磁盘剩余空间可能不够:预计需要约 {:.1} GB,当前剩余约 {:.1} GB。\
                                     可以点上面的“更改...”换一个空间更充足的位置。",
                                    needed as f64 / 1_073_741_824.0,
                                    avail as f64 / 1_073_741_824.0,
                                ),
                            );
                        }
                    }
                    ui.add_space(12.0);
                    if ui.button("开始下载").clicked() {
                        self.start_downloading();
                    }
                });
            }
            SetupPhase::Downloading {
                current_label,
                current_index,
                total_count,
                downloaded,
                total,
                ..
            } => {
                let current_label = current_label.clone();
                let current_index = *current_index;
                let total_count = *total_count;
                let downloaded = *downloaded;
                let total = *total;
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("正在下载组件...");
                    ui.add_space(8.0);
                    ui.label(format!(
                        "当前: {current_label}(第 {current_index}/{total_count} 项)"
                    ));
                    let progress = if total > 0 {
                        downloaded as f32 / total as f32
                    } else {
                        0.0
                    };
                    ui.add(egui::ProgressBar::new(progress).show_percentage());
                    ui.label(format!(
                        "{:.1} MB / {:.1} MB",
                        downloaded as f64 / 1_048_576.0,
                        total as f64 / 1_048_576.0
                    ));
                    ui.add_space(8.0);
                    if ui.button("取消").clicked() {
                        self.setup_cancel.store(true, Ordering::Relaxed);
                    }
                    ui.add_space(12.0);
                    ui.separator();
                    // 默认收起:原始下载日志逐行滚动,信息密度低、看着比较像半成品,
                    // 大多数用户根本不需要看这个;需要时(比如报障)自己展开就行。
                    egui::CollapsingHeader::new("详细信息")
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(160.0)
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    for line in &self.log_lines {
                                        ui.label(line);
                                    }
                                });
                        });
                });
            }
            SetupPhase::Migrating {
                current_label,
                current_index,
                total_count,
                copied,
                total,
            } => {
                let current_label = current_label.clone();
                let current_index = *current_index;
                let total_count = *total_count;
                let copied = *copied;
                let total = *total;
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("正在搬迁模型文件到新目录...");
                    ui.add_space(8.0);
                    if total_count > 0 {
                        ui.label(format!(
                            "当前: {current_label}(第 {current_index}/{total_count} 个)"
                        ));
                        let progress = if total > 0 {
                            copied as f32 / total as f32
                        } else {
                            0.0
                        };
                        ui.add(egui::ProgressBar::new(progress).show_percentage());
                        ui.label(format!(
                            "{:.1} MB / {:.1} MB",
                            copied as f64 / 1_048_576.0,
                            total as f64 / 1_048_576.0
                        ));
                    } else {
                        ui.label("正在准备...");
                    }
                    ui.add_space(8.0);
                    if ui.button("取消").clicked() {
                        self.migrate_cancel.store(true, Ordering::Relaxed);
                    }
                    ui.add_space(12.0);
                    ui.separator();
                    egui::CollapsingHeader::new("详细信息")
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(160.0)
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    for line in &self.log_lines {
                                        ui.label(line);
                                    }
                                });
                        });
                });
            }
            SetupPhase::Error(e) => {
                let e = e.clone();
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("组件下载失败");
                    ui.add_space(8.0);
                    ui.colored_label(egui::Color32::RED, &e);
                    ui.add_space(12.0);
                    ui.label("请检查网络后重试,或参考 README 手动下载放置对应文件。");
                    if ui.button("重试").clicked() {
                        self.begin_setup_check();
                    }
                });
            }
            SetupPhase::Ready => {
                self.show_main_ui(ui);
            }
        }
    }

    // glow 后端的 App::on_exit 比 wgpu 后端多一个 Option<&glow::Context> 参数(用来在退出前
    // 释放 OpenGL 资源);这里不需要手动清理任何 GL 资源,参数用不上,但签名必须匹配。
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.config.save();
    }
}

impl SubgenApp {
    fn show_main_ui(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("controls").show(ui, |ui| {
            ui.add_space(6.0);
            // 处理中这一整块设置(模型/语言/覆盖策略/翻译开关及其子选项)全部禁用——之前
            // 界面上看着能改,改了也确实存进了 config,但当前这一批任务完全不会用上新设置,
            // 用户会以为"改了=生效了",跑完发现结果和界面显示的设置对不上,是很隐蔽的
            // "设置是摆设"体验。禁用后 hover 会有一层浅灰遮罩,直观地表明"现在不能碰"。
            ui.add_enabled_ui(!self.processing, |ui| {
            ui.horizontal(|ui| {
                ui.label("模型:");
                let mut model_changed = false;
                egui::ComboBox::from_id_salt("model")
                    .selected_text(self.config.model.clone())
                    .show_ui(ui, |ui| {
                        for (key, desc) in Config::AVAILABLE_MODELS {
                            let selected = self.config.model == *key;
                            if ui
                                .selectable_label(selected, format!("{key} - {desc}"))
                                .clicked()
                                && !selected
                            {
                                self.config.model = key.to_string();
                                model_changed = true;
                            }
                        }
                    });
                if model_changed {
                    self.config.save();
                    self.setup_phase = SetupPhase::Checking;
                }

                ui.add_space(16.0);
                ui.label("语言:");
                let mut lang_changed = false;
                egui::ComboBox::from_id_salt("lang")
                    .selected_text(self.config.language.clone())
                    .show_ui(ui, |ui| {
                        for lang in ["auto", "en", "zh", "ja", "ko", "fr", "de", "es"] {
                            if ui
                                .selectable_label(self.config.language == lang, lang)
                                .clicked()
                            {
                                self.config.language = lang.to_string();
                                lang_changed = true;
                            }
                        }
                    });
                if lang_changed {
                    self.config.save();
                }

                ui.add_space(16.0);
                let mut overwrite = self.config.overwrite_existing;
                if ui.checkbox(&mut overwrite, "已有同名字幕时覆盖").changed() {
                    self.config.overwrite_existing = overwrite;
                    self.config.save();
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // 开关/模型切换需要重新检测组件是否已下载(可能要装 llama.cpp + 模型文件),
                // 双语/仅中文纯粹是输出格式,不影响要下载什么,所以单独存 save 不触发重新检测。
                let mut needs_recheck = false;
                let mut translate = self.config.translate;
                if ui.checkbox(&mut translate, "生成中文翻译(本地模型)").changed() {
                    self.config.translate = translate;
                    needs_recheck = true;
                }

                if self.config.translate {
                    ui.add_space(16.0);
                    ui.label("翻译模型:");
                    let current_label = if self.config.translate_model == "auto" {
                        "自动(按视频语言选择)".to_string()
                    } else {
                        translate::model_by_key(&self.config.translate_model)
                            .map(|m| m.label.to_string())
                            .unwrap_or_else(|| self.config.translate_model.clone())
                    };
                    egui::ComboBox::from_id_salt("translate_model")
                        .selected_text(current_label)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(
                                    self.config.translate_model == "auto",
                                    "自动(按视频语言选择)",
                                )
                                .clicked()
                            {
                                self.config.translate_model = "auto".to_string();
                                needs_recheck = true;
                            }
                            for m in translate::MODELS {
                                if ui
                                    .selectable_label(self.config.translate_model == m.key, m.label)
                                    .clicked()
                                {
                                    self.config.translate_model = m.key.to_string();
                                    needs_recheck = true;
                                }
                            }
                        });

                    ui.add_space(16.0);
                    let mut bilingual = self.config.bilingual;
                    if ui.checkbox(&mut bilingual, "双语字幕(中文+原文)").changed() {
                        self.config.bilingual = bilingual;
                        self.config.save();
                    }
                }

                if needs_recheck {
                    self.config.save();
                    self.setup_phase = SetupPhase::Checking;
                }
            });
            }); // add_enabled_ui(!self.processing) 到此结束
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // 常驻显示 + 可改,不止首次下载那一次机会——用了一段时间之后想搬家也应该
                // 随时能改,不应该被困在当初选的位置。处理中禁用:一批任务跑到一半时,
                // 后面文件的翻译阶段会用当时刚查到的模型路径去启动 llama-server,这时候
                // 换目录容易踩"模型文件还没搬过去"的空。
                ui.label(format!("模型位置: {}", paths::models_dir().display()));
                if ui
                    .add_enabled(!self.processing, egui::Button::new("更改..."))
                    .clicked()
                {
                    self.begin_change_models_dir();
                }
                if let Some(e) = &self.models_dir_error {
                    ui.colored_label(egui::Color32::RED, e);
                }
            });
            ui.add_space(6.0);
        });

        egui::Panel::bottom("actions").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("选择视频文件...").clicked() {
                    if let Some(files) = rfd::FileDialog::new()
                        .add_filter("视频", Self::VIDEO_EXTENSIONS)
                        .set_title("选择要生成字幕的视频文件")
                        .pick_files()
                    {
                        self.add_files(files);
                    }
                }
                // 处理中直接置灰禁用,而不是"按钮外观正常、点了却静默无效"——那种写法
                // 会让用户以为按钮坏了,反复点击也得不到任何反馈。
                if ui
                    .add_enabled(!self.processing, egui::Button::new("清空列表"))
                    .clicked()
                {
                    self.jobs.clear();
                }
                ui.add_space(16.0);

                let can_start = !self.jobs.is_empty() && !self.processing;
                if ui
                    .add_enabled(can_start, egui::Button::new("开始处理"))
                    .clicked()
                {
                    self.start_processing();
                }
                // 取消按钮点击后立即变灰、文案改成"正在取消…"——worker 线程要等当前这一步
                // (比如正在跑的 whisper-cli 子进程)结束才会真正检查取消标志,这中间有肉眼
                // 可见的延迟。不给即时反馈的话,用户会以为点击没生效,进而连点甚至直接关窗。
                let cancel_label = if self.cancelling { "正在取消…" } else { "取消" };
                if ui
                    .add_enabled(
                        self.processing && !self.cancelling,
                        egui::Button::new(cancel_label),
                    )
                    .clicked()
                {
                    self.worker_cancel.store(true, Ordering::Relaxed);
                    self.cancelling = true;
                }
            });
            if self.gpu_warning_shown {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 150, 20),
                    "⚠ 检测到处理过程未使用 GPU,速度会非常慢。请检查显卡驱动 / CUDA 环境。",
                );
            }
            if let Some((text, _)) = &self.transient_notice {
                ui.colored_label(egui::Color32::from_rgb(230, 150, 20), text);
            }
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.jobs.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("把视频文件拖到这个窗口,或点下方“选择视频文件”按钮");
                });
                return;
            }
            let translate_enabled = self.config.translate;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for job in &self.jobs {
                    ui.vertical(|ui| {
                        // 第一行:文件名(挤不下时省略号截断,hover 能看到完整名字)+ 步骤条。
                        // 步骤条先画(right_to_left 从右边开始分配空间,把它的宽度先占住),
                        // 文件名最后画、用 truncate() 吃掉剩余空间——不管文件名或步骤条多长,
                        // 两者都不会再互相覆盖(之前的实现两边都不限宽,长文件名+长状态文案
                        // 撞在一起时界面会花掉,这里从根上改了布局顺序解决)。
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                render_stage_stepper(ui, &job.status, translate_enabled);
                                let name = job
                                    .video_path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                ui.add(egui::Label::new(name.clone()).truncate())
                                    .on_hover_text(name);
                            });
                        });
                        // 第二行:详细状态文案,单独占一行,不用再和文件名/步骤条抢空间。
                        // 失败且有对应日志文件时加一个"查看日志"按钮——技术性的报错细节
                        // (退出码、原始 stderr)留在日志文件里,状态栏只给一行人话摘要,
                        // 想深挖的用户一键用记事本打开,不用去 logs/ 文件夹里翻文件名。
                        ui.horizontal(|ui| {
                            let (text, color) = job_status_line(job, self.cancelling);
                            ui.colored_label(color, text);
                            if let JobStatus::Failed(_, _, Some(log_path)) = &job.status {
                                if ui.small_button("查看日志").clicked() {
                                    let _ = std::process::Command::new("notepad")
                                        .arg(log_path)
                                        .spawn();
                                }
                            }
                        });
                    });
                    ui.separator();
                }
            });
        });
    }
}

/// 追加一行日志:既存进界面日志区(`log_lines`),也同步落盘到 `logs/app.log`——`log_lines`
/// 只在内存里,关窗即丢,用户报障时"把 logs 文件夹发我"这句话要成立,日志就得先落盘。
/// 时间戳格式和 crash.log(见 install_panic_logger)保持一致,用 unix 时间戳而不是拼日历
/// 时间,没有额外引入日期时间处理依赖。写盘失败(比如目录还没建出来)静默忽略,这只是个
/// 附加的排障能力,不该反过来影响主流程。
///
/// 写成自由函数、只接收 `&mut Vec<String>` 而不是 `&mut SubgenApp` 方法——调用方常常是
/// 先 `if let Some(rx) = &self.xxx_rx { while ... }` 借着 `rx` 读事件、循环体里再要记日志,
/// 如果写成 `&mut self` 方法,会和外层对 `self.xxx_rx` 的借用冲突(借用检查器认为方法调用
/// 需要整个 `self` 的独占访问权);只借 `log_lines` 这一个字段就没有这个问题。
fn push_log(log_lines: &mut Vec<String>, line: String) {
    let _ = std::fs::create_dir_all(paths::logs_dir());
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::logs_dir().join("app.log"))
    {
        use std::io::Write;
        let _ = writeln!(file, "[unix {timestamp}] {line}");
    }
    log_lines.push(line);
}

/// 把秒数格式化成 "12s" 或 "1分23秒" 这种人可读的形式,用于界面上的"已运行"计时。
fn format_elapsed(total_secs: u64) -> String {
    if total_secs < 60 {
        format!("{total_secs}秒")
    } else {
        let m = total_secs / 60;
        let s = total_secs % 60;
        format!("{m}分{s}秒")
    }
}

/// 把字节数格式化成下载页面看着顺眼的大小单位:≥1GB 显示到 GB(1 位小数),否则显示到
/// MB(1 位小数)。只用于下载组件大小提示,不追求通用场景。
fn format_download_size(bytes: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else {
        format!("{:.1} MB", bytes / MB)
    }
}

/// 画流水线步骤条(比如"✓提取 › ●识别 › ○翻译")。`translate_enabled` 决定要不要画"翻译"
/// 这一步——没开翻译的话流水线只有两步,画第三步没有意义,只会让人以为漏了什么。
/// Pending/Skipped/Cancelled 这几种状态没有明确的"当前处于哪个阶段"可言,不画步骤条,
/// 靠下面单独一行的状态文案说明就够了。
fn render_stage_stepper(ui: &mut egui::Ui, status: &JobStatus, translate_enabled: bool) {
    let stages: &[job::Stage] = if translate_enabled {
        &[
            job::Stage::ExtractAudio,
            job::Stage::Transcribe,
            job::Stage::Translate,
        ]
    } else {
        &[job::Stage::ExtractAudio, job::Stage::Transcribe]
    };

    let (current_ord, is_failed, is_all_done) = match status {
        JobStatus::Running(stage, _, _) => (Some(stage.ordinal()), false, false),
        JobStatus::Failed(stage, _, _) => (Some(stage.ordinal()), true, false),
        JobStatus::Done(_) | JobStatus::DoneWithWarning(..) => (None, false, true),
        _ => return,
    };

    const DONE_COLOR: egui::Color32 = egui::Color32::from_rgb(60, 170, 60);
    const RUNNING_COLOR: egui::Color32 = egui::Color32::from_rgb(90, 150, 230);
    const FAIL_COLOR: egui::Color32 = egui::Color32::RED;
    const PENDING_COLOR: egui::Color32 = egui::Color32::GRAY;

    ui.horizontal(|ui| {
        for (i, stage) in stages.iter().enumerate() {
            let ord = stage.ordinal();
            let (glyph, color) = if is_all_done {
                ("✓", DONE_COLOR)
            } else {
                match current_ord {
                    Some(cur) if ord < cur => ("✓", DONE_COLOR),
                    Some(cur) if ord == cur && is_failed => ("✗", FAIL_COLOR),
                    Some(cur) if ord == cur => ("●", RUNNING_COLOR),
                    _ => ("○", PENDING_COLOR),
                }
            };
            ui.colored_label(color, format!("{glyph}{}", stage.label()));
            if i + 1 < stages.len() {
                ui.label("›");
            }
        }
    });
}

/// 把 ISO 639-1 语言代码翻成用户能看懂的中文名,覆盖界面语言下拉框里的选项(见
/// Config::AVAILABLE_MODELS 旁边那个语言下拉的选项列表:auto/en/zh/ja/ko/fr/de/es)。
/// 没覆盖到的代码原样返回而不是显示"未知"——保留原始信息总比丢掉信息更安全,万一以后
/// whisper 检测出一个列表之外的语言代码,用户至少还能看到那串代码去查。
fn lang_display(code: &str) -> &str {
    match code {
        "auto" => "自动检测",
        "en" => "英语",
        "zh" => "中文",
        "ja" => "日语",
        "ko" => "韩语",
        "fr" => "法语",
        "de" => "德语",
        "es" => "西班牙语",
        other => other,
    }
}

/// 把一次成功处理的 JobReport 拼成"完成"后面跟着的补充信息,比如
/// " · 日语 · Sakura 7B(日→中,轻小说风格)(复用了上次的识别结果)"。让用户在结果不理想时
/// 至少知道用的是哪个语言、哪个翻译模型、这次是不是复用了旧结果——不用被动接受一个孤零零
/// 的"完成"、猜不出问题出在哪个环节。
fn format_job_report_suffix(report: &job::JobReport) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(lang) = &report.detected_language {
        parts.push(lang_display(lang));
    }
    if let Some(model) = report.translate_model_label {
        parts.push(model);
    }
    let mut suffix = if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
    };
    if report.reused_transcript {
        suffix.push_str("(复用了上次的识别结果)");
    }
    suffix
}

/// 生成任务行第二行的详细状态文案 + 配色。`cancelling` 为 true 时(用户点了取消、批次还没
/// 真正停下来),正在跑的那一条任务额外附一句"正在取消,等当前步骤结束"——不然用户点完
/// 取消后看到这一行还在正常显示进度百分比,会以为取消没生效。
fn job_status_line(job: &Job, cancelling: bool) -> (String, egui::Color32) {
    match &job.status {
        JobStatus::Pending => ("等待中".to_string(), egui::Color32::GRAY),
        JobStatus::Running(_, text, pct) => {
            let elapsed = job.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            let elapsed_str = format_elapsed(elapsed);
            let mut text = match pct {
                Some(pct) => format!("{text} {pct:.0}%(已运行 {elapsed_str})"),
                None => format!("{text}(已运行 {elapsed_str})"),
            };
            if cancelling {
                text.push_str("(正在取消,等当前步骤结束)");
            }
            (text, egui::Color32::LIGHT_GRAY)
        }
        JobStatus::Done(report) => {
            if report.empty_subtitle() {
                // 空字幕是合理结果(片段确实没有对白),不是软件坏了——但如果只显示"完成"
                // 两个字,用户在播放器里加载不出字幕文件时,唯一的结论会是"这个软件坏了"。
                (
                    "完成(未检测到对白,字幕为空)".to_string(),
                    egui::Color32::from_rgb(60, 170, 60),
                )
            } else {
                (
                    format!("完成{}", format_job_report_suffix(report)),
                    egui::Color32::from_rgb(60, 170, 60),
                )
            }
        }
        JobStatus::DoneWithWarning(reason, report) => (
            format!("完成,但有提示: {reason}{}", format_job_report_suffix(report)),
            egui::Color32::from_rgb(230, 150, 20),
        ),
        JobStatus::Skipped(reason) => (format!("跳过: {reason}"), egui::Color32::GRAY),
        JobStatus::Failed(stage, reason, _) => (
            format!("{}阶段失败: {reason}", stage.label()),
            egui::Color32::RED,
        ),
        JobStatus::Cancelled => ("已取消".to_string(), egui::Color32::GRAY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_download_size_switches_between_mb_and_gb_at_one_gb() {
        assert_eq!(format_download_size(885_098), "0.8 MB");
        assert_eq!(format_download_size(1_073_741_824), "1.0 GB");
        assert_eq!(format_download_size(3_095_033_483), "2.9 GB");
    }

    /// 用独立的 `Vec<String>` 而不是真的构造一个 `SubgenApp` 来测——`push_log` 就是特意
    /// 为了不需要整个 `&mut self` 才写成自由函数的,测试也应该只依赖它实际需要的那点状态。
    #[test]
    fn push_log_writes_to_app_log_and_in_memory_vec() {
        let app_log = paths::logs_dir().join("app.log");
        let _ = std::fs::remove_file(&app_log);

        let mut lines: Vec<String> = Vec::new();
        push_log(&mut lines, "测试日志行".to_string());

        assert_eq!(lines, vec!["测试日志行".to_string()]);
        let content = std::fs::read_to_string(&app_log).expect("app.log 应该已经被写出来");
        assert!(content.contains("测试日志行"), "app.log 应该包含这行日志: {content}");
        assert!(content.contains("[unix "), "应该带时间戳前缀: {content}");

        let _ = std::fs::remove_file(&app_log);
    }

    #[test]
    fn lang_display_translates_known_codes_and_passes_through_unknown() {
        assert_eq!(lang_display("ja"), "日语");
        assert_eq!(lang_display("auto"), "自动检测");
        assert_eq!(lang_display("xx"), "xx", "没覆盖到的代码应该原样返回,而不是丢掉信息");
    }

    #[test]
    fn format_job_report_suffix_combines_language_model_and_reuse_note() {
        let report = job::JobReport {
            detected_language: Some("ja".to_string()),
            translate_model_label: Some("Sakura 7B(日→中,轻小说风格)"),
            subtitle_count: 42,
            reused_transcript: true,
            repetition_at: None,
        };
        assert_eq!(
            format_job_report_suffix(&report),
            " · 日语 · Sakura 7B(日→中,轻小说风格)(复用了上次的识别结果)"
        );
    }

    #[test]
    fn format_job_report_suffix_empty_when_nothing_to_show() {
        let report = job::JobReport {
            detected_language: None,
            translate_model_label: None,
            subtitle_count: 10,
            reused_transcript: false,
            repetition_at: None,
        };
        assert_eq!(format_job_report_suffix(&report), "");
    }

    #[test]
    fn format_job_report_suffix_shows_language_only_when_translate_not_used() {
        let report = job::JobReport {
            detected_language: Some("en".to_string()),
            translate_model_label: None,
            subtitle_count: 10,
            reused_transcript: false,
            repetition_at: None,
        };
        assert_eq!(format_job_report_suffix(&report), " · 英语");
    }

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
            .write(true)
            .open(&path)
            .expect("应该能创建/打开锁文件");
        first.try_lock().expect("第一个句柄应该能拿到排他锁");

        let second = std::fs::OpenOptions::new()
            .create(true)
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
