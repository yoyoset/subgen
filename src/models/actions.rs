//! `SubgenApp` 的模型管理操作:定位/校验/绑定/内联安装/扫描/清理。以 `impl SubgenApp` 的形式
//! 放在 models 域,避免这些方法全部堆在 app.rs 里撑爆行数预算(scripts/check-line-budgets.ps1
//! 限制 app.rs ≤1300 行)。和主文件里的方法同属一个类型,调用方式不变
//! (app.begin_locate_model(...) 等),只是按"这段代码回答哪个问题"拆文件。

use crate::app::{InlineInstall, PendingCleanup, PendingVerify, SubgenApp};
use crate::format::*;
use crate::paths;
use crate::setup::{self, ComponentKey, SetupEvent};
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

impl SubgenApp {
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
                crate::app::spawn_guarded_thread(
                    tx.clone(),
                    || Err("校验过程中发生内部错误(详情见 logs/crash.log)".to_string()),
                    move || {
                        let result =
                            setup::sha256_file_cancelable(&path_thread, &cancel_thread)
                                .map(|digest| {
                                    setup::to_hex(&digest).eq_ignore_ascii_case(expected_sha256)
                                });
                        let _ = tx.send(result);
                    },
                );
                self.pending_verify = Some(PendingVerify { binding_key, label, path: candidate, rx, cancel });
            }
        }
    }

    /// 命中本地已有文件、决定绑定而不是复制/下载(见 `setup::install_or_bind`)——写进
    /// config 才能跨进程记住,同步进程级绑定表才能这一轮立刻生效,不用等重启。两个来源
    /// 共用:首次运行的整屏下载流程(`SetupEvent::ModelBound`)和清单里的"下载"内联安装
    /// (`poll_inline_install`)。push_log 而不是静默处理——这是"省下了一次几 GB 下载"的
    /// 用户可见事件,值得留痕。
    pub(crate) fn apply_model_bound(&mut self, key: String, path: PathBuf) {
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
        crate::app::spawn_guarded_thread(
            tx.clone(),
            || {
                SetupEvent::Failed(
                    "安装过程中发生内部错误,已中止(详情见 logs/crash.log)".to_string(),
                )
            },
            move || {
                match setup::install_component(key, &whisper_model, &tx, &cancel_thread) {
                    Ok(()) => {
                        let _ = tx.send(SetupEvent::AllReady);
                    }
                    Err(e) => {
                        let _ = tx.send(SetupEvent::Failed(e));
                    }
                }
            },
        );
        self.inline_install = Some(InlineInstall { key, label, rx, cancel, downloaded: 0, total: 0 });
    }

    pub(crate) fn poll_inline_install(&mut self, ctx: &egui::Context) {
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
                    Err(e) if crate::app::is_user_cancel(e) => {
                        // 用户主动取消是"没装",不是"装失败"——措辞不能把取消说成错误。
                        format!("{label} 已取消安装")
                    }
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
        crate::app::spawn_guarded_thread(
            tx.clone(),
            || {
                (
                    std::collections::BTreeMap::new(),
                    std::collections::BTreeMap::new(),
                )
            },
            move || {
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
            },
        );
        self.scan_rx = Some(rx);
    }

    pub(crate) fn poll_scan(&mut self, ctx: &egui::Context) {
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
        crate::app::spawn_guarded_thread(
            tx.clone(),
            || (0, vec!["清理线程内部错误,已中止(详情见 logs/crash.log)".to_string()]),
            move || {
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
            },
        );
        self.pending_cleanup = Some(PendingCleanup { binding_key, label, rx, cancel });
    }

    pub(crate) fn poll_cleanup(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn poll_verify(&mut self, ctx: &egui::Context) {
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

}

/// 读文件开头 4 个字节,给 `models::classify_picked_file` 做类型嗅探用。
fn read_magic(path: &std::path::Path) -> std::io::Result<[u8; 4]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; 4];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

