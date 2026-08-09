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
    /// 启动时配置恢复的说明(配置文件损坏被重置时,用户必须知道自己的模型目录设置没了),
    /// 以可关闭横幅的形式显示,直到用户点"知道了"或显式关闭。
    pub(crate) startup_warning: Option<String>,
    pub(crate) jobs: Vec<Job>,
    pub(crate) setup_phase: SetupPhase,
    pub(crate) setup_rx: Option<mpsc::Receiver<SetupEvent>>,
    pub(crate) setup_cancel: Arc<AtomicBool>,
    /// 用户点了下载/搬迁页的"取消"、但后台线程还没真正停下来(下载线程可能正卡在两次
    /// 重试之间的 2 秒 sleep,或正在解压 zip——这两个阶段都不检查取消标志)。只用来驱动
    /// 界面反馈:按钮立即变灰显示"正在取消…",不然用户点击后界面纹丝不动,会怀疑按钮没
    /// 反应而连点甚至直接关窗。和批次条上的 `cancelling` 是同一个设计,只是那个管批次、
    /// 这个管首次下载/搬迁。
    pub(crate) setup_cancelling: bool,
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
    /// 上一次写进标题的下载进度文本(比如 "下载中 3/5 · 42%"),值没变就不重复发
    /// ViewportCommand::Title。首次下载动辄几 GB、要跑很久,用户 alt-tab 去干别的时,
    /// 任务栏悬停能直接看到下到哪了(和批次处理时标题带 "第 N/M 个" 是同一个考虑)。
    pub(crate) last_setup_title: Option<String>,
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

/// 后台线程的通用兜底:线程体内部 panic 时,向 `tx` 补发 `on_panic()` 返回的终态消息,并
/// 记一条日志——而不是让线程静默死掉、sender 被丢弃、界面的 poll 循环永远等不到终态
/// 消息卡在"处理中/下载中/扫描中"。下载、搬迁、内联安装、哈希校验、扫描、清理这些后台
/// 线程是同一个失败模式,统一走这里兜底。
///
/// `body` 正常结束时自己负责发送终态消息(现有逻辑不变);panic 时由这里补发。用
/// `AssertUnwindSafe` 是显式接受"panic 可能把线程局部状态留在中间值"的风险——这些线程
/// 的共享状态只有 cancel 标志和 channel,不会有跨线程读到的脏数据。
/// 下载/搬迁失败的消息是否表示"用户主动取消"(而不是真实错误)。取消路径统一产生带
/// "取消"字样的消息("用户取消" / "用户取消下载" / "已取消"),真实网络/IO/哈希错误不会
/// 出现这个字眼。把判断抽成命名函数,方便单测锁住这个约定。
pub(crate) fn is_user_cancel(msg: &str) -> bool {
    msg.contains("取消")
}

pub(crate) fn spawn_guarded_thread<T: Send + 'static>(
    tx: std::sync::mpsc::Sender<T>,
    on_panic: impl FnOnce() -> T + Send + 'static,
    body: impl FnOnce() + Send + 'static,
) {
    std::thread::spawn(move || {
        if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            crate::format::push_log(
                &mut Vec::new(),
                format!(
                    "后台线程内部崩溃(详情见 logs/crash.log): {}",
                    crate::job::panic_payload_message(&panic)
                ),
            );
            let _ = tx.send(on_panic());
        }
    });
}

impl SubgenApp {
    pub fn new(config: Config, startup_warning: Option<String>) -> Self {
        Self {
            config,
            startup_warning,
            jobs: Vec::new(),
            setup_phase: SetupPhase::Checking,
            setup_rx: None,
            setup_cancel: Arc::new(AtomicBool::new(false)),
            setup_cancelling: false,
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
            last_setup_title: None,
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
        self.setup_cancelling = false;
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
        // 清掉上一次下载可能留下的任务栏进度标题(比如用户取消后再点开始下载),等新的
        // Progress 事件来了再写新标题。
        self.last_setup_title = None;

        spawn_guarded_thread(
            tx.clone(),
            || {
                SetupEvent::Failed(
                    "下载过程中发生内部错误,已中止(详情见 logs/crash.log)".to_string(),
                )
            },
            move || {
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
            },
        );
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
        self.setup_cancelling = false;
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

        spawn_guarded_thread(
            tx.clone(),
            || {
                setup::MigrateEvent::AllDone(vec![
                    "搬迁线程内部错误,已中止(详情见 logs/crash.log)".to_string(),
                ])
            },
            move || {
                let issues = setup::migrate_models_dir(&old_dir, &new_dir_for_thread, &tx, &cancel);
                let _ = tx.send(setup::MigrateEvent::AllDone(issues));
            },
        );
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

    /// 把文件加入任务列表。返回 `(实际添加了几个, 被忽略了几个)`——拖进来的文件常常是
    /// 视频和别的文件混在一起(抓整季时 .nfo/.jpg 会一起拖进来),忽略非视频是预期行为;
    /// 但调用方要知道有没有"全都被忽略"的情况,好给用户一句反馈而不是让界面毫无反应。
    /// 顺带记住本次添加的第一个视频所在目录,下次"选择视频文件"默认跳到那。
    /// 只改内存里的 config.last_dir,不在这里落盘——config 统一在 on_exit/设置变更时保存,
    /// 这里做落盘会和 config 的测试在同一个 config 路径上产生竞争。
    pub(crate) fn add_files(&mut self, paths: Vec<PathBuf>) -> (usize, usize) {
        let mut added = 0usize;
        let mut ignored = 0usize;
        let mut last_dir: Option<PathBuf> = None;
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
                if last_dir.is_none()
                    && let Some(parent) = p.parent()
                {
                    last_dir = Some(parent.to_path_buf());
                }
                self.jobs.push(Job::new(p));
                added += 1;
            } else {
                ignored += 1;
            }
        }
        if let Some(dir) = last_dir {
            self.config.last_dir = Some(dir.to_string_lossy().to_string());
        }
        (added, ignored)
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
            // 所有任务都已完成,按钮却还亮着——点了毫无反应是最差的情况。给一句说明,
            // 并指出去哪重跑(任务行点"重新处理")。
            self.transient_notice = Some((
                "所有任务都已处理完成,没有需要重新处理的(想重跑某一集请在其任务行点“重新处理”)"
                    .to_string(),
                std::time::Instant::now(),
            ));
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
            self.config.subtitle_max_width,
            tx,
            self.worker_cancel.clone(),
        );
    }

    /// 生成 `job_index` 对应任务的翻译复核报告。文件读取/粗筛/格式化/写盘都在
    /// `report.rs`(见 `report::generate_review_report`),这里只负责把结果存进界面状态
    /// `self.review`,失败时用 transient_notice 提示——之前文件写失败是静默吞掉的,
    /// 用户看到界面有报告、实际视频旁边没有文件,属于"后台失败但看起来成功"。
    pub(crate) fn generate_review_report(&mut self, job_index: usize) {
        let Some(job) = self.jobs.get(job_index) else {
            return;
        };
        let video_path = job.video_path.clone();
        let label = video_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        match crate::report::generate_review_report(&video_path, &label) {
            Ok(data) => {
                self.review = Some(ReviewReportState {
                    job_index,
                    text: data.text,
                    suspect_indices: data.suspect_indices,
                    saved_path: data.saved_path,
                    correction_input: String::new(),
                    correction_result: None,
                });
            }
            Err(e) => {
                self.transient_notice = Some((e, std::time::Instant::now()));
            }
        }
    }

    /// 把用户粘贴回来的编号行("129. 修正后的译文")应用到对应任务的 `.zh.srt`。
    /// 文件重写逻辑在 `report::apply_corrections`,这里只负责编排:校验报告属于当前任务、
    /// 取粘贴内容、把结果文案写回 `self.review`。
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

        let msg = match crate::report::apply_corrections(&zh_path, &allowed, &pasted) {
            Ok((0, ignored)) => {
                if ignored.is_empty() {
                    "没有识别到任何可应用的修正行(格式应为“编号. 修正后的译文”)".to_string()
                } else {
                    format!(
                        "没有应用任何修正——这些编号不在本次报告标记的可疑行范围内: {}",
                        ignored.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("、")
                    )
                }
            }
            Ok((applied, ignored)) => {
                if let Some(r) = &mut self.review {
                    r.correction_input.clear();
                }
                if ignored.is_empty() {
                    format!("已应用 {applied} 处修正(原文件已备份为 .bak)")
                } else {
                    format!(
                        "已应用 {applied} 处修正(原文件已备份为 .bak);忽略了不在标记范围内的编号: {}",
                        ignored.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("、")
                    )
                }
            }
            Err(e) => e,
        };
        if let Some(r) = &mut self.review {
            r.correction_result = Some(msg);
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
                            current_index,
                            total_count,
                            downloaded: d,
                            total: t,
                            ..
                        } = &mut self.setup_phase
                        {
                            *current_label = component;
                            *d = downloaded;
                            *t = total;
                            // 任务栏标题带上"下载中 第 N/M 项 · 百分比"——首次下载几 GB 要跑
                            // 很久,用户 alt-tab 去别的窗口时不用切回来就能看到进度。只在
                            // 文本真的变了才发 Title 命令(和批次标题同一个防抖思路)。
                            let pct = if *t > 0 {
                                format!("{:.0}%", *d as f32 / *t as f32 * 100.0)
                            } else {
                                String::new()
                            };
                            // total 还没拿到(首次 Progress 事件可能只有 downloaded)时 pct 为空,
                            // 标题不带" · "尾巴,免得出现 "下载中 1/5 · " 这种残句。
                            let title = if pct.is_empty() {
                                format!(
                                    "{} 下载中 {}/{}",
                                    crate::window_title(),
                                    *current_index,
                                    *total_count,
                                )
                            } else {
                                format!(
                                    "{} 下载中 {}/{} · {}",
                                    crate::window_title(),
                                    *current_index,
                                    *total_count,
                                    pct
                                )
                            };
                            if self.last_setup_title.as_deref() != Some(title.as_str()) {
                                self.last_setup_title = Some(title.clone());
                                ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
                            }
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
            self.last_setup_title = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(crate::window_title()));
        } else if let Some(e) = failed {
            self.setup_rx = None;
            if is_user_cancel(&e) {
                // 用户主动取消下载,不是失败:回到"还缺什么"的清单界面(已下完的组件
                // 会被 missing_components 重新算掉),而不是显示"下载失败,请检查网络"
                // 这种误导文案——用户会以为自己网断了,其实只是自己点了取消。
                self.last_setup_title = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(crate::window_title()));
                self.begin_setup_check();
            } else {
                self.setup_phase = SetupPhase::Error(e);
            }
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
            // 批次已经跑完了,退出确认框里那句"正在处理中,退出将中止全部任务"就不成立了——
            // 继续留着会误导用户以为还有东西在跑。没有在跑的任务要中止,直接关掉确认框。
            self.close_confirm_open = false;
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
                let (added, ignored) = self.add_files(dropped);
                // 全都被忽略(比如拖进来的都是非视频/重复文件)时,界面不能毫无反应——
                // 用户会以为操作没成功、反复重试。给一句说明;有部分成功就保持安静,
                // 避免每次拖文件都弹提示制造噪音。
                if added == 0 && ignored > 0 {
                    self.transient_notice = Some((
                        "没有添加任务:拖入的文件里没有可识别的视频(mp4/mkv/avi 等),或都在列表中了"
                            .to_string(),
                        std::time::Instant::now(),
                    ));
                }
            }
        }
        if let Some((_, shown_at)) = &self.transient_notice {
            if shown_at.elapsed().as_secs_f32() > 3.0 {
                self.transient_notice = None;
            } else {
                ctx.request_repaint(); // 保证过期后能被下一帧检查到、及时消失,不用等下次交互触发重绘
            }
        }

        crate::ui::setup_screens::render_phase(self, ui);
    }

    // glow 后端的 App::on_exit 比 wgpu 后端多一个 Option<&glow::Context> 参数(用来在退出前
    // 释放 OpenGL 资源);这里不需要手动清理任何 GL 资源,参数用不上,但签名必须匹配。
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.config.save();
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 关键不变量:线程体内部 panic 时,`spawn_guarded_thread` 必须补发 on_panic 的终态
    /// 消息——这是"后台线程死了但界面永远等不到结果"这类卡死的根治手段。
    #[test]
    fn spawn_guarded_thread_sends_fallback_instead_of_hanging_on_panic() {
        let (tx, rx) = mpsc::channel();
        spawn_guarded_thread(
            tx.clone(),
            || "panic fallback".to_string(),
            move || {
                panic!("测试用:模拟后台线程内部崩溃");
            },
        );
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(msg) => assert_eq!(msg, "panic fallback"),
            Err(e) => panic!("应该收到 panic 兜底消息,而不是永远等不到: {e:?}"),
        }
    }

    #[test]
    fn spawn_guarded_thread_passes_through_normal_terminal_message() {
        let (tx, rx) = mpsc::channel();
        spawn_guarded_thread(
            tx.clone(),
            || "panic fallback".to_string(),
            move || {
                let _ = tx.send("normal result".to_string());
            },
        );
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(msg) => assert_eq!(msg, "normal result"),
            Err(e) => panic!("正常结束也应该收到结果: {e:?}"),
        }
    }

    /// 拖入/选中的文件里混着非视频和重复项时,只加第一个,其余按"被忽略"计数返回,
    /// 让调用方能据此给用户反馈;并记住第一个视频所在目录,供下次文件对话框默认跳转。
    #[test]
    fn add_files_counts_added_vs_ignored_and_remembers_dir() {
        let dir = std::env::temp_dir().join(format!(
            "subgen_test_add_files_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("Episode.mkv");
        let image = dir.join("poster.jpg");
        std::fs::write(&video, b"v").unwrap();
        std::fs::write(&image, b"i").unwrap();

        let mut app = SubgenApp::new(Config::default(), None);
        let (added, ignored) = app.add_files(vec![video.clone(), image.clone(), video.clone()]);
        assert_eq!(added, 1, "只有一个视频被加进列表");
        assert_eq!(ignored, 2, "jpg 和重复的视频都被忽略");
        assert_eq!(app.jobs.len(), 1);
        assert_eq!(
            app.config.last_dir.as_deref(),
            Some(dir.to_string_lossy().as_ref()),
            "应记住第一个视频所在目录,供下次对话框默认跳转"
        );

        // 全部被忽略时(没有视频可记),last_dir 保持不变。
        let before = app.config.last_dir.clone();
        let (added, _) = app.add_files(vec![image.clone()]);
        assert_eq!(added, 0);
        assert_eq!(app.config.last_dir, before, "没有添加任何视频时不该改动 last_dir");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 下载取消的判定约定:取消路径的消息都带"取消",真实失败(网络/IO/哈希)不带。
    #[test]
    fn is_user_cancel_only_matches_cancel_messages() {
        assert!(is_user_cancel("用户取消"));
        assert!(is_user_cancel("用户取消下载"));
        assert!(is_user_cancel("已取消"));
        assert!(!is_user_cancel("下载 whisper 多次重试后仍失败: 连接超时"));
        assert!(!is_user_cancel("文件校验哈希不一致(期望 xxx,实际 yyy)"));
    }
}
