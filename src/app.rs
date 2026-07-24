use crate::config::Config;
use crate::format::*;
use crate::job::{self, Job, JobStatus, WorkerEvent};
use crate::paths;
use crate::setup::{self, ComponentKey, SetupEvent};
use crate::translate;
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

pub(crate) enum SetupPhase {
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

pub struct SubgenApp {
    pub(crate) config: Config,
    pub(crate) jobs: Vec<Job>,
    pub(crate) setup_phase: SetupPhase,
    pub(crate) setup_rx: Option<mpsc::Receiver<SetupEvent>>,
    pub(crate) setup_cancel: Arc<AtomicBool>,
    pub(crate) worker_rx: Option<mpsc::Receiver<WorkerEvent>>,
    pub(crate) worker_cancel: Arc<AtomicBool>,
    pub(crate) processing: bool,
    /// 用户点了"取消"、但批次还没真正停下来(worker 线程要等当前这一步跑完才会检查取消
    /// 标志)。只用来驱动界面反馈——按钮立刻变灰显示"正在取消…"、当前任务状态行附加提示,
    /// 不然用户会因为点击后界面毫无变化而怀疑按钮没反应,进而连点或直接关窗强退。
    pub(crate) cancelling: bool,
    /// 本批次里,哪些阶段(识别/翻译)被观测到没用上 GPU——用一个集合而不是单个 bool,
    /// 是因为识别和翻译是两个独立检测的阶段,警告文案要能说清具体是哪一步慢,不是笼统
    /// 一句"没用上 GPU"。
    pub(crate) gpu_warning_stages: std::collections::HashSet<job::Stage>,
    pub(crate) log_lines: Vec<String>,
    pub(crate) migrate_rx: Option<mpsc::Receiver<setup::MigrateEvent>>,
    pub(crate) migrate_cancel: Arc<AtomicBool>,
    /// 搬迁线程跑完后要提交成生效目录的目标路径;None 说明当前没有搬迁在进行。
    pub(crate) pending_models_dir: Option<PathBuf>,
    /// 更改模型目录时的校验错误(目录不可写等),显示在"更改..."按钮旁边,下次操作前清空。
    pub(crate) models_dir_error: Option<String>,
    /// 处理中用户点了窗口的关闭按钮,弹出"确定退出吗"确认弹窗时为 true。
    pub(crate) close_confirm_open: bool,
    /// 用户在确认弹窗里点了"确定退出"——下一次(或本次)收到关闭请求时不再拦截,真正放行。
    /// 没有这个标志,程序发出的 ViewportCommand::Close 如果也会产生一次 close_requested()
    /// 事件,会被同一段"处理中就拦截"的逻辑再次误拦,变成怎么点都关不掉。
    pub(crate) allow_close: bool,
    /// 处理中拖入文件时给出的提示,连同显示到期时间一起存着,到点了在下一帧渲染前自动清掉
    /// (不需要用户手动关闭这条提示)。
    pub(crate) transient_notice: Option<(String, std::time::Instant)>,
    /// 当前批次里(界面列表原始下标)包含哪些任务,用来算窗口标题上的"第 N/M 个"——用户
    /// 切到别的窗口干活时,任务栏悬停就能看到大致进度,不用切回来才知道跑到哪了。
    pub(crate) batch_indices: Vec<usize>,
    /// 上一次写进标题的进度,值没变就不重复调用 send_viewport_cmd(避免每帧都发一次
    /// 相同的标题更新)。
    pub(crate) last_title_progress: Option<(usize, usize)>,
    /// 当前批次开始处理的时刻,用来在结束时算总耗时(和单个任务的 started_at 是两码事)。
    pub(crate) batch_started_at: Option<std::time::Instant>,
    /// 上一批结束时生成的汇总文案("17 个成功、2 个建议核对、1 个失败"这种),下一批开始
    /// 时清空——批次跑完前用户看到的应该是上一批的结果还是空白,不该是半新不旧的状态。
    pub(crate) last_summary: Option<String>,
}

impl SubgenApp {
    pub fn new(config: Config) -> Self {
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
            gpu_warning_stages: std::collections::HashSet::new(),
            log_lines: Vec::new(),
            migrate_rx: None,
            migrate_cancel: Arc::new(AtomicBool::new(false)),
            pending_models_dir: None,
            models_dir_error: None,
            close_confirm_open: false,
            allow_close: false,
            transient_notice: None,
            batch_indices: Vec::new(),
            last_title_progress: None,
            batch_started_at: None,
            last_summary: None,
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
        // (之前的实现会把所有任务的状态重置成 Pending 再全部重跑)。除非用户在任务行点了
        // "重新处理"(force_reprocess)——那种情况即使是 Done/DoneWithWarning 终态也要
        // 纳入这一批,并且真正重新跑一遍,不是复用旧结果。
        let indices: Vec<usize> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, j)| should_include_in_batch(j))
            .map(|(i, _)| i)
            .collect();
        if indices.is_empty() {
            return;
        }

        self.processing = true;
        self.cancelling = false;
        self.gpu_warning_stages.clear();
        self.worker_cancel = Arc::new(AtomicBool::new(false));
        self.batch_indices = indices.clone();
        self.last_title_progress = None;
        self.batch_started_at = Some(std::time::Instant::now());
        self.last_summary = None;
        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);

        let batch: Vec<(usize, PathBuf, bool)> = indices
            .iter()
            .map(|&i| (i, self.jobs[i].video_path.clone(), self.jobs[i].force_reprocess))
            .collect();
        for &i in &indices {
            self.jobs[i].status = JobStatus::Pending;
            self.jobs[i].started_at = None;
            // 入批后复位——这不是"以后每次都强制重跑"的持久开关,只对这一次生效。
            self.jobs[i].force_reprocess = false;
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
        let mut gpu_missing_stages: Vec<job::Stage> = Vec::new();
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
                    WorkerEvent::GpuMissing(stage) => gpu_missing_stages.push(stage),
                    WorkerEvent::BatchDone => batch_done = true,
                }
            }
        }
        self.gpu_warning_stages.extend(gpu_missing_stages);
        // 标题带上"第 N/M 个"——用户切到别的窗口干活时,任务栏悬停就能看到大致进度,
        // 不用切回来才知道跑到哪了。只在进度真的变化时才发 Title 命令,不是每帧都发。
        if self.processing && !self.batch_indices.is_empty() {
            let total = self.batch_indices.len();
            let terminal_count = self
                .batch_indices
                .iter()
                .filter(|&&i| self.jobs.get(i).is_some_and(|j| j.status.is_terminal()))
                .count();
            let current = (terminal_count + 1).min(total);
            if self.last_title_progress != Some((current, total)) {
                self.last_title_progress = Some((current, total));
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                    "{} [{current}/{total}]",
                    crate::window_title()
                )));
            }
        }
        if batch_done {
            self.processing = false;
            self.cancelling = false;
            self.worker_rx = None;
            self.last_title_progress = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(crate::window_title()));

            let elapsed = self
                .batch_started_at
                .map(|t| t.elapsed())
                .unwrap_or_default();
            self.last_summary = Some(build_batch_summary(&self.jobs, &self.batch_indices, elapsed));
            // 用户跑一批 20 集去干别的事了,批次结束时闪一下任务栏——不用非得切回来才知道
            // 跑完了。Informational 是"标准程度的提醒"而不是 Critical(那个更适合真正
            // 紧急的场景),批次完成不算紧急,不需要用最抓眼球的那一档。
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
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
    pub fn show_main_ui(&mut self, ui: &mut egui::Ui) {
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
                // 下拉框显示中文名(lang_display,和完成状态行回显用的是同一个函数,保持
                // 界面上语言名称的说法前后一致),存进 config 的还是 ISO 码,不影响存储格式。
                egui::ComboBox::from_id_salt("lang")
                    .selected_text(lang_display(&self.config.language))
                    .show_ui(ui, |ui| {
                        for lang in ["auto", "en", "zh", "ja", "ko", "fr", "de", "es"] {
                            if ui
                                .selectable_label(self.config.language == lang, lang_display(lang))
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
                // "本地模型"是给开发者看的实现细节;新手真正关心的是"要不要联网、内容
                // 会不会被传出去",直接把这个卖点说出来比暗示"用了本地模型"更直接。
                if ui.checkbox(&mut translate, "翻译成中文(在本机运行,不上传内容)").changed() {
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
            if let Some(summary) = &self.last_summary {
                ui.label(summary);
            }
            if !self.gpu_warning_stages.is_empty() {
                // 按流水线顺序(识别在前、翻译在后)拼出受影响的阶段名,而不是笼统一句
                // "没用上 GPU"——识别和翻译是分别独立检测的,用户应该知道具体是哪一步慢。
                // "CUDA 环境"对不知道自己有没有独立显卡的用户是天书,给出的建议要么可执行
                // (更新驱动),要么直接告诉他们"你可能就是没有这个硬件",别让人对着一句
                // 看不懂的话不知道下一步该做什么。
                let mut stages: Vec<job::Stage> = self.gpu_warning_stages.iter().copied().collect();
                stages.sort_by_key(|s| s.ordinal());
                let stages_text = stages
                    .iter()
                    .map(|s| s.label())
                    .collect::<Vec<_>>()
                    .join("和");
                ui.colored_label(
                    egui::Color32::from_rgb(230, 150, 20),
                    format!(
                        "⚠ {stages_text}阶段未能使用显卡加速,处理会慢很多。请到 NVIDIA 官网\
                         更新显卡驱动后重试;如果这台电脑没有 NVIDIA 显卡,只能使用较慢的模式。"
                    ),
                );
            }
            if let Some((text, _)) = &self.transient_notice {
                ui.colored_label(egui::Color32::from_rgb(230, 150, 20), text);
            }
            ui.add_space(6.0);
        });

        // 单文件操作(移除/重新处理/打开文件夹/展开预览)在渲染循环里只读取 self.jobs,
        // 真正的修改(可能触发 Vec::remove 导致下标错位)收集到这几个变量里,渲染循环结束
        // 后统一应用——不在遍历 self.jobs 的同时又去修改它。
        let mut remove_index: Option<usize> = None;
        let mut reprocess_index: Option<usize> = None;
        let mut open_folder_path: Option<PathBuf> = None;
        let mut toggle_preview_index: Option<usize> = None;

        egui::CentralPanel::default().show(ui, |ui| {
            if self.jobs.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("把视频文件拖到这个窗口,或点下方“选择视频文件”按钮");
                });
                return;
            }
            let translate_enabled = self.config.translate;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, job) in self.jobs.iter().enumerate() {
                    ui.vertical(|ui| {
                        // 第一行:文件名(挤不下时省略号截断,hover 能看到完整名字)+ 步骤条。
                        // 步骤条先画(right_to_left 从右边开始分配空间,把它的宽度先占住),
                        // 文件名最后画、用 truncate() 吃掉剩余空间——不管文件名或步骤条多长,
                        // 两者都不会再互相覆盖(之前的实现两边都不限宽,长文件名+长状态文案
                        // 撞在一起时界面会花掉,这里从根上改了布局顺序解决)。
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                crate::render_stage_stepper(ui, &job.status, translate_enabled);
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
                            // 正在跑的任务前面加个转圈动画——纯文字状态在长时间不变的等待期
                            // (比如提取音轨、翻译模型加载)看着和"卡死了"没有视觉区别,
                            // 一个持续在动的图形元素能立刻打消这个疑虑,不用非得等百分比更新。
                            if matches!(job.status, JobStatus::Running(..)) {
                                ui.spinner();
                            }
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
                        // 第三行:单文件操作按钮。处理中全部禁用——"移除"会让 Vec 下标
                        // 错位,worker 线程发回来的 JobUpdate{index, ..} 是按批次开始时
                        // 固定下来的下标算的,处理中改动列表会让后续状态更新对错行;
                        // "重新处理"/预览虽然本身不涉及下标错位,但处理中点这些于用户
                        // 而言意图也不明确(现在改会不会影响正在跑的这一批?),干脆和
                        // "移除"一起统一禁用,行为更容易预期。
                        ui.add_enabled_ui(!self.processing, |ui| {
                            ui.horizontal(|ui| {
                                if ui.small_button("移除").clicked() {
                                    remove_index = Some(i);
                                }
                                if job.status.is_terminal() {
                                    if ui.small_button("重新处理").clicked() {
                                        reprocess_index = Some(i);
                                    }
                                }
                                if ui.small_button("打开文件夹").clicked() {
                                    open_folder_path = Some(reveal_path_for(job));
                                }
                                if matches!(job.status, JobStatus::Done(_) | JobStatus::DoneWithWarning(..))
                                {
                                    let label = if job.preview_open { "收起预览" } else { "预览" };
                                    if ui.small_button(label).clicked() {
                                        toggle_preview_index = Some(i);
                                    }
                                }
                            });
                        });
                        if job.preview_open {
                            crate::render_subtitle_preview(ui, job);
                        }
                    });
                    ui.separator();
                }
            });
        });

        if let Some(i) = remove_index {
            self.jobs.remove(i);
        }
        if let Some(i) = reprocess_index {
            if let Some(j) = self.jobs.get_mut(i) {
                j.force_reprocess = true;
            }
        }
        if let Some(path) = open_folder_path {
            let _ = std::process::Command::new("explorer")
                .arg(format!("/select,{}", path.display()))
                .spawn();
        }
        if let Some(i) = toggle_preview_index {
            if let Some(j) = self.jobs.get_mut(i) {
                j.preview_open = !j.preview_open;
            }
        }
    }
}
