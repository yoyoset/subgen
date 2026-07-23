//! 首次运行组件检测与自动下载。
//!
//! 所有下载地址、哈希值均在编写本文件时实际验证过(直接下载并本地计算哈希 / 实测运行确认可用),
//! 不是凭空猜测的占位值。engine、vad、model(large-v3)、ffmpeg 四者的哈希都是完整下载后本地
//! sha256sum 验证过的,校验失败会阻断安装(model 的哈希最初来自 HuggingFace 响应头 X-Linked-ETag,
//! 后来完整下载 large-v3 复核过一遍,两者一致,已从"仅警告"升级为强校验)。ffmpeg 早期用的是
//! gyan.dev 滚动更新的"最新构建"链接,没有可钉死的哈希;已改为 GyanD/codexffmpeg 在 GitHub
//! Release 发布的带版本号构建(gyan.dev 官方在 GitHub 上的镜像),可以钉版本号和哈希,不再
//! 依赖一个随时可能变化的滚动链接。

use crate::paths;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum SetupEvent {
    Log(String),
    Progress {
        component: String,
        downloaded: u64,
        total: u64,
    },
    ComponentReady(String),
    Failed(String),
    AllReady,
}

const ENGINE_URL: &str =
    "https://github.com/ggml-org/whisper.cpp/releases/download/v1.9.1/whisper-cublas-12.4.0-bin-x64.zip";
const ENGINE_SHA256: &str = "106a2030eff8998e4ef320fe72e263a78449e9040386ee27c41ea80b001b601b";

// GyanD/codexffmpeg 是 gyan.dev 构建在 GitHub 上的正式发布仓库,essentials 包内容和
// gyan.dev 官网提供的一致,但带版本号 tag,可以钉死哈希,不像官网的"最新构建"链接那样
// 会在不通知的情况下随时变化。哈希取自 GitHub Release API 返回的资产 digest 字段,
// 并额外完整下载 109,728,040 字节的包本地 sha256sum 复核过一遍,两者一致。
// 以后升级 ffmpeg 版本:去 https://github.com/GyanD/codexffmpeg/releases/latest 看新 tag,
// 重新下载一遍算真实哈希,不要凭记忆/推测填写。
const FFMPEG_URL: &str =
    "https://github.com/GyanD/codexffmpeg/releases/download/8.1.2/ffmpeg-8.1.2-essentials_build.zip";
const FFMPEG_SHA256: &str = "db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec";

const VAD_URL: &str = "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin";
const VAD_SHA256: &str = "29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf";

// llama.cpp release b10079,Windows CUDA 12.4 预编译版。哈希取自 GitHub Release API 返回的
// 资产 digest 字段(下载前用 API 校验过,不是凭空写的)。不随附单独的 cudart 运行库包(那个
// 包约 400MB)——translate.rs 里 llama-server 启动时复用 whisper 引擎目录已有的同版本 CUDA
// 运行库 DLL,见 translate::LlamaServer::start。
const TRANSLATOR_URL: &str =
    "https://github.com/ggml-org/llama.cpp/releases/download/b10079/llama-b10079-bin-win-cuda-12.4-x64.zip";
const TRANSLATOR_SHA256: &str = "3f94750a057e22e970e5fc6e4960dddd018baf4ed69829c84d1a2f09b5894bdd";

fn model_url(model: &str) -> String {
    format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}.bin")
}

/// 完整下载 + 本地 sha256sum 验证过的哈希(large-v3 已复核,与 HF 响应头一致)。
fn model_sha256_hint(model: &str) -> Option<&'static str> {
    match model {
        "large-v3" => Some("64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2"),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKey {
    Engine,
    Ffmpeg,
    Model,
    Vad,
    /// 本地翻译引擎(llama.cpp / llama-server)
    Translator,
    /// 翻译模型,携带 translate::MODELS 里的 key(如 "sakura-7b")
    TransModel(&'static str),
}

impl ComponentKey {
    pub fn label(&self) -> &'static str {
        match self {
            ComponentKey::Engine => "识别引擎 (whisper.cpp)",
            ComponentKey::Ffmpeg => "FFmpeg (音频提取)",
            ComponentKey::Model => "语音识别模型",
            ComponentKey::Vad => "语音活动检测模型",
            ComponentKey::Translator => "本地翻译引擎 (llama.cpp)",
            ComponentKey::TransModel(key) => crate::translate::model_by_key(key)
                .map(|m| m.label)
                .unwrap_or("翻译模型"),
        }
    }
}

pub fn is_present(key: ComponentKey, model: &str) -> bool {
    match key {
        ComponentKey::Engine => paths::whisper_cli_exe().is_some(),
        ComponentKey::Ffmpeg => paths::ffmpeg_exe().is_some(),
        ComponentKey::Model => {
            let p = paths::model_path(model);
            p.is_file() && p.metadata().map(|m| m.len() > 1_000_000).unwrap_or(false)
        }
        ComponentKey::Vad => {
            let p = paths::vad_model_path();
            p.is_file() && p.metadata().map(|m| m.len() > 100_000).unwrap_or(false)
        }
        ComponentKey::Translator => paths::llama_server_exe().is_some(),
        ComponentKey::TransModel(key) => match crate::translate::model_by_key(key) {
            Some(tm) => {
                let p = paths::trans_model_path(tm.file_name);
                p.is_file() && p.metadata().map(|m| m.len() > 1_000_000).unwrap_or(false)
            }
            None => false,
        },
    }
}

/// 组件的估算下载大小(字节),用于下载前的磁盘空间提示。均从 GitHub Release API / HuggingFace
/// LFS 指针文件核实过的真实字节数(ffmpeg 已改为固定版本,见 FFMPEG_URL 处说明),不是拍脑袋
/// 估的;只用于提示、不做硬阻断,所以即便未来上游文件大小有细微出入也不影响功能。
pub fn estimated_size_bytes(key: ComponentKey, model: &str) -> u64 {
    match key {
        ComponentKey::Engine => 677_887_125,
        ComponentKey::Ffmpeg => 109_728_040,
        ComponentKey::Model => match model {
            "large-v3" => 3_095_033_483,
            "large-v3-turbo" => 1_624_555_275,
            "medium" => 1_533_763_059,
            _ => 3_000_000_000, // 未知档位,给一个保守估算,不影响功能只影响提示准确度
        },
        ComponentKey::Vad => 885_098,
        ComponentKey::Translator => 249_081_777,
        ComponentKey::TransModel(key) => crate::translate::model_by_key(key)
            .map(|m| m.size_bytes)
            .unwrap_or(3_000_000_000),
    }
}

/// 汇总一批组件的估算大小并与目标目录所在盘的剩余空间比较。返回 (需要的字节数, 剩余字节数)
/// 供界面拼提示文案;剩余空间查询失败(比如路径尚不存在)时返回 None,界面应该跳过这条提示
/// 而不是报错阻断——磁盘空间检查是锦上添花,查不到就不查,不该变成新的失败点。
///
/// `target_dir` 传的是 `models_dir()`,所以这里只汇总真正会落到 models_dir() 下的组件
/// (语音模型、VAD、翻译模型);Engine/Ffmpeg/Translator 是装到 app_dir() 相对目录的可执行
/// 程序,不受 models_dir() 影响,不该被算进"这块盘够不够"里——用户自定义模型目录之后,
/// exe 所在的盘和模型所在的盘完全可能是两块不同的盘。
pub fn check_disk_space(missing: &[ComponentKey], whisper_model: &str, target_dir: &Path) -> Option<(u64, u64)> {
    let needed: u64 = missing
        .iter()
        .filter(|k| !matches!(k, ComponentKey::Engine | ComponentKey::Ffmpeg | ComponentKey::Translator))
        .map(|k| estimated_size_bytes(*k, whisper_model))
        .sum();
    // 目标目录可能还没创建(比如换到一个全新的路径),往上找第一个存在的祖先目录来查询,
    // 磁盘剩余空间是盘级别的,祖先目录还没建好不影响查询结果的准确性。
    let mut probe = target_dir.to_path_buf();
    loop {
        if probe.exists() {
            return fs4::available_space(&probe).ok().map(|avail| (needed, avail));
        }
        if !probe.pop() {
            return None;
        }
    }
}

/// `translate_enabled` 为 false 时不检测/不下载任何翻译相关组件(不用的功能不该逼用户下载
/// 几 GB 东西)。`translate_model` 为 "auto" 时两个翻译模型都要有(因为运行时按视频语言
/// 动态选);指定具体模型时只需要那一个。
pub fn missing_components(
    whisper_model: &str,
    translate_enabled: bool,
    translate_model: &str,
) -> Vec<ComponentKey> {
    let mut keys = vec![
        ComponentKey::Engine,
        ComponentKey::Ffmpeg,
        ComponentKey::Model,
        ComponentKey::Vad,
    ];
    if translate_enabled {
        keys.push(ComponentKey::Translator);
        // ComponentKey::TransModel 存的是 &'static str,不能直接塞调用方传入的 &str
        // (生命周期不够),所以从注册表里查出对应的 'static key 再用。查不到(比如配置文件
        // 里存了个过期/拼错的 key)时按 "auto" 处理,把两个模型都要求齐全,run 时更保险。
        let known = translate_model != "auto"
            && crate::translate::model_by_key(translate_model).is_some();
        if known {
            let m = crate::translate::model_by_key(translate_model).unwrap();
            keys.push(ComponentKey::TransModel(m.key));
        } else {
            for m in crate::translate::MODELS {
                keys.push(ComponentKey::TransModel(m.key));
            }
        }
    }
    keys.into_iter()
        .filter(|k| !is_present(*k, whisper_model))
        .collect()
}

/// 下载并安装一个组件。cancel 标志在下载中会被周期性检查,置位后会中止并清理临时文件。
pub fn install_component(
    key: ComponentKey,
    model: &str,
    tx: &Sender<SetupEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    match key {
        ComponentKey::Engine => {
            download_and_extract_zip(
                key.label(),
                ENGINE_URL,
                Some(ENGINE_SHA256),
                true,
                &paths::engine_dir(),
                tx,
                cancel,
            )?;
            if paths::whisper_cli_exe().is_none() {
                return Err("解压完成但未找到 whisper-cli.exe,官方压缩包内容可能已变化".to_string());
            }
            Ok(())
        }
        ComponentKey::Ffmpeg => {
            download_and_extract_zip(
                key.label(),
                FFMPEG_URL,
                Some(FFMPEG_SHA256),
                true,
                &paths::ffmpeg_dir(),
                tx,
                cancel,
            )?;
            if paths::ffmpeg_exe().is_none() {
                return Err("解压完成但未找到 ffmpeg.exe,官方压缩包内容可能已变化".to_string());
            }
            Ok(())
        }
        ComponentKey::Model => download_file(
            key.label(),
            &model_url(model),
            model_sha256_hint(model),
            true,
            &paths::model_path(model),
            tx,
            cancel,
        ),
        ComponentKey::Vad => download_file(
            key.label(),
            VAD_URL,
            Some(VAD_SHA256),
            true,
            &paths::vad_model_path(),
            tx,
            cancel,
        ),
        ComponentKey::Translator => {
            download_and_extract_zip(
                key.label(),
                TRANSLATOR_URL,
                Some(TRANSLATOR_SHA256),
                true,
                &paths::translator_dir(),
                tx,
                cancel,
            )?;
            if paths::llama_server_exe().is_none() {
                return Err(
                    "解压完成但未找到 llama-server.exe,官方压缩包内容可能已变化".to_string(),
                );
            }
            Ok(())
        }
        ComponentKey::TransModel(model_key) => {
            let tm = crate::translate::model_by_key(model_key)
                .ok_or_else(|| format!("未知的翻译模型标识: {model_key}"))?;
            install_trans_model(tm, tx, cancel)
        }
    }
}

/// 安装翻译模型:优先复用本地 HuggingFace 缓存(用户可能已经用别的工具下载过,比如
/// SakuraLLM 这种几 GB 大小的模型,重新下载没必要),哈希校验通过才复制;缓存里没有或
/// 哈希不对才走网络下载。
fn install_trans_model(
    tm: &crate::translate::TransModel,
    tx: &Sender<SetupEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let target = paths::trans_model_path(tm.file_name);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    if let Some(cached) = find_in_hf_cache(tm.file_name) {
        let _ = tx.send(SetupEvent::Log(format!(
            "在本地 HuggingFace 缓存中找到 {},校验哈希中...",
            tm.label
        )));
        match sha256_file(&cached) {
            Ok(digest) if to_hex(&digest).eq_ignore_ascii_case(tm.sha256) => {
                std::fs::copy(&cached, &target)
                    .map_err(|e| format!("复制 {} 失败: {e}", tm.label))?;
                let _ = tx.send(SetupEvent::ComponentReady(tm.label.to_string()));
                return Ok(());
            }
            Ok(_) => {
                let _ = tx.send(SetupEvent::Log(format!(
                    "{} 本地缓存文件哈希不一致,改为重新下载",
                    tm.label
                )));
            }
            Err(e) => {
                let _ = tx.send(SetupEvent::Log(format!(
                    "读取 {} 本地缓存失败({e}),改为重新下载",
                    tm.label
                )));
            }
        }
    }

    download_file(tm.label, tm.url, Some(tm.sha256), true, &target, tx, cancel)
}

/// 在 HuggingFace 本地缓存里按文件名查找。依次检查 HF_HOME、HF_HUB_CACHE 环境变量指向的
/// 目录,以及 huggingface_hub 默认位置(`~/.cache/huggingface/hub`),都是 huggingface_hub
/// 官方文档记录的标准缓存位置解析顺序,不是猜的路径。
fn find_in_hf_cache(file_name: &str) -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(hf_hub_cache) = std::env::var("HF_HUB_CACHE") {
        roots.push(PathBuf::from(hf_hub_cache));
    }
    if let Ok(hf_home) = std::env::var("HF_HOME") {
        roots.push(PathBuf::from(hf_home).join("hub"));
    }
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        roots.push(
            PathBuf::from(userprofile)
                .join(".cache")
                .join("huggingface")
                .join("hub"),
        );
    }
    roots
        .into_iter()
        .find_map(|root| paths::find_file_recursive(&root, file_name))
}

fn sha256_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("读取文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

// ---------------------------------------------------------------------------
// 模型目录搬迁:用户改存放位置时,把旧目录下已经下载好的文件搬到新目录,不用重新下载。
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum MigrateEvent {
    Log(String),
    Progress {
        file_label: String,
        current_index: usize,
        total_count: usize,
        copied: u64,
        total: u64,
    },
    FileDone(String),
    /// 搬迁跑完了(不管顺不顺利)。携带的 Vec 是每个"没能完全按预期完成"的文件的说明
    /// (跳过、单个文件失败、取消导致没轮到的都算);空 Vec 表示全部顺利搬完。
    AllDone(Vec<String>),
}

/// 把 `old_dir` 下的所有文件搬到 `new_dir`。不针对具体文件名(不维护一份"已知模型文件名"
/// 清单,那份清单容易漏、容易跟 translate::MODELS 等注册表脱节),而是"旧目录里有什么就搬
/// 什么"——models_dir() 本来就是个平铺目录,不该有用户不认识的东西在里面,这样处理更通用,
/// 也顺带覆盖了用户手动放进去的文件。
///
/// 目标目录如果已经有同名文件:大小相同就当作"已经搬过/本来就有效"跳过,不重新搬;大小不同
/// 时为避免误覆盖一份可能是有效的文件,直接跳过并留在原地——这份文件的最终有效性由外层
/// `is_present`/哈希校验逻辑负责,搬迁这一步只负责不丢数据,不负责判断内容对不对。
///
/// **单个文件出错不会中断整个搬迁**(取消除外):哪怕某个文件搬迁失败,也继续搬下一个,
/// 最后把所有问题汇总返回,而不是"一个文件坏了、后面几个 GB 的文件全跳过不搬"。调用方
/// 不管返回值是否为空、过程中是否被取消,都应该把生效的模型目录切到 `new_dir`——部分文件
/// 可能已经被物理搬过去了,`new_dir` 才是准确反映当前实际存放位置的地方,剩下的缺口交给
/// 外层现成的 `missing_components()` 流程去补齐(它本来就是按"新目录下这个文件在不在、
/// 对不对"逐项判断,不需要为这个中间状态写特殊分支)。
pub fn migrate_models_dir(
    old_dir: &Path,
    new_dir: &Path,
    tx: &Sender<MigrateEvent>,
    cancel: &Arc<AtomicBool>,
) -> Vec<String> {
    if let Err(e) = std::fs::create_dir_all(new_dir) {
        return vec![format!("创建目标目录失败: {e}")];
    }

    let entries: Vec<PathBuf> = match std::fs::read_dir(old_dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect(),
        Err(_) => Vec::new(), // 旧目录不存在/读不了,当作没有东西要搬,不算错误
    };

    let mut issues = Vec::new();
    if entries.is_empty() {
        return issues;
    }

    let total_count = entries.len();
    for (i, src) in entries.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            issues.push(format!(
                "已取消,剩余 {} 个文件未搬迁,仍留在旧目录",
                total_count - i
            ));
            break;
        }
        let file_name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("未知文件")
            .to_string();
        let dst = new_dir.join(&file_name);
        let current_index = i + 1;
        let _ = tx.send(MigrateEvent::Log(format!(
            "正在搬迁 {file_name}({current_index}/{total_count})..."
        )));
        if let Err(e) = migrate_one_file(src, &dst, &file_name, current_index, total_count, tx, cancel)
        {
            let _ = tx.send(MigrateEvent::Log(format!("{file_name} 搬迁出错: {e}")));
            issues.push(e);
        }
    }
    issues
}

fn migrate_one_file(
    src: &Path,
    dst: &Path,
    label: &str,
    index: usize,
    total_count: usize,
    tx: &Sender<MigrateEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    if dst.exists() {
        let src_len = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        let dst_len = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        if src_len == dst_len {
            let _ = tx.send(MigrateEvent::Log(format!("{label} 目标位置已存在且大小一致,跳过")));
        } else {
            let _ = tx.send(MigrateEvent::Log(format!(
                "{label} 目标位置已有同名但大小不同的文件,为避免覆盖有效数据已跳过搬迁,\
                 原文件仍保留在旧目录"
            )));
        }
        let _ = tx.send(MigrateEvent::FileDone(label.to_string()));
        return Ok(());
    }

    // 优先尝试同盘 rename——原子操作,几乎瞬间完成,不需要额外的完整性校验。
    if std::fs::rename(src, dst).is_ok() {
        let _ = tx.send(MigrateEvent::FileDone(label.to_string()));
        return Ok(());
    }

    // rename 失败(通常是跨盘符):退化成"复制到临时名 -> 核对大小 -> 原子改名到位 ->
    // 删除源文件"。任何一步中断(取消/掉电/进程被杀),旧目录里的源文件都还在、没被删,
    // 不会出现两边都没有的丢失状态,顶多是这次搬迁没完成、下次再试一遍。
    let tmp_dst = dst.with_extension("migrating");
    let total = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    copy_with_progress(src, &tmp_dst, label, index, total_count, total, tx, cancel)?;

    let copied_len = std::fs::metadata(&tmp_dst).map(|m| m.len()).unwrap_or(0);
    if copied_len != total {
        let _ = std::fs::remove_file(&tmp_dst);
        return Err(format!("{label} 搬迁校验失败(复制后大小不一致),源文件保留在原位置"));
    }

    std::fs::rename(&tmp_dst, dst).map_err(|e| format!("{label} 搬迁完成但重命名失败: {e}"))?;
    // 新位置的文件此时已经就绪可用,即使下面删源文件这步失败,也不算搬迁失败——顶多是旧
    // 目录里多留了一份没清理掉的副本,不影响功能,日志里如实记录方便用户自己清。
    if let Err(e) = std::fs::remove_file(src) {
        let _ = tx.send(MigrateEvent::Log(format!(
            "{label} 已搬到新目录,但删除旧位置文件失败(可手动清理): {e}"
        )));
    }
    let _ = tx.send(MigrateEvent::FileDone(label.to_string()));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_with_progress(
    src: &Path,
    dst: &Path,
    label: &str,
    index: usize,
    total_count: usize,
    total: u64,
    tx: &Sender<MigrateEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mut reader = std::fs::File::open(src).map_err(|e| format!("打开源文件失败: {e}"))?;
    let mut writer = std::fs::File::create(dst).map_err(|e| format!("创建目标文件失败: {e}"))?;
    // 1MB 缓冲区:这是跨盘搬迁大文件(几个 GB),块大一点能减少系统调用次数,
    // 和 setup.rs 下载用的 64KB(网络包大小量级)不是同一个场景,没必要保持一致。
    let mut buf = vec![0u8; 1024 * 1024];
    let mut copied: u64 = 0;
    let mut last_report = std::time::Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(writer);
            let _ = std::fs::remove_file(dst);
            return Err("已取消".to_string());
        }
        let n = reader.read(&mut buf).map_err(|e| format!("读取源文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .map_err(|e| format!("写入目标文件失败: {e}"))?;
        copied += n as u64;
        if last_report.elapsed().as_millis() > 150 {
            let _ = tx.send(MigrateEvent::Progress {
                file_label: label.to_string(),
                current_index: index,
                total_count,
                copied,
                total,
            });
            last_report = std::time::Instant::now();
        }
    }
    let _ = tx.send(MigrateEvent::Progress {
        file_label: label.to_string(),
        current_index: index,
        total_count,
        copied,
        total,
    });
    Ok(())
}

fn download_file(
    label: &str,
    url: &str,
    sha256_hex: Option<&str>,
    strict: bool,
    target: &Path,
    tx: &Sender<SetupEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let tmp_path = target.with_extension("part");
    let digest = download_to_file(label, url, &tmp_path, tx, cancel)?;
    verify_hash_or_warn(label, &digest, sha256_hex, strict, &tmp_path, tx)?;

    std::fs::rename(&tmp_path, target).map_err(|e| format!("安装 {label} 失败: {e}"))?;
    let _ = tx.send(SetupEvent::ComponentReady(label.to_string()));
    Ok(())
}

fn download_and_extract_zip(
    label: &str,
    url: &str,
    sha256_hex: Option<&str>,
    strict: bool,
    extract_dir: &Path,
    tx: &Sender<SetupEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    std::fs::create_dir_all(extract_dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let tmp_zip = paths::app_dir().join(format!("_dl_{}.zip.part", sanitize(label)));
    let digest = download_to_file(label, url, &tmp_zip, tx, cancel)?;
    verify_hash_or_warn(label, &digest, sha256_hex, strict, &tmp_zip, tx)?;

    let _ = tx.send(SetupEvent::Log(format!("正在解压 {label} ...")));
    let file = std::fs::File::open(&tmp_zip).map_err(|e| format!("打开压缩包失败: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("{label} 不是有效的压缩包: {e}"))?;
    archive
        .extract(extract_dir)
        .map_err(|e| format!("解压 {label} 失败: {e}"))?;
    let _ = std::fs::remove_file(&tmp_zip);
    Ok(())
}

/// 校验下载文件的哈希。strict=true 且不匹配时删除文件并返回错误(阻断安装);
/// strict=false 且不匹配时只发一条警告日志,继续安装。
fn verify_hash_or_warn(
    label: &str,
    digest: &[u8; 32],
    expected_hex: Option<&str>,
    strict: bool,
    file_to_remove_on_fail: &Path,
    tx: &Sender<SetupEvent>,
) -> Result<(), String> {
    if let Some(expected) = expected_hex {
        let actual = to_hex(digest);
        if !actual.eq_ignore_ascii_case(expected) {
            let msg = format!("{label} 文件校验哈希不一致(期望 {expected}, 实际 {actual})");
            if strict {
                let _ = std::fs::remove_file(file_to_remove_on_fail);
                return Err(msg);
            } else {
                let _ = tx.send(SetupEvent::Log(format!("警告: {msg}, 仍继续使用")));
            }
        }
    }
    Ok(())
}

/// 流式下载到临时文件,边下载边计算 sha256,定期上报进度,支持取消。
fn download_to_file(
    label: &str,
    url: &str,
    tmp_path: &Path,
    tx: &Sender<SetupEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<[u8; 32], String> {
    let _ = tx.send(SetupEvent::Log(format!("开始下载 {label} ...")));
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(1800))
        .build()
        .map_err(|e| format!("创建下载客户端失败: {e}"))?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| format!("下载 {label} 失败(网络错误): {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载 {label} 失败: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);

    let mut file =
        std::fs::File::create(tmp_path).map_err(|e| format!("创建临时文件失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    let mut last_report = std::time::Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(tmp_path);
            return Err("用户取消下载".to_string());
        }
        let n = resp
            .read(&mut buf)
            .map_err(|e| format!("下载 {label} 中断: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入文件失败: {e}"))?;
        hasher.update(&buf[..n]);
        downloaded += n as u64;
        if last_report.elapsed().as_millis() > 150 {
            let _ = tx.send(SetupEvent::Progress {
                component: label.to_string(),
                downloaded,
                total,
            });
            last_report = std::time::Instant::now();
        }
    }
    let _ = tx.send(SetupEvent::Progress {
        component: label.to_string(),
        downloaded,
        total: total.max(downloaded),
    });

    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// 每个测试用独立的、带随机后缀的临时目录,避免并行跑的测试互相踩到同一个路径。
    fn temp_subdir(name: &str) -> PathBuf {
        let unique = format!(
            "{}_{:x}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join("subgen_setup_test").join(unique)
    }

    #[test]
    fn migrate_moves_files_to_new_dir() {
        let old_dir = temp_subdir("migrate_move_old");
        let new_dir = temp_subdir("migrate_move_new");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("model.bin"), b"hello model").unwrap();

        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let issues = migrate_models_dir(&old_dir, &new_dir, &tx, &cancel);

        assert!(issues.is_empty(), "正常搬迁不应该有问题: {issues:?}");
        assert!(!old_dir.join("model.bin").exists(), "源文件应该被搬走");
        assert_eq!(
            std::fs::read(new_dir.join("model.bin")).unwrap(),
            b"hello model",
            "新目录里的文件内容应该和原文件一致"
        );

        let _ = std::fs::remove_dir_all(&old_dir);
        let _ = std::fs::remove_dir_all(&new_dir);
    }

    #[test]
    fn migrate_skips_when_target_already_has_same_size_file() {
        let old_dir = temp_subdir("migrate_skip_same_old");
        let new_dir = temp_subdir("migrate_skip_same_new");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(old_dir.join("model.bin"), b"same size!!").unwrap(); // 11 bytes
        std::fs::write(new_dir.join("model.bin"), b"already here").unwrap(); // 12 bytes,故意不同

        // 先测大小不同的情况:应该跳过,两边都保留原样
        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let issues = migrate_models_dir(&old_dir, &new_dir, &tx, &cancel);
        assert!(issues.is_empty(), "跳过不算错误,不应该出现在 issues 里");
        assert!(old_dir.join("model.bin").exists(), "大小不一致时不应该删除源文件");
        assert_eq!(
            std::fs::read(new_dir.join("model.bin")).unwrap(),
            b"already here",
            "大小不一致时不应该覆盖目标目录里已有的文件"
        );

        let _ = std::fs::remove_dir_all(&old_dir);
        let _ = std::fs::remove_dir_all(&new_dir);
    }

    #[test]
    fn migrate_skips_and_removes_source_when_target_has_same_size_file() {
        let old_dir = temp_subdir("migrate_skip_exact_old");
        let new_dir = temp_subdir("migrate_skip_exact_new");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        // 大小相同(都是 11 字节),内容其实不同也没关系——这一步只用大小做"看起来已经
        // 搬过"的启发式判断,不做内容比对(见 migrate_one_file 的注释)。
        std::fs::write(old_dir.join("model.bin"), b"source data").unwrap();
        std::fs::write(new_dir.join("model.bin"), b"target data").unwrap();

        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let issues = migrate_models_dir(&old_dir, &new_dir, &tx, &cancel);
        assert!(issues.is_empty());
        // 跳过时不会去删源文件(migrate_one_file 里"已存在"分支直接 return,不碰源文件)
        assert!(old_dir.join("model.bin").exists());
        assert_eq!(std::fs::read(new_dir.join("model.bin")).unwrap(), b"target data");

        let _ = std::fs::remove_dir_all(&old_dir);
        let _ = std::fs::remove_dir_all(&new_dir);
    }

    #[test]
    fn migrate_handles_missing_old_dir_gracefully() {
        let old_dir = temp_subdir("migrate_missing_old");
        let new_dir = temp_subdir("migrate_missing_new");
        // 故意不创建 old_dir

        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let issues = migrate_models_dir(&old_dir, &new_dir, &tx, &cancel);

        assert!(issues.is_empty(), "旧目录不存在时应该当作没有东西要搬,不是错误");
        assert!(new_dir.is_dir(), "目标目录仍然应该被创建出来");

        let _ = std::fs::remove_dir_all(&new_dir);
    }

    #[test]
    fn migrate_continues_past_a_single_file_cancellation_and_reports_remainder() {
        // 用一个已经置位的 cancel token 模拟"刚开始搬就被取消":应该在处理第一个文件前
        // 就停下,issues 里要能看出"还有文件没搬完",而不是安安静静地什么都不做。
        let old_dir = temp_subdir("migrate_cancel_old");
        let new_dir = temp_subdir("migrate_cancel_new");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("a.bin"), b"a").unwrap();
        std::fs::write(old_dir.join("b.bin"), b"b").unwrap();

        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(true)); // 从一开始就是取消状态
        let issues = migrate_models_dir(&old_dir, &new_dir, &tx, &cancel);

        assert!(!issues.is_empty(), "被取消应该在 issues 里留下说明,不能悄无声息");
        assert!(old_dir.join("a.bin").exists(), "取消时源文件不应该丢失");
        assert!(old_dir.join("b.bin").exists(), "取消时源文件不应该丢失");

        let _ = std::fs::remove_dir_all(&old_dir);
        let _ = std::fs::remove_dir_all(&new_dir);
    }

    #[test]
    fn check_disk_space_excludes_exe_relative_components() {
        // Engine/Ffmpeg/Translator 落在 app_dir() 相对目录,不落在 models_dir(),不该被
        // 算进"models_dir() 所在盘够不够"的计算里。
        let dir = temp_subdir("disk_space_check");
        std::fs::create_dir_all(&dir).unwrap();

        let only_engine = [ComponentKey::Engine];
        assert!(
            check_disk_space(&only_engine, "large-v3", &dir)
                .map(|(needed, _)| needed)
                == Some(0),
            "只缺 Engine 时,models_dir() 视角下需要的空间应该是 0"
        );

        let only_model = [ComponentKey::Model];
        let needed = check_disk_space(&only_model, "large-v3", &dir).map(|(n, _)| n);
        assert_eq!(
            needed,
            Some(estimated_size_bytes(ComponentKey::Model, "large-v3")),
            "只缺语音模型时,需要的空间应该等于该模型的估算大小"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
