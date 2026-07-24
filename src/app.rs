use crate::config::Config;
use crate::format::*;
use crate::job::{self, Job, JobStatus, WorkerEvent};
use crate::paths;
use crate::setup::{self, ComponentKey, SetupEvent};
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
    /// 详情区当前钉住的任务行下标(界面列表原始下标)。None = 自动跟随正在处理的那一个
    /// (单 GPU 串行处理,任何时刻至多一个文件在跑,天然有唯一的默认关注对象,见
    /// `format::active_job_index`);Some(i) = 用户手动点选过某一行,详情区钉住显示它,
    /// 不再跟着处理进度跳动,方便一边继续跑一边查看别的文件。`jobs` 长度变化(移除某行)
    /// 时用 `format::reconcile_selection` 校正,避免指向已经不存在的下标。
    pub(crate) selected: Option<usize>,
    /// "设置"浮窗是否展开。按需弹出而不是常驻顶部——设置是批次之间的低频操作,处理中
    /// 整块常驻却大部分时间不可用,反而是最容易让人困惑的界面状态。
    pub(crate) settings_open: bool,
    /// "组件/存放位置"浮窗是否展开,和设置浮窗是两个独立入口(而不是揉进设置里)——
    /// 模型存放目录这类"环境"信息在处理中依然可能想查看(比如确认这次到底用的是哪个
    /// 目录),不该和"改了要下一批才生效"的设置项混在一起。
    pub(crate) components_open: bool,
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
            selected: None,
            settings_open: false,
            components_open: false,
        }
    }

    pub(crate) fn begin_setup_check(&mut self) {
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

    pub(crate) fn start_downloading(&mut self) {
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
    pub(crate) fn begin_change_models_dir(&mut self) {
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
    pub(crate) const VIDEO_EXTENSIONS: &'static [&'static str] =
        &["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "ts", "m2ts"];

    pub(crate) fn add_files(&mut self, paths: Vec<PathBuf>) {
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

    pub(crate) fn start_processing(&mut self) {
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
            let (done, total) = batch_progress_counts(&self.jobs, &self.batch_indices);
            let current = (done + 1).min(total);
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
                crate::ui::setup_screens::render_checking(ui);
            }
            SetupPhase::NeedsDownload(missing) => {
                let missing = missing.clone();
                crate::ui::setup_screens::render_needs_download(self, ui, &missing);
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
                crate::ui::setup_screens::render_downloading(
                    self,
                    ui,
                    current_label,
                    current_index,
                    total_count,
                    downloaded,
                    total,
                );
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
                crate::ui::setup_screens::render_migrating(
                    self,
                    ui,
                    current_label,
                    current_index,
                    total_count,
                    copied,
                    total,
                );
            }
            SetupPhase::Error(e) => {
                let e = e.clone();
                crate::ui::setup_screens::render_error(self, ui, e);
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
