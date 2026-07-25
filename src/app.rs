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
    /// 详情区当前显示的翻译复核报告(见 report.rs)。只保存最近一次生成的这一份,而不是
    /// 挂在 Job 结构体上长期持有——报告文本可能有几十 KB,只有正在查看的这一份需要留在
    /// 内存里,切换到别的任务或重新生成会覆盖它。
    pub(crate) review: Option<ReviewReportState>,
    /// 模型清单界面"定位…"选中一个候选文件后,正在后台校验哈希的那一项。同一时刻只有
    /// 一个在跑(用户点了另一行的"定位…"会覆盖它,前一个后台线程发现自己被取代不会再有
    /// 人收它的结果,静默结束——不需要显式互斥,`pending_verify` 被覆盖后旧的 `rx`/`cancel`
    /// 直接被丢弃,线程发送失败的 `Err` 会被忽略)。
    pub(crate) pending_verify: Option<PendingVerify>,
    /// 模型清单里点"下载"正在装的那一项(同一时刻只有一个,和 `pending_verify` 同一种
    /// 单槽设计)。见 `begin_inline_install`/`poll_inline_install`。
    pub(crate) inline_install: Option<InlineInstall>,
    /// "扫描本机已有模型"结果:绑定表 key -> 候选文件路径。只按文件名+大小匹配,不做
    /// 哈希(扫描要遍历用户可能几十 GB 的共享缓存目录,哈希每个候选太慢)——真正的哈希
    /// 校验留到用户点"使用此文件"时,复用 `begin_locate_model` 同一条校验+绑定路径。
    /// `None` = 还没扫描过或正在扫描中,`Some(空表)` = 扫描完成但什么都没找到,两者在
    /// 界面上该显示的文案不一样("点这里扫一下" vs "没找到候选")。
    pub(crate) scan_results: Option<std::collections::BTreeMap<String, PathBuf>>,
    /// 同一趟扫描顺带找到的"存放目录里同一个文件名不止一份"的记录:绑定表 key -> 除了
    /// 正在用的那份之外,还找到的其它路径。只探测不动手,用户点"清理"才会真的移动文件
    /// (见 `begin_cleanup_duplicates`)。
    pub(crate) dup_results: Option<std::collections::BTreeMap<String, Vec<PathBuf>>>,
    #[allow(clippy::type_complexity)]
    pub(crate) scan_rx: Option<
        mpsc::Receiver<(
            std::collections::BTreeMap<String, PathBuf>,
            std::collections::BTreeMap<String, Vec<PathBuf>>,
        )>,
    >,
    /// "清理"重复文件正在跑的那一项(同一时刻只有一个,和 `pending_verify`/`inline_install`
    /// 同一种单槽设计)。
    pub(crate) pending_cleanup: Option<PendingCleanup>,
}

/// 见 `SubgenApp::pending_cleanup` 字段说明。
pub(crate) struct PendingCleanup {
    pub binding_key: String,
    pub label: String,
    /// `(移动成功的数量, 每一处没能清理的说明)`。
    pub rx: mpsc::Receiver<(usize, Vec<String>)>,
    pub cancel: Arc<AtomicBool>,
}

/// 见 `SubgenApp::pending_verify` 字段说明。
pub(crate) struct PendingVerify {
    pub binding_key: String,
    pub label: String,
    pub path: PathBuf,
    /// `Ok(true)` = 哈希匹配,应该绑定;`Ok(false)` = 哈希不匹配;`Err` = 读取失败或被取消。
    pub rx: mpsc::Receiver<Result<bool, String>>,
    pub cancel: Arc<AtomicBool>,
}

/// 见 `SubgenApp::inline_install` 字段说明。
pub(crate) struct InlineInstall {
    pub key: ComponentKey,
    pub label: String,
    pub rx: mpsc::Receiver<SetupEvent>,
    pub cancel: Arc<AtomicBool>,
    pub downloaded: u64,
    pub total: u64,
}

/// 见 `SubgenApp::review` 字段说明。
pub(crate) struct ReviewReportState {
    /// 这份报告对应界面列表里的哪个任务(原始下标)——用户切换选中的任务后,详情区不该
    /// 继续显示上一个任务的报告。
    pub job_index: usize,
    pub text: String,
    /// 生成报告时真正被粗筛标记为可疑的行下标(0-based)。粘贴修正回灌时只接受这些下标,
    /// 不认识的编号一律忽略——不然一次误粘贴就可能覆盖没有问题的行。
    pub suspect_indices: Vec<usize>,
    pub saved_path: PathBuf,
    /// "粘贴修正"输入框的内容。
    pub correction_input: String,
    /// 上一次点"应用修正"的结果文案(成功应用了几处/被忽略了哪些编号),None 表示还没点过。
    pub correction_result: Option<String>,
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
            review: None,
            pending_verify: None,
            inline_install: None,
            scan_results: None,
            dup_results: None,
            scan_rx: None,
            pending_cleanup: None,
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
                // 绑定表存的是绝对路径,文件物理搬走后旧路径就失效了——在切换生效目录之前
                // (还能读到旧的 models_dir())把指向旧目录的绑定改写到新目录,不然界面会把
                // 明明装着的模型显示成"绑定的文件已不在原位置"。
                let old_dir = paths::models_dir();
                let files_present_in_new_dir: std::collections::HashSet<String> = std::fs::read_dir(&new_dir)
                    .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
                    .unwrap_or_default();
                self.config.model_files = setup::rewrite_bindings_after_migration(
                    &self.config.model_files,
                    &old_dir,
                    &new_dir,
                    &files_present_in_new_dir,
                );
                // 立刻同步进程级绑定表——不然改写只在 config.toml 里生效,这一次运行期间
                // 界面读到的还是旧路径,得等重启才能看到效果。
                paths::set_model_bindings(
                    self.config.model_files.iter().map(|(k, v)| (k.clone(), PathBuf::from(v))).collect(),
                );
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
        // 开工前预检:复用 missing_components 同一份"这批真正需要什么"的判断(B8 已经
        // relax 过——"auto" 只要求兜底翻译模型),缺了就老实说清楚缺什么,不开工。不这样
        // 做的话,20 个文件的批次可能在第 1 个文件语音识别跑完之后,翻译阶段才发现模型没装,
        // 白白浪费识别那几分钟,而且用户设置改动已经不再触发整屏检查(见 B8),这里是
        // 唯一还会主动拦一下的地方。
        let missing = setup::missing_components(&self.config.model, self.config.translate, &self.config.translate_model);
        if !missing.is_empty() {
            let names: Vec<&str> = missing.iter().map(|k| k.label()).collect();
            self.transient_notice = Some((
                format!(
                    "还差这些组件没装好,暂时无法开始:{}(可以在“组件与存放位置”里查看/下载)",
                    names.join("、")
                ),
                std::time::Instant::now(),
            ));
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

    /// 生成 `job_index` 对应任务的翻译复核报告:读原文/译文两份 srt、跑粗筛、格式化成文本、
    /// 写到视频旁的 `.zh.review.txt`,同时存进 `self.review` 供详情面板显示/复制。
    ///
    /// 中文字幕文件可能是双语的("中文\n原文"两行一块)也可能是仅中文单行——不管哪种,
    /// 写出时中文永远是每块的第一行(见 `translate::build_output_blocks`),所以这里用
    /// `parse_srt_blocks_lines` 取每块的行数组、只拿第一行,而不是 `parse_srt_blocks`
    /// (它会把多行块用空格拼接,双语文件会把中文和原文拼成一坨、失去边界)。
    pub(crate) fn generate_review_report(&mut self, job_index: usize) {
        let Some(job) = self.jobs.get(job_index) else {
            return;
        };
        let video_path = job.video_path.clone();
        let original_path = paths::sidecar_srt_path(&video_path);
        let zh_path = paths::sidecar_srt_lang_path(&video_path, "zh");
        let label = video_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        let original_content = match std::fs::read_to_string(&original_path) {
            Ok(c) => c,
            Err(e) => {
                self.transient_notice =
                    Some((format!("找不到原文字幕: {e}"), std::time::Instant::now()));
                return;
            }
        };
        let zh_content = match std::fs::read_to_string(&zh_path) {
            Ok(c) => c,
            Err(e) => {
                self.transient_notice =
                    Some((format!("找不到中文字幕: {e}"), std::time::Instant::now()));
                return;
            }
        };

        let original_blocks = crate::engine::parse_srt_blocks(&original_content);
        let zh_blocks: Vec<(String, String, String)> = crate::engine::parse_srt_blocks_lines(&zh_content)
            .into_iter()
            .map(|(start, end, lines)| (start, end, lines.into_iter().next().unwrap_or_default()))
            .collect();

        let suspects = match crate::report::detect_suspects(&original_blocks, &zh_blocks) {
            Ok(s) => s,
            Err(e) => {
                self.transient_notice = Some((e, std::time::Instant::now()));
                return;
            }
        };
        let suspect_indices: Vec<usize> = suspects.iter().map(|s| s.index).collect();
        let text = crate::report::format_review_report(&label, &original_blocks, &zh_blocks, &suspects);

        let saved_path = paths::sidecar_review_path(&video_path);
        let _ = std::fs::write(&saved_path, &text);

        self.review = Some(ReviewReportState {
            job_index,
            text,
            suspect_indices,
            saved_path,
            correction_input: String::new(),
            correction_result: None,
        });
    }

    /// 把用户粘贴回来的编号行("129. 修正后的译文")应用到对应任务的 `.zh.srt`。只接受
    /// 这次生成报告时真正标记为可疑的下标(`self.review.suspect_indices`),不认识的编号
    /// 一律忽略并在结果文案里列出来——不该让一次误粘贴覆盖到没有被标记过的行。
    ///
    /// 双语字幕每块有"中文\n原文"两行,只替换第一行(中文),保留后面的原文行不动;
    /// 原文件先备份成 `.zh.srt.bak` 再覆盖写入。
    pub(crate) fn apply_review_corrections(&mut self, job_index: usize) {
        let Some(review) = &self.review else {
            return;
        };
        if review.job_index != job_index {
            return;
        }
        let pasted = review.correction_input.clone();
        let allowed: std::collections::HashSet<usize> =
            review.suspect_indices.iter().copied().collect();

        let Some(job) = self.jobs.get(job_index) else {
            return;
        };
        let zh_path = paths::sidecar_srt_lang_path(&job.video_path, "zh");

        let zh_content = match std::fs::read_to_string(&zh_path) {
            Ok(c) => c,
            Err(e) => {
                if let Some(r) = &mut self.review {
                    r.correction_result = Some(format!("读取中文字幕失败: {e}"));
                }
                return;
            }
        };
        let mut blocks = crate::engine::parse_srt_blocks_lines(&zh_content);

        let mut applied = 0usize;
        let mut ignored: Vec<usize> = Vec::new();
        for line in pasted.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((num, text)) = crate::translate::split_leading_number(line) else {
                continue;
            };
            let index = num.saturating_sub(1);
            if !allowed.contains(&index) {
                ignored.push(num);
                continue;
            }
            if let Some((_, _, lines)) = blocks.get_mut(index) {
                if lines.is_empty() {
                    lines.push(text.to_string());
                } else {
                    lines[0] = text.to_string();
                }
                applied += 1;
            }
        }

        if applied == 0 {
            if let Some(r) = &mut self.review {
                r.correction_result = Some(if ignored.is_empty() {
                    "没有识别到任何可应用的修正行(格式应为“编号. 修正后的译文”)".to_string()
                } else {
                    format!(
                        "没有应用任何修正——这些编号不在本次报告标记的可疑行范围内: {}",
                        ignored.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("、")
                    )
                });
            }
            return;
        }

        let backup_path = PathBuf::from(format!("{}.bak", zh_path.display()));
        let _ = std::fs::write(&backup_path, &zh_content);

        let out_blocks: Vec<(String, String, String)> = blocks
            .into_iter()
            .map(|(start, end, lines)| (start, end, lines.join("\n")))
            .collect();
        let _ = std::fs::write(&zh_path, crate::engine::write_srt_blocks(&out_blocks));

        let msg = if ignored.is_empty() {
            format!("已应用 {applied} 处修正(原文件已备份为 .bak)")
        } else {
            format!(
                "已应用 {applied} 处修正(原文件已备份为 .bak);忽略了不在标记范围内的编号: {}",
                ignored.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("、")
            )
        };
        if let Some(r) = &mut self.review {
            r.correction_result = Some(msg);
            r.correction_input.clear();
        }
    }

    /// "定位…"按钮的入口:弹出文件选择框,选中后交给 `begin_verify_candidate` 做粗筛+
    /// 哈希校验。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_locate_model(
        &mut self,
        binding_key: String,
        label: String,
        expected_kind: crate::models::FileKind,
        expected_size: u64,
        expected_sha256: &'static str,
        extension_filter: &'static str,
    ) {
        let Some(picked) = rfd::FileDialog::new()
            .add_filter(&label, &[extension_filter])
            .pick_file()
        else {
            return; // 用户在选择框里按了取消
        };
        self.begin_verify_candidate(binding_key, label, picked, expected_kind, expected_size, expected_sha256);
    }

    /// 给定一个候选路径(来自文件选择框,或者"扫描本机已有模型"找到的候选),先做便宜的
    /// 类型+大小粗筛(读文件开头 4 字节魔数 + 元数据,不用起线程),粗筛通过才起后台线程
    /// 算哈希(见 `poll_verify`)——几 GB 的文件哈希要算几十秒,不能挡住 UI 线程。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_verify_candidate(
        &mut self,
        binding_key: String,
        label: String,
        candidate: PathBuf,
        expected_kind: crate::models::FileKind,
        expected_size: u64,
        expected_sha256: &'static str,
    ) {
        let actual_size = match std::fs::metadata(&candidate) {
            Ok(m) => m.len(),
            Err(e) => {
                self.transient_notice = Some((format!("读取文件信息失败: {e}"), std::time::Instant::now()));
                return;
            }
        };
        let magic = match read_magic(&candidate) {
            Ok(m) => m,
            Err(e) => {
                self.transient_notice = Some((format!("读取文件失败: {e}"), std::time::Instant::now()));
                return;
            }
        };

        match crate::models::classify_picked_file(magic, actual_size, expected_kind, expected_size) {
            crate::models::PickOutcome::WrongKind => {
                self.transient_notice = Some((
                    "这个文件看起来不是同一类模型,已忽略(只按文件头判断类型,不解析内容细节)"
                        .to_string(),
                    std::time::Instant::now(),
                ));
            }
            crate::models::PickOutcome::SizeMismatch { expected, actual } => {
                self.transient_notice = Some((
                    format!(
                        "文件大小不对(期望 {},实际 {}),已忽略",
                        format_download_size(expected),
                        format_download_size(actual)
                    ),
                    std::time::Instant::now(),
                ));
            }
            crate::models::PickOutcome::Plausible => {
                let cancel = Arc::new(AtomicBool::new(false));
                let (tx, rx) = mpsc::channel();
                let cancel_thread = cancel.clone();
                let path_thread = candidate.clone();
                std::thread::spawn(move || {
                    let result = setup::sha256_file_cancelable(&path_thread, &cancel_thread)
                        .map(|digest| setup::to_hex(&digest).eq_ignore_ascii_case(expected_sha256));
                    let _ = tx.send(result);
                });
                self.pending_verify = Some(PendingVerify { binding_key, label, path: candidate, rx, cancel });
            }
        }
    }

    /// 命中本地已有文件、决定绑定而不是复制/下载(见 `setup::install_or_bind`)——写进
    /// config 才能跨进程记住,同步进程级绑定表才能这一轮立刻生效,不用等重启。两个来源
    /// 共用:首次运行的整屏下载流程(`SetupEvent::ModelBound`)和清单里的"下载"内联安装
    /// (`poll_inline_install`)。push_log 而不是静默处理——这是"省下了一次几 GB 下载"的
    /// 用户可见事件,值得留痕。
    fn apply_model_bound(&mut self, key: String, path: PathBuf) {
        self.config.model_files.insert(key, path.to_string_lossy().to_string());
        self.config.save();
        paths::set_model_bindings(
            self.config.model_files.iter().map(|(k, v)| (k.clone(), PathBuf::from(v))).collect(),
        );
        push_log(&mut self.log_lines, format!("已绑定到本地已有文件: {}", path.display()));
    }

    /// 模型清单里点"下载"的入口——不进整屏的 `SetupPhase::Downloading` 流程,单独起一个
    /// 后台线程装这一个组件,用同一套 `setup::install_component`/`SetupEvent`,只是消费方
    /// 换成 `poll_inline_install`。这样用户在设置里调整了某个用不上的选项(比如换了个还没
    /// 下载的翻译模型)不会被强制拖回整屏下载页,只在真的想要那个模型时,就地点一下就装。
    pub(crate) fn begin_inline_install(&mut self, key: ComponentKey, label: String, whisper_model: String) {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let cancel_thread = cancel.clone();
        std::thread::spawn(move || {
            match setup::install_component(key, &whisper_model, &tx, &cancel_thread) {
                Ok(()) => {
                    let _ = tx.send(SetupEvent::AllReady);
                }
                Err(e) => {
                    let _ = tx.send(SetupEvent::Failed(e));
                }
            }
        });
        self.inline_install = Some(InlineInstall { key, label, rx, cancel, downloaded: 0, total: 0 });
    }

    fn poll_inline_install(&mut self, ctx: &egui::Context) {
        let mut logs: Vec<String> = Vec::new();
        let mut bound: Option<(String, PathBuf)> = None;
        let mut progress: Option<(u64, u64)> = None;
        let mut finished: Option<Result<(), String>> = None;
        if let Some(inst) = &self.inline_install {
            while let Ok(ev) = inst.rx.try_recv() {
                match ev {
                    SetupEvent::Log(l) => logs.push(l),
                    SetupEvent::Progress { downloaded, total, .. } => progress = Some((downloaded, total)),
                    SetupEvent::ModelBound { key, path } => bound = Some((key, path)),
                    SetupEvent::ComponentReady(_) | SetupEvent::AllReady => finished = Some(Ok(())),
                    SetupEvent::Failed(e) => finished = Some(Err(e)),
                }
            }
        }
        for l in logs {
            push_log(&mut self.log_lines, l);
        }
        if let Some((d, t)) = progress
            && let Some(inst) = &mut self.inline_install
        {
            inst.downloaded = d;
            inst.total = t;
        }
        if let Some((key, path)) = bound {
            self.apply_model_bound(key, path);
        }
        if let Some(result) = finished {
            let label = self.inline_install.take().map(|i| i.label).unwrap_or_default();
            self.transient_notice = Some((
                match &result {
                    Ok(()) => format!("{label} 安装完成"),
                    Err(e) => format!("{label} 安装失败: {e}"),
                },
                std::time::Instant::now(),
            ));
        }
        if self.inline_install.is_some() {
            ctx.request_repaint();
        }
    }

    /// "扫描本机已有模型"入口。做两件事,同一趟遍历顺带做,不需要用户分别点两个按钮:
    /// (1) 后台遍历已知的外部缓存位置(HF_HOME/HF_HUB_CACHE/`~/.cache/huggingface/hub`,
    /// 见 `setup::find_in_known_roots`),按(文件名, 大小)粗筛候选——不做哈希,扫描要遍历
    /// 用户可能几十 GB 的共享缓存目录,把每个候选都哈希一遍太慢。真正认定"是不是这个模型"
    /// 留到用户点"使用此文件"时,复用 `begin_locate_model` 同一条哈希校验+绑定路径。
    /// (2) 在存放目录树里找同一个文件名是否存在不止一份(见 `paths::find_all_matching_files`)
    /// ——这是"清理重复"(用户明确要求"先不去重,方案完善好再删除",这里只探测、只在
    /// 用户点"清理"时才真的动文件,见 `begin_cleanup_duplicates`)的前置扫描。
    pub(crate) fn begin_scan_for_models(&mut self) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut found: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
            let mut duplicates: std::collections::BTreeMap<String, Vec<PathBuf>> = std::collections::BTreeMap::new();
            let mut check = |binding_key: String, file_name: &str, expected_size: u64, kept: Option<PathBuf>| {
                if let Some(p) = setup::find_in_known_roots(file_name)
                    && std::fs::metadata(&p).map(|m| m.len()).ok() == Some(expected_size)
                {
                    found.insert(binding_key.clone(), p);
                }
                let all = paths::find_all_matching_files(&paths::models_dir(), file_name);
                let extra: Vec<PathBuf> = all.into_iter().filter(|p| Some(p) != kept.as_ref()).collect();
                if !extra.is_empty() {
                    duplicates.insert(binding_key, extra);
                }
            };
            for wm in crate::models::whisper::MODELS {
                check(
                    crate::models::whisper_binding_key(wm.key),
                    wm.file_name,
                    wm.size_bytes,
                    Some(paths::model_path(wm.key)),
                );
            }
            check(
                crate::models::VAD_BINDING_KEY.to_string(),
                crate::models::VAD_FILE_NAME,
                crate::models::VAD_SIZE_BYTES,
                Some(paths::vad_model_path()),
            );
            for tm in crate::translate::MODELS {
                check(
                    crate::models::trans_binding_key(tm.key),
                    tm.file_name,
                    tm.size_bytes,
                    Some(paths::trans_model_path(tm.key, tm.file_name)),
                );
            }
            let _ = tx.send((found, duplicates));
        });
        self.scan_rx = Some(rx);
    }

    fn poll_scan(&mut self, ctx: &egui::Context) {
        let mut result = None;
        if let Some(rx) = &self.scan_rx
            && let Ok(r) = rx.try_recv()
        {
            result = Some(r);
        }
        if let Some((found, duplicates)) = result {
            self.scan_rx = None;
            self.scan_results = Some(found);
            self.dup_results = Some(duplicates);
        }
        if self.scan_rx.is_some() {
            ctx.request_repaint();
        }
    }

    /// "清理"按钮的入口:对某个模型已知的重复文件逐个哈希校验(只处理确认过哈希和期望值
    /// 一致的副本——用户要求"先不去重",这里再加一层保险,不匹配的文件原样不动),验证
    /// 通过的移到 `models_dir/_trash/`(软删除,不是真删,用户想反悔还能从那里找回来)。
    pub(crate) fn begin_cleanup_duplicates(
        &mut self,
        binding_key: String,
        label: String,
        candidates: Vec<PathBuf>,
        expected_sha256: &'static str,
    ) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = cancel.clone();
        std::thread::spawn(move || {
            let trash_dir = paths::models_dir().join("_trash");
            let _ = std::fs::create_dir_all(&trash_dir);
            let already_in_trash: std::collections::HashSet<String> = std::fs::read_dir(&trash_dir)
                .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
                .unwrap_or_default();
            let mut moved = 0usize;
            let mut skipped: Vec<String> = Vec::new();
            let mut trash_names = already_in_trash;
            for candidate in candidates {
                if cancel_thread.load(Ordering::Relaxed) {
                    break;
                }
                let file_name = candidate
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                match setup::sha256_file_cancelable(&candidate, &cancel_thread) {
                    Ok(digest) if setup::to_hex(&digest).eq_ignore_ascii_case(expected_sha256) => {
                        let dst = crate::models::trash_destination(&trash_dir, &file_name, &trash_names);
                        match std::fs::rename(&candidate, &dst) {
                            Ok(()) => {
                                trash_names.insert(
                                    dst.file_name().and_then(|n| n.to_str()).unwrap_or(&file_name).to_string(),
                                );
                                moved += 1;
                            }
                            Err(e) => skipped.push(format!("{} 移动失败: {e}", candidate.display())),
                        }
                    }
                    Ok(_) => skipped.push(format!("{} 哈希和预期不一致,未清理", candidate.display())),
                    Err(e) => skipped.push(format!("{} 校验失败: {e}", candidate.display())),
                }
            }
            let _ = tx.send((moved, skipped));
        });
        self.pending_cleanup = Some(PendingCleanup { binding_key, label, rx, cancel });
    }

    fn poll_cleanup(&mut self, ctx: &egui::Context) {
        let mut finished: Option<(usize, Vec<String>)> = None;
        if let Some(pc) = &self.pending_cleanup
            && let Ok(result) = pc.rx.try_recv()
        {
            finished = Some(result);
        }
        if let Some((moved, skipped)) = finished {
            let label = self.pending_cleanup.take().map(|p| p.label).unwrap_or_default();
            for s in &skipped {
                push_log(&mut self.log_lines, format!("{label} 清理: {s}"));
            }
            self.transient_notice = Some((
                if skipped.is_empty() {
                    format!("{label}:已清理 {moved} 份重复文件到 _trash")
                } else {
                    format!("{label}:已清理 {moved} 份,{} 份未处理(详见日志)", skipped.len())
                },
                std::time::Instant::now(),
            ));
            // 清理完之后候选列表已经不准了(文件挪走了),清掉避免用户对着过期的
            // "发现候选"提示再点一次。
            self.dup_results = None;
        }
        if self.pending_cleanup.is_some() {
            ctx.request_repaint();
        }
    }

    fn poll_verify(&mut self, ctx: &egui::Context) {
        let mut finished: Option<(String, String, PathBuf, Result<bool, String>)> = None;
        if let Some(pv) = &self.pending_verify
            && let Ok(result) = pv.rx.try_recv()
        {
            finished = Some((pv.binding_key.clone(), pv.label.clone(), pv.path.clone(), result));
        }
        if let Some((binding_key, label, path, result)) = finished {
            self.pending_verify = None;
            match result {
                Ok(true) => {
                    self.config.model_files.insert(binding_key, path.to_string_lossy().to_string());
                    self.config.save();
                    paths::set_model_bindings(
                        self.config
                            .model_files
                            .iter()
                            .map(|(k, v)| (k.clone(), PathBuf::from(v)))
                            .collect(),
                    );
                    push_log(&mut self.log_lines, format!("{label} 已绑定到 {}", path.display()));
                    self.transient_notice = Some((format!("{label} 已绑定"), std::time::Instant::now()));
                }
                Ok(false) => {
                    self.transient_notice = Some((
                        format!("{label}:文件哈希和预期不一致,不是这个模型,已忽略"),
                        std::time::Instant::now(),
                    ));
                }
                Err(e) if e == "已取消" => {}
                Err(e) => {
                    self.transient_notice =
                        Some((format!("{label}:校验失败: {e}"), std::time::Instant::now()));
                }
            }
        }
        if self.pending_verify.is_some() {
            ctx.request_repaint();
        }
    }

    fn poll_setup(&mut self, ctx: &egui::Context) {
        let mut done = false;
        let mut failed: Option<String> = None;
        let mut bound: Vec<(String, PathBuf)> = Vec::new();
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
                    SetupEvent::ModelBound { key, path } => bound.push((key, path)),
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
                            && !queue.is_empty() {
                                let next = queue.remove(0);
                                *current_label = next.label().to_string();
                                *current_index += 1;
                                *downloaded = 0;
                                *total = 0;
                            }
                    }
                    SetupEvent::Failed(e) => failed = Some(e),
                    SetupEvent::AllReady => done = true,
                }
            }
        }
        for (key, path) in bound {
            self.apply_model_bound(key, path);
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
        self.poll_verify(&ctx);
        self.poll_inline_install(&ctx);
        self.poll_scan(&ctx);
        self.poll_cleanup(&ctx);

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

/// 读文件开头 4 个字节,给 `models::classify_picked_file` 做类型嗅探用。
fn read_magic(path: &std::path::Path) -> std::io::Result<[u8; 4]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; 4];
    file.read_exact(&mut buf)?;
    Ok(buf)
}
