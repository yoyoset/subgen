//! 所有路径默认相对于可执行文件所在目录,保证程序可以被整体拷贝到任意位置("绿色"要求)。
//! 唯一例外是 `models_dir()`:模型文件动辄几个 GB 到十几个 GB,不应该被迫跟着 exe 摆放的
//! 位置走(很多人图方便把 exe 放 C 盘,C 盘往往是容量较小的系统盘),所以支持用户显式指定
//! 一个覆盖目录,见 `set_models_dir_override`。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 程序所在目录(不是当前工作目录 —— 双击运行或从任意目录启动都要能找到自己的文件)。
pub fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 进程级的模型目录覆盖值,None = 用默认值。main.rs 启动时用 Config 里读到的值设置一次;
/// 用户在界面里改存放位置、迁移成功后重新设置。用 Mutex 而不是一次性的 OnceLock,是因为
/// 这个值在一次程序运行期间可能被用户改多次(不止首次设置这一次)。
static MODELS_DIR_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn set_models_dir_override(path: Option<PathBuf>) {
    if let Ok(mut guard) = MODELS_DIR_OVERRIDE.lock() {
        *guard = path;
    }
}

pub fn models_dir_override() -> Option<PathBuf> {
    MODELS_DIR_OVERRIDE.lock().ok().and_then(|g| g.clone())
}

/// 进程级的模型绑定表(见 `Config.model_files`),和 `MODELS_DIR_OVERRIDE` 是同一种模式:
/// main.rs 启动时用配置里读到的值设置一次,以后用户通过"定位…"确认一个绑定时再更新。
static MODEL_BINDINGS: Mutex<std::collections::BTreeMap<String, PathBuf>> =
    Mutex::new(std::collections::BTreeMap::new());

pub fn set_model_bindings(bindings: std::collections::BTreeMap<String, PathBuf>) {
    if let Ok(mut guard) = MODEL_BINDINGS.lock() {
        *guard = bindings;
    }
}

/// 按绑定表 key(见 `models::whisper_binding_key` 等)查显式绑定的路径,不检查文件是否
/// 还真的存在——调用方(`model_path` 等)已经在做"路径解析优先级"的编排,存在性检查统一
/// 交给它们做,这里只负责回答"有没有绑定"。
pub fn model_binding(key: &str) -> Option<PathBuf> {
    MODEL_BINDINGS.lock().ok().and_then(|g| g.get(key).cloned())
}

pub fn engine_dir() -> PathBuf {
    app_dir().join("engine")
}

pub fn ffmpeg_dir() -> PathBuf {
    app_dir().join("ffmpeg")
}

pub fn translator_dir() -> PathBuf {
    app_dir().join("translator")
}

pub fn models_dir() -> PathBuf {
    models_dir_override().unwrap_or_else(|| app_dir().join("models"))
}

pub fn logs_dir() -> PathBuf {
    app_dir().join("logs")
}

pub fn tmp_dir() -> PathBuf {
    app_dir().join("tmp")
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.toml")
}

/// whisper.cpp 官方发布包解压后内部目录结构不完全固定(历史上出现过 Release/ 前缀等),
/// 所以不硬编码内部路径,改为递归搜索目标文件名。这样即使以后 whisper.cpp 发布包内部结构变化,
/// 只要文件名不变,程序依然能找到。只要第一个命中就返回——是 `find_all_matching_files`
/// 的薄包装,大多数调用方只关心"有没有、在哪",不需要知道是不是有多份。
pub fn find_file_recursive(dir: &Path, file_name: &str) -> Option<PathBuf> {
    find_all_matching_files(dir, file_name).into_iter().next()
}

/// 和 `find_file_recursive` 一样递归搜索,但收集**全部**命中而不是找到第一个就停——用于
/// 检测重复文件(见 `models` 模块的清单"清理重复"功能):同一个模型的文件如果在存放目录
/// 树里出现不止一份,这是找出"除了正在用的那份之外还有哪些"的基础。
///
/// 用规范化后的目录路径去重,防止 Windows 符号链接/目录联接(Junction)构成环时无限遍历
/// (真实的用户缓存目录里可能出现指向自身祖先的 junction,不加防护会让整个遍历死循环,
/// 而遍历函数内部不检查取消标志——batch worker 会卡死且无法打断)。
pub fn find_all_matching_files(dir: &Path, file_name: &str) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    // 同一个真实目录无论从几条路径到达,只展开一次。
    let mut visited: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    while let Some(current) = stack.pop() {
        // 规范化失败(比如目录被并发删除)就退回原路径,不影响继续遍历。
        let canonical = std::fs::canonicalize(&current).unwrap_or_else(|_| current.clone());
        if !visited.insert(canonical) {
            continue;
        }
        let entries = match std::fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // 不进入 `_trash` 子目录:那是"清理重复"移进去的软删除区(见 models::actions.rs
            // 的 begin_cleanup_duplicates),里面是用户已经确认不要的副本,不该再被当作
            // "存放目录里还有一份重复文件"的候选扫出来。不加这个排除的话,清理完再扫一次
            // 会把 _trash 里的文件当成新发现的重复,用户点"清理"又把它们从 _trash 挪到
            // _trash(加数字后缀),越清越多。
            let is_trash_dir = path.is_dir()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n == "_trash")
                    .unwrap_or(false);
            if path.is_dir() && !is_trash_dir {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.eq_ignore_ascii_case(file_name))
                .unwrap_or(false)
            {
                found.push(path);
            }
        }
    }
    found
}

pub fn whisper_cli_exe() -> Option<PathBuf> {
    find_file_recursive(&engine_dir(), "whisper-cli.exe")
}

pub fn ffmpeg_exe() -> Option<PathBuf> {
    find_file_recursive(&ffmpeg_dir(), "ffmpeg.exe")
}

pub fn llama_server_exe() -> Option<PathBuf> {
    find_file_recursive(&translator_dir(), "llama-server.exe")
}

/// 在模型存放目录里查找 `file_name` 对应的实际文件,不假设 `models_dir()` 内部是平铺
/// 结构(这样 `models_dir_override` 可以直接指向用户自己的 HuggingFace 缓存目录,比如
/// `F:\hf_cache`,内部是 `hub/models--xxx/snapshots/<commit>/文件名` 这种嵌套结构,和
/// 别的工具共用,不用再复制一份进 subgen 自己的目录)。只回答"现在在哪"这一个问题,
/// 找不到返回 `None`——不像旧版 `model_path` 等函数那样在找不到时顺带算出一个"如果要
/// 下载该放哪"的路径,那是 `store_path` 单独的职责(下面),两件事分开才不会在后面加
/// "绑定路径"(见项目计划 B3)时把"解析"和"这个模型该被装到哪"这两件事继续混在一起。
pub fn resolve_in_store(file_name: &str) -> Option<PathBuf> {
    find_file_recursive(&models_dir(), file_name)
}

/// `file_name` 应该被写入(下载/复制)到的位置——不检查文件是否已存在,单纯是"新文件该
/// 放哪"这个问题的答案。新装/更新模型时的写入目标用这个,不要和 `resolve_in_store` 的
/// 返回值混用。
pub fn store_path(file_name: &str) -> PathBuf {
    models_dir().join(file_name)
}

/// whisper 语音识别模型路径:先在存放目录里找,找不到就退回"如果要下载该放哪"
/// (`store_path`)——这个退回值同时也是 `setup.rs` 下载新模型时的写入目标,语义上依然
/// 正确,不需要另外维护一套"下载目标路径"。
pub fn model_path(model: &str) -> PathBuf {
    if let Some(bound) = model_binding(&crate::models::whisper_binding_key(model)) {
        return bound;
    }
    // 已注册的模型用注册表里的 file_name(单一事实来源);未注册的 key(比如配置文件里
    // 存了个过期/拼错的值)退回原来的模板,行为和收拢进注册表之前完全一致。
    let file_name = crate::models::whisper::by_key(model)
        .map(|m| m.file_name.to_string())
        .unwrap_or_else(|| format!("ggml-{model}.bin"));
    resolve_in_store(&file_name).unwrap_or_else(|| store_path(&file_name))
}

pub fn vad_model_path() -> PathBuf {
    if let Some(bound) = model_binding(crate::models::VAD_BINDING_KEY) {
        return bound;
    }
    resolve_in_store(crate::models::VAD_FILE_NAME).unwrap_or_else(|| store_path(crate::models::VAD_FILE_NAME))
}

/// 翻译模型(gguf)文件路径。`key` 是 translate::MODELS 里的模型标识(如 "sakura-7b"),
/// 用来查绑定表;`file_name` 是没有绑定时用来在存放目录里搜索/计算下载目标的文件名
/// (调用方传 `TransModel.file_name`)。两个参数分开是因为绑定表按稳定的模型标识
/// (不随文件名变化)存,不能只拿 file_name 反查。
pub fn trans_model_path(key: &str, file_name: &str) -> PathBuf {
    if let Some(bound) = model_binding(&crate::models::trans_binding_key(key)) {
        return bound;
    }
    resolve_in_store(file_name).unwrap_or_else(|| store_path(file_name))
}

/// 视频文件名同名的字幕输出路径,如 EpisodeName.mkv -> EpisodeName.srt
pub fn sidecar_srt_path(video_path: &Path) -> PathBuf {
    video_path.with_extension("srt")
}

/// 带语言后缀的字幕输出路径,如 EpisodeName.mkv + "zh" -> EpisodeName.zh.srt
pub fn sidecar_srt_lang_path(video_path: &Path, lang: &str) -> PathBuf {
    let stem = video_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    video_path.with_file_name(format!("{stem}.{lang}.srt"))
}

/// 翻译复核报告的输出路径,如 EpisodeName.mkv -> EpisodeName.zh.review.txt
pub fn sidecar_review_path(video_path: &Path) -> PathBuf {
    let stem = video_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    video_path.with_file_name(format!("{stem}.zh.review.txt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 外挂字幕/复核报告的输出路径必须严格跟随视频文件名,这是播放器自动加载字幕的
    /// 前提(同目录同名)。带点、带空格、无扩展名的文件都要有稳定的结果。
    #[test]
    fn sidecar_paths_follow_video_naming() {
        let video = Path::new(r"E:\videos\Episode 5.mkv");
        assert_eq!(sidecar_srt_path(video), Path::new(r"E:\videos\Episode 5.srt"));
        assert_eq!(
            sidecar_srt_lang_path(video, "zh"),
            Path::new(r"E:\videos\Episode 5.zh.srt")
        );
        assert_eq!(
            sidecar_review_path(video),
            Path::new(r"E:\videos\Episode 5.zh.review.txt")
        );

        let bare = Path::new(r"E:\videos\noext");
        assert_eq!(sidecar_srt_path(bare), Path::new(r"E:\videos\noext.srt"));
    }

    /// 不碰 MODELS_DIR_OVERRIDE(参数直接传目录,不经过全局状态),可以独立成测试,
    /// 不用并进下面那个大测试函数里。
    #[test]
    fn find_all_matching_files_collects_every_match_not_just_first() {
        let dir = std::env::temp_dir().join(format!(
            "subgen_test_find_all_{:x}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let nested = dir.join("nested").join("deeper");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("model.gguf"), b"flat copy").unwrap();
        std::fs::write(nested.join("model.gguf"), b"nested copy").unwrap();
        std::fs::write(dir.join("other.gguf"), b"different file").unwrap();

        let mut found = find_all_matching_files(&dir, "model.gguf");
        found.sort();
        assert_eq!(found.len(), 2, "应该找到两份同名文件,不是只找到第一份: {found:?}");
        assert!(found.contains(&dir.join("model.gguf")));
        assert!(found.contains(&nested.join("model.gguf")));

        // find_file_recursive 应该还是只返回其中一个(行为没变,只是现在是薄包装)。
        assert!(find_file_recursive(&dir, "model.gguf").is_some());
        assert_eq!(find_all_matching_files(&dir, "does-not-exist.gguf"), Vec::<PathBuf>::new());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// "清理重复"把文件移进 `_trash` 之后,下次扫描不该再把 _trash 里的文件当成新发现的
    /// 重复(否则清理一次 → 又发现一次 → 再清理,越清越多)。`find_all_matching_files`
    /// 跳过 `_trash` 子目录。
    #[test]
    fn find_all_matching_files_skips_trash_subdirectory() {
        let dir = std::env::temp_dir().join(format!(
            "subgen_test_trash_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let trash = dir.join("_trash");
        std::fs::create_dir_all(&trash).unwrap();
        std::fs::write(dir.join("model.gguf"), b"in use").unwrap();
        std::fs::write(trash.join("model.gguf"), b"cleaned up").unwrap();

        let found = find_all_matching_files(&dir, "model.gguf");
        assert_eq!(
            found,
            vec![dir.join("model.gguf")],
            "_trash 里的副本不该被当成重复扫出来: {found:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows 目录联接(Junction)可能构成环(把子目录指回自己的祖先)。旧实现会无限遍历,
    /// 让 batch worker 卡死且无法取消;现在用规范化路径去重,遇到环必须终止。真实创建
    /// junction 验证(mklink /J 不需要管理员权限);环境不支持时跳过(逻辑正确性由 visited-set 保证)。
    #[test]
    fn find_all_matching_files_terminates_on_junction_cycle() {
        let base = std::env::temp_dir().join(format!(
            "subgen_junction_cycle_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("target.bin"), b"x").unwrap();
        let loop_link = real.join("loop");
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&loop_link)
            .arg(&base)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !created {
            // junction 创建失败(受限环境),跳过——正常路径的行为由上面的测试覆盖。
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        // base/real/loop 指回 base,形成 base -> real -> loop(base) 的环。
        // 遍历必须终止,而且 target.bin 仍然能找到。
        let found = find_all_matching_files(&base, "target.bin");
        assert!(!found.is_empty(), "穿过 junction 环也应该能找到 target.bin");

        // 先拆掉 junction 再整体删除,避免 remove_dir_all 顺着环走。
        let _ = std::fs::remove_dir(&loop_link);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 合并成一个测试而不是拆成好几个:MODELS_DIR_OVERRIDE 是进程级全局状态,cargo test
    /// 默认并行跑同一个二进制里的测试,拆开写会互相踩状态导致偶发失败(真实踩过一次:
    /// 这个测试和另一个单独写的"嵌套目录查找"测试并行跑时,后者设置的覆盖值被这个测试的
    /// 断言看到了,断言失败但其实两边逻辑都是对的,只是被并行执行搅在一起)。所有跟
    /// MODELS_DIR_OVERRIDE 有关的场景都合并到这一个测试函数里,才能保证不被其它测试打断。
    #[test]
    fn models_dir_override_behavior() {
        set_models_dir_override(None);
        assert_eq!(
            models_dir(),
            app_dir().join("models"),
            "未设置覆盖值时应该回退到默认位置(exe 同目录下的 models/)"
        );

        let custom = PathBuf::from(r"Z:\somewhere\custom_models");
        set_models_dir_override(Some(custom.clone()));
        assert_eq!(models_dir(), custom, "设置了覆盖值后,models_dir() 应该读到新值");
        // 下游路径函数应该自动跟着覆盖值走,不需要单独适配(这个目录本身不存在,
        // find_file_recursive 会直接返回 None,正确退回平铺路径)
        assert_eq!(model_path("large-v3"), custom.join("ggml-large-v3.bin"));
        assert_eq!(vad_model_path(), custom.join("ggml-silero-v5.1.2.bin"));
        assert_eq!(trans_model_path("qwen3-14b", "Qwen3-14B-Q4_K_M.gguf"), custom.join("Qwen3-14B-Q4_K_M.gguf"));
        // resolve_in_store/store_path 是上面几个函数拆出来的两块——目录不存在时,
        // resolve_in_store 该老实返回 None(不是"猜"出一个路径),store_path 只管算写入
        // 目标、不检查存在与否。
        assert_eq!(resolve_in_store("ggml-large-v3.bin"), None);
        assert_eq!(store_path("ggml-large-v3.bin"), custom.join("ggml-large-v3.bin"));

        // 模拟 HuggingFace 缓存那种嵌套结构(hub/models--xxx/snapshots/<commit>/文件),
        // 确认能在深层目录里找到文件,不要求平铺——这样 models_dir_override 才能直接指向
        // 用户自己的 HF 缓存目录,和别的工具共用,不用再复制一份。
        let nested_root = std::env::temp_dir().join(format!(
            "subgen_test_hf_cache_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sakura_dir = nested_root
            .join("hub")
            .join("models--SakuraLLM--Sakura-7B-Qwen2.5-v1.0-GGUF")
            .join("snapshots")
            .join("24393225dfab4750c6fb9ec2468fb76ad3d2dd1d");
        std::fs::create_dir_all(&sakura_dir).unwrap();
        let sakura_file = sakura_dir.join("sakura-7b-qwen2.5-v1.0-iq4xs.gguf");
        std::fs::write(&sakura_file, b"fake gguf content").unwrap();

        let whisper_dir = nested_root.join("hub").join("models--ggerganov--whisper.cpp");
        std::fs::create_dir_all(&whisper_dir).unwrap();
        let large_v3_file = whisper_dir.join("ggml-large-v3.bin");
        std::fs::write(&large_v3_file, b"fake model content").unwrap();
        let vad_dir = nested_root.join("hub").join("models--ggml-org--whisper-vad");
        std::fs::create_dir_all(&vad_dir).unwrap();
        let vad_file = vad_dir.join("ggml-silero-v5.1.2.bin");
        std::fs::write(&vad_file, b"fake vad content").unwrap();

        set_models_dir_override(Some(nested_root.clone()));
        assert_eq!(
            trans_model_path("sakura-7b", "sakura-7b-qwen2.5-v1.0-iq4xs.gguf"),
            sakura_file,
            "应该在嵌套目录里找到翻译模型,而不是退回平铺路径"
        );
        assert_eq!(model_path("large-v3"), large_v3_file, "应该在嵌套目录里找到语音模型");
        assert_eq!(vad_model_path(), vad_file, "应该在嵌套目录里找到 VAD 模型");
        assert_eq!(
            resolve_in_store("ggml-large-v3.bin"),
            Some(large_v3_file.clone()),
            "resolve_in_store 应该能直接在嵌套目录里找到文件"
        );
        assert_eq!(
            trans_model_path("qwen3-14b", "Qwen3-14B-not-downloaded-yet.gguf"),
            nested_root.join("Qwen3-14B-not-downloaded-yet.gguf"),
            "找不到时应该退回平铺路径(下载新模型要用这个当写入目标),而不是 panic"
        );

        // 绑定表优先于递归搜索/平铺退回路径——用户显式定位过的文件不该被"猜"出来的结果
        // 覆盖,哪怕绑定的路径和递归搜索本来会找到的结果不是同一个文件。
        let mut bindings = std::collections::BTreeMap::new();
        let explicit = nested_root.join("explicitly_located_sakura.gguf");
        bindings.insert(crate::models::trans_binding_key("sakura-7b"), explicit.clone());
        set_model_bindings(bindings);
        assert_eq!(
            trans_model_path("sakura-7b", "sakura-7b-qwen2.5-v1.0-iq4xs.gguf"),
            explicit,
            "有绑定时应该直接用绑定路径,不再走递归搜索"
        );
        // 没有绑定的模型不受影响,继续走原来的解析顺序。
        assert_eq!(model_path("large-v3"), large_v3_file, "没有绑定的模型不该被绑定表影响");
        set_model_bindings(std::collections::BTreeMap::new());

        let _ = std::fs::remove_dir_all(&nested_root);

        set_models_dir_override(None);
        assert_eq!(
            models_dir(),
            app_dir().join("models"),
            "清除覆盖值后应该重新回退到默认位置"
        );
    }
}
