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
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
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
    /// 命中本地已有文件(HF 缓存,或者以后其它已知位置)且哈希校验通过,不用复制/下载,
    /// 直接把这个模型绑定到那个文件——调用方(app.rs::poll_setup)据此更新
    /// `Config.model_files` 并同步进程级绑定表(`paths::set_model_bindings`),不然要等
    /// 重启才生效,而这个模型这一轮理应立刻可用。`key` 是 `models::*_binding_key` 生成的
    /// 绑定表 key。
    ModelBound { key: String, path: PathBuf },
    Failed(String),
    AllReady,
}

mod download;
mod migrate;

pub use migrate::{migrate_models_dir, MigrateEvent};

use download::{download_and_extract_zip, download_file, sha256_file};
pub(crate) use download::{sha256_file_cancelable, to_hex};


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
pub(crate) const VAD_SHA256: &str = "29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf";

// llama.cpp release b10079,Windows CUDA 12.4 预编译版。哈希取自 GitHub Release API 返回的
// 资产 digest 字段(下载前用 API 校验过,不是凭空写的)。不随附单独的 cudart 运行库包(那个
// 包约 400MB)——translate.rs 里 llama-server 启动时复用 whisper 引擎目录已有的同版本 CUDA
// 运行库 DLL,见 translate::LlamaServer::start。
const TRANSLATOR_URL: &str =
    "https://github.com/ggml-org/llama.cpp/releases/download/b10079/llama-b10079-bin-win-cuda-12.4-x64.zip";
const TRANSLATOR_SHA256: &str = "3f94750a057e22e970e5fc6e4960dddd018baf4ed69829c84d1a2f09b5894bdd";

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
    /// 用途导向的文案,不是给开发者看的库名——普通用户不知道 whisper.cpp/llama.cpp/FFmpeg
    /// 是什么,只关心"这个东西是干什么用的"。技术名放在括号里当补充信息,不是主标题,想
    /// 深挖的用户(比如想知道具体版本、去查对应项目文档)依然能看到。
    pub fn label(&self) -> &'static str {
        match self {
            ComponentKey::Engine => "语音识别组件(whisper.cpp)",
            ComponentKey::Ffmpeg => "音频处理组件(FFmpeg)",
            ComponentKey::Model => "语音识别模型",
            ComponentKey::Vad => "语音活动检测模型",
            ComponentKey::Translator => "中文翻译组件(llama.cpp)",
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
        ComponentKey::Translator => paths::llama_server_exe().is_some(),
        ComponentKey::Model | ComponentKey::Vad | ComponentKey::TransModel(_) => {
            status_for(key, model).map(|s| s.is_ready()).unwrap_or(false)
        }
    }
}

/// `ModelStatus` 版本的 `is_present`,给"单文件模型"三种组件用(Engine/Ffmpeg/Translator
/// 是解压出的可执行文件,不是单个已知大小的文件,不适用这套)。`None` 表示传入的模型 key
/// 在注册表里查不到(比如配置文件里存了个过期/拼错的值)——这种情况不该被判定为"已就绪",
/// 交给上层(`is_present`)按"不存在"处理,后续尝试安装时 `install_component` 会给出
/// 明确的"未知标识"错误,而不是静默用一个凑巧存在的同名文件。
///
/// 是 `pub(crate)` 而不是内部私有——B6(模型清单界面)需要完整的 `ModelStatus`,不只是
/// 一个布尔值,是同一份判定逻辑的两种消费方式。
pub(crate) fn status_for(key: ComponentKey, model: &str) -> Option<crate::models::ModelStatus> {
    match key {
        ComponentKey::Model => {
            let wm = crate::models::whisper::by_key(model)?;
            let path = paths::model_path(model);
            let binding_key = crate::models::whisper_binding_key(model);
            Some(crate::models::resolve_status(&path, &binding_key, wm.size_bytes))
        }
        ComponentKey::Vad => {
            let path = paths::vad_model_path();
            Some(crate::models::resolve_status(&path, crate::models::VAD_BINDING_KEY, crate::models::VAD_SIZE_BYTES))
        }
        ComponentKey::TransModel(key) => {
            let tm = crate::translate::model_by_key(key)?;
            let path = paths::trans_model_path(tm.key, tm.file_name);
            let binding_key = crate::models::trans_binding_key(tm.key);
            Some(crate::models::resolve_status(&path, &binding_key, tm.size_bytes))
        }
        ComponentKey::Engine | ComponentKey::Ffmpeg | ComponentKey::Translator => None,
    }
}

/// 组件的估算下载大小(字节),用于下载前的磁盘空间提示。均从 GitHub Release API / HuggingFace
/// LFS 指针文件核实过的真实字节数(ffmpeg 已改为固定版本,见 FFMPEG_URL 处说明),不是拍脑袋
/// 估的;只用于提示、不做硬阻断,所以即便未来上游文件大小有细微出入也不影响功能。
pub fn estimated_size_bytes(key: ComponentKey, model: &str) -> u64 {
    match key {
        ComponentKey::Engine => 677_887_125,
        ComponentKey::Ffmpeg => 109_728_040,
        ComponentKey::Model => crate::models::whisper::by_key(model)
            .map(|m| m.size_bytes)
            .unwrap_or(3_000_000_000), // 未知档位,给一个保守估算,不影响功能只影响提示准确度
        ComponentKey::Vad => crate::models::VAD_SIZE_BYTES,
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
/// 几 GB 东西)。`translate_model` 为 "auto" 时只要求兜底模型(`translate::fallback_model`)
/// 齐全——不看日语的用户不该被迫留着 Sakura 的几个 GB。真遇到需要某个具体模型(比如日语
/// 视频要 Sakura)但那个模型没装时,不在这里提前拦截,而是走 `job.rs` 已有的降级路径:
/// `LlamaServer::start` 找不到文件会失败,这一个文件会带着"翻译引擎启动失败,已保留原文
/// 字幕"的警告完成,不影响批次里其它文件,也不阻塞用户先把其它设置调好。
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
        // 里存了个过期/拼错的 key)时按 "auto" 处理,只要求兜底模型。
        let m = if translate_model != "auto" {
            crate::translate::model_by_key(translate_model).unwrap_or_else(crate::translate::fallback_model)
        } else {
            crate::translate::fallback_model()
        };
        keys.push(ComponentKey::TransModel(m.key));
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
        ComponentKey::Model => {
            let wm = crate::models::whisper::by_key(model)
                .ok_or_else(|| format!("未知的语音识别模型标识: {model}"))?;
            install_or_bind(
                key.label(),
                crate::models::whisper_binding_key(model),
                wm.file_name,
                wm.url,
                wm.sha256,
                &paths::store_path(wm.file_name),
                tx,
                cancel,
            )
        }
        ComponentKey::Vad => install_or_bind(
            key.label(),
            crate::models::VAD_BINDING_KEY.to_string(),
            crate::models::VAD_FILE_NAME,
            VAD_URL,
            Some(VAD_SHA256),
            &paths::store_path(crate::models::VAD_FILE_NAME),
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
            install_or_bind(
                tm.label,
                crate::models::trans_binding_key(tm.key),
                tm.file_name,
                tm.url,
                Some(tm.sha256),
                &paths::store_path(tm.file_name),
                tx,
                cancel,
            )
        }
    }
}

/// 决定"在本地已知位置找到一份候选文件"之后该怎么办的纯逻辑,不碰文件系统——校验结果
/// 由调用方(`install_or_bind`)算好传进来。哈希验证通过就绑定,不用复制/下载;没有候选、
/// 或者有候选但校验不通过(哈希不一致、没有权威哈希可比对、读取失败),都老实走下载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InstallAction {
    BindExisting(PathBuf),
    Download,
}

pub(super) fn plan_install(candidate: Option<&Path>, hash_ok: bool) -> InstallAction {
    match candidate {
        Some(p) if hash_ok => InstallAction::BindExisting(p.to_path_buf()),
        _ => InstallAction::Download,
    }
}

/// 安装一个"存放目录里没有、但可能已经在本地其它已知位置(HF 缓存等)存在"的模型文件
/// ——命中候选且哈希验证通过就发一个 `ModelBound` 事件绑定过去,不占用双份磁盘空间
/// (真实踩过的坑:用户用别的工具已经下载过 Sakura 7B,之前的实现会不管三七二十一
/// 复制一份进 subgen 自己的目录,同一个 3.96GB 的文件在磁盘上存两份)。缓存里没有、
/// 或哈希对不上,才真正走网络下载。
///
/// `expected_sha256` 是 `None` 时(理论上不该再发生,现在三种模型都已经有权威哈希了,
/// 保留这条分支给以后可能出现的、还没来得及核实哈希的模型)不采信任何本地候选,直接下载
/// ——没有校验依据就绑定一个未知来源的文件,风险比多下载几 GB 更大。
#[allow(clippy::too_many_arguments)]
fn install_or_bind(
    label: &str,
    binding_key: String,
    file_name: &str,
    url: &str,
    expected_sha256: Option<&str>,
    download_target: &Path,
    tx: &Sender<SetupEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let candidate = find_in_known_roots(file_name);
    let hash_ok = match (&candidate, expected_sha256) {
        (Some(p), Some(expected)) => {
            let _ = tx.send(SetupEvent::Log(format!("在本地已有文件中找到 {label},校验哈希中...")));
            match sha256_file(p) {
                Ok(digest) => to_hex(&digest).eq_ignore_ascii_case(expected),
                Err(e) => {
                    let _ = tx.send(SetupEvent::Log(format!("读取 {label} 本地文件失败({e}),改为重新下载")));
                    false
                }
            }
        }
        (Some(_), None) => {
            let _ = tx.send(SetupEvent::Log(format!(
                "{label} 没有可比对的权威哈希,本地文件不采信,改为重新下载"
            )));
            false
        }
        (None, _) => false,
    };
    if candidate.is_some() && !hash_ok {
        let _ = tx.send(SetupEvent::Log(format!("{label} 本地文件哈希不一致,改为重新下载")));
    }

    match plan_install(candidate.as_deref(), hash_ok) {
        InstallAction::BindExisting(path) => {
            let _ = tx.send(SetupEvent::ModelBound { key: binding_key, path });
            let _ = tx.send(SetupEvent::ComponentReady(label.to_string()));
            Ok(())
        }
        InstallAction::Download => {
            if let Some(parent) = download_target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
            }
            download_file(label, url, expected_sha256, true, download_target, tx, cancel)
        }
    }
}

/// 在本地已知位置按文件名查找候选文件。目前只有一类已知位置:HuggingFace 本地缓存
/// ——依次检查 HF_HOME、HF_HUB_CACHE 环境变量指向的目录,以及 huggingface_hub 默认位置
/// (`~/.cache/huggingface/hub`),都是 huggingface_hub 官方文档记录的标准缓存位置解析
/// 顺序,不是猜的路径。以前只有翻译模型会走这个查找,现在 whisper/VAD 模型也复用同一套
/// (见项目计划 B4)——没道理翻译模型能省下重复下载,语音模型就不能。
pub(crate) fn find_in_known_roots(file_name: &str) -> Option<PathBuf> {
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


#[cfg(test)]
mod tests {
    use super::*;

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
    fn plan_install_binds_when_candidate_found_and_hash_ok() {
        let candidate = PathBuf::from(r"F:\hf_cache\sakura-7b.gguf");
        assert_eq!(
            plan_install(Some(&candidate), true),
            InstallAction::BindExisting(candidate)
        );
    }

    #[test]
    fn plan_install_downloads_when_no_candidate() {
        assert_eq!(plan_install(None, false), InstallAction::Download);
        // hash_ok 在没有候选文件时没有意义,不该影响结果。
        assert_eq!(plan_install(None, true), InstallAction::Download);
    }

    #[test]
    fn plan_install_downloads_when_candidate_hash_mismatches() {
        let candidate = PathBuf::from(r"F:\hf_cache\corrupted.gguf");
        assert_eq!(plan_install(Some(&candidate), false), InstallAction::Download);
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
