//! 模型定义的统一入口。眼下只有 whisper 语音识别模型收拢进了这里(见 `whisper.rs`)——
//! 翻译模型的注册表(`TransModel`/`translate::MODELS`)本来就是干净的单一注册表,不需要
//! 挪动。这个模块是 Track B(模型管理重构)剩余步骤(绑定表/清单界面)的落脚点。

use std::path::{Path, PathBuf};

pub mod whisper;

/// 一个模型现在处于什么状态,取代旧版 `setup::is_present` 的
/// "`is_file() && len > 1_000_000`" 这种只挡得住"完全没文件"的粗糙判断——一个被截断到
/// 900KB 还是 2.9GB 的下载,旧判断一样会说"存在",新的会精确识别成 `SizeMismatch`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    Ready { path: PathBuf, size: u64 },
    Missing,
    /// 绑定表里有这条记录(见 `Config.model_files`),但绑定的文件现在已经不存在了
    /// (可能被移动、改名或删除)。和 `Missing`(从来没装过/没绑定过)分开,是因为界面上
    /// 该给用户的提示不一样——"文件不见了,重新定位一下"vs"还没装"。
    PathBroken { path: PathBuf },
    /// 文件存在,但大小和注册表里记录的精确值不一致——可能是下载中断被截断,也可能是
    /// 绑定指到了同名但内容不同的文件。
    SizeMismatch { path: PathBuf, expected: u64, actual: u64 },
}

impl ModelStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, ModelStatus::Ready { .. })
    }

    /// 这个状态携带的路径,`Missing` 没有路径可言(从来没找到过任何候选)。清单界面用它
    /// 决定"在文件夹中显示"/"复制路径"这两个操作要不要启用。
    pub fn path(&self) -> Option<&Path> {
        match self {
            ModelStatus::Ready { path, .. }
            | ModelStatus::PathBroken { path }
            | ModelStatus::SizeMismatch { path, .. } => Some(path),
            ModelStatus::Missing => None,
        }
    }
}

/// 纯函数:根据"解析到的路径"、"这个路径是不是来自显式绑定"、"文件当前实际大小(不存在则
/// `None`)"、"注册表里记录的期望大小"这四个已经算好的输入,判定状态。不碰文件系统——
/// 调用方(`resolve_status`)负责做真正的 I/O,这里只负责判断逻辑,方便单测覆盖每种组合
/// 而不用真的摆一堆测试文件。
pub fn status_of(path: &Path, is_binding: bool, actual_size: Option<u64>, expected_size: u64) -> ModelStatus {
    match actual_size {
        Some(size) if size == expected_size => ModelStatus::Ready { path: path.to_path_buf(), size },
        Some(size) => ModelStatus::SizeMismatch { path: path.to_path_buf(), expected: expected_size, actual: size },
        None if is_binding => ModelStatus::PathBroken { path: path.to_path_buf() },
        None => ModelStatus::Missing,
    }
}

/// `status_of` 的不纯包装:真的去查文件系统(`std::fs::metadata`)和进程级绑定表
/// (`paths::model_binding`),拼出纯函数需要的输入。
pub fn resolve_status(path: &Path, binding_key: &str, expected_size: u64) -> ModelStatus {
    let is_binding = crate::paths::model_binding(binding_key).is_some();
    let actual_size = std::fs::metadata(path).ok().map(|m| m.len());
    status_of(path, is_binding, actual_size, expected_size)
}

/// 模型绑定表(`Config.model_files`,见 config.rs)用的稳定字符串 key。三种模型各自一份
/// 独立的命名空间前缀,避免 whisper 的 "large-v3" 和以后可能出现的同名翻译模型 key 混淆。
/// 用格式化字符串而不是一个 `ModelId` 枚举——绑定表的 key 只是个查找用的字符串,不需要
/// 携带 `&'static` 生命周期(用户手动定位的模型文件对应的 key 来自运行时输入,不是注册表
/// 常量),没必要为此背上生命周期设计的负担。
pub fn whisper_binding_key(model_key: &str) -> String {
    format!("whisper:{model_key}")
}

pub fn trans_binding_key(model_key: &str) -> String {
    format!("trans:{model_key}")
}

pub const VAD_BINDING_KEY: &str = "vad";

/// VAD(语音活动检测)模型是单例,不像 whisper/翻译模型那样有多个可选档位,不需要一整个
/// 注册表——但文件名之前在 paths.rs 和 setup/mod.rs 两处各写了一份字面量(见项目计划里
/// 记录的这处重复),这里收成唯一一份,两边都改成引用它。
pub const VAD_FILE_NAME: &str = "ggml-silero-v5.1.2.bin";

/// VAD 模型的精确期望字节数,来自 HuggingFace LFS 指针核实过的真实值。之前在
/// `setup::estimated_size_bytes` 里是个内联字面量,`ModelStatus` 需要同一个数字做精确
/// 大小比对,收成一份常量,两处共用。
pub const VAD_SIZE_BYTES: u64 = 885_098;

/// "定位…"选中一个文件后,先做的廉价类型嗅探——不解析 GGUF/ggml 完整文件头(那里没有
/// `prompt_kind`/`langs` 这些真正需要的信息,解析了也白解析,见项目计划里的决定 2),
/// 只看前 4 个字节的魔数,挡掉"选错文件类型"这种最基本的误操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// whisper.cpp 的 ggml 格式(语音识别模型、VAD 模型都是这个格式)。
    Ggml,
    /// llama.cpp 的 GGUF 格式(翻译模型)。
    Gguf,
}

/// whisper.cpp 的 `GGML_FILE_MAGIC` 是 `0x67676d6c`,按小端字节序写在磁盘上就是这四个
/// 字节(ASCII 读作 "lmgg")——**已用真实文件核实**(`F:\hf_cache\ggml-large-v3.bin` 和
/// `ggml-silero-v5.1.2.bin` 的前 4 字节都是这个值,hexdump 核对过,不是凭记忆写的字节序)。
const GGML_MAGIC: [u8; 4] = [0x6c, 0x6d, 0x67, 0x67];
/// GGUF 格式的魔数就是 ASCII "GGUF" 四个字符,同样用真实文件核实过
/// (`Qwen3-4B-Instruct-2507-Q4_K_M.gguf` 和 `sakura-7b-qwen2.5-v1.0-iq4xs.gguf` 都是)。
const GGUF_MAGIC: [u8; 4] = *b"GGUF";

/// 纯函数:看前 4 个字节判断文件类型,两种已知魔数之外的一律返回 `None`(不认识,不猜测)。
pub fn sniff_kind(magic: [u8; 4]) -> Option<FileKind> {
    match magic {
        GGML_MAGIC => Some(FileKind::Ggml),
        GGUF_MAGIC => Some(FileKind::Gguf),
        _ => None,
    }
}

/// "定位…"选中一个文件后、真正读取哈希之前的粗筛结果。`Plausible` 只表示"类型和大小都对
/// 得上,值得花时间去算哈希",不代表内容一定对——最终认定还是要看哈希比对结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickOutcome {
    Plausible,
    WrongKind,
    SizeMismatch { expected: u64, actual: u64 },
}

/// "清理重复"要把确认过的重复文件移到 `models_dir/_trash/`,而不是直接删除(用户要求
/// "先不去重,方案完善好再删除"——移到 _trash 是"完善好"之后的版本,依然是可撤销的软删除,
/// 不是真删)。纯函数:如果目标文件名在 _trash 里还没有,直接用原文件名;已经有同名的
/// (比如清理过两次、或者两个不同来源但同名的文件),加数字后缀直到不冲突,不覆盖已经在
/// _trash 里的东西。
pub fn trash_destination(
    trash_dir: &std::path::Path,
    file_name: &str,
    already_in_trash: &std::collections::HashSet<String>,
) -> PathBuf {
    if !already_in_trash.contains(file_name) {
        return trash_dir.join(file_name);
    }
    let mut n = 1u32;
    loop {
        let candidate = format!("{n}_{file_name}");
        if !already_in_trash.contains(&candidate) {
            return trash_dir.join(candidate);
        }
        n += 1;
    }
}

/// 这个文件名是不是"subgen 认得"的模型文件——判断依据是三个注册表(whisper::MODELS/
/// VAD/translate::MODELS)本身,不是另外单独维护的一份名单,不会跟注册表脱节。
/// `setup::migrate_models_dir` 用它决定"旧目录里哪些文件是 subgen 自己的、该跟着搬"
/// ——目录可能是用户多个工具共用的缓存(比如指向 HF_HOME),不该把别的工具的文件也搬走。
pub fn is_known_model_file_name(file_name: &str) -> bool {
    whisper::MODELS.iter().any(|m| m.file_name == file_name)
        || file_name == VAD_FILE_NAME
        || crate::translate::MODELS.iter().any(|m| m.file_name == file_name)
}

/// 纯函数:根据选中文件的魔数+实际大小,和这一行模型期望的类型+大小比对。
pub fn classify_picked_file(
    magic: [u8; 4],
    actual_size: u64,
    expected_kind: FileKind,
    expected_size: u64,
) -> PickOutcome {
    match sniff_kind(magic) {
        Some(kind) if kind == expected_kind => {
            if actual_size == expected_size {
                PickOutcome::Plausible
            } else {
                PickOutcome::SizeMismatch { expected: expected_size, actual: actual_size }
            }
        }
        _ => PickOutcome::WrongKind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_keys_are_namespaced_and_dont_collide() {
        assert_eq!(whisper_binding_key("large-v3"), "whisper:large-v3");
        assert_eq!(trans_binding_key("large-v3"), "trans:large-v3");
        assert_ne!(whisper_binding_key("large-v3"), trans_binding_key("large-v3"));
        assert_ne!(whisper_binding_key("vad"), VAD_BINDING_KEY);
    }

    #[test]
    fn status_of_ready_when_size_matches_exactly() {
        let path = PathBuf::from(r"F:\models\ggml-large-v3.bin");
        let status = status_of(&path, false, Some(3_095_033_483), 3_095_033_483);
        assert_eq!(status, ModelStatus::Ready { path, size: 3_095_033_483 });
    }

    #[test]
    fn status_of_size_mismatch_when_truncated() {
        // 真实踩过的坑(推演):旧版 is_present 只检查 "> 1_000_000",一个被截断到 2GB 的
        // large-v3(期望 2.9GB)会被误判为"已就绪"。
        let path = PathBuf::from(r"F:\models\ggml-large-v3.bin");
        let status = status_of(&path, false, Some(2_000_000_000), 3_095_033_483);
        assert_eq!(
            status,
            ModelStatus::SizeMismatch { path, expected: 3_095_033_483, actual: 2_000_000_000 }
        );
    }

    #[test]
    fn status_of_missing_when_no_binding_and_no_file() {
        let path = PathBuf::from(r"F:\models\ggml-large-v3.bin");
        assert_eq!(status_of(&path, false, None, 3_095_033_483), ModelStatus::Missing);
    }

    #[test]
    fn status_of_path_broken_when_bound_but_file_gone() {
        // 有绑定记录、但文件不在了(被移动/删除)——和"从来没装过"要能区分开,界面上
        // 该给的提示不一样。
        let path = PathBuf::from(r"F:\hf_cache\moved_away.gguf");
        let status = status_of(&path, true, None, 3_095_033_483);
        assert_eq!(status, ModelStatus::PathBroken { path });
    }

    #[test]
    fn sniff_kind_recognizes_real_ggml_magic_from_hexdump() {
        // 真实核实过的字节(见 GGML_MAGIC 处注释):F:\hf_cache\ggml-large-v3.bin 和
        // ggml-silero-v5.1.2.bin 的前 4 字节都是 6c 6d 67 67。
        assert_eq!(sniff_kind([0x6c, 0x6d, 0x67, 0x67]), Some(FileKind::Ggml));
    }

    #[test]
    fn sniff_kind_recognizes_real_gguf_magic_from_hexdump() {
        // 真实核实过的字节:Qwen3-4B-Instruct-2507-Q4_K_M.gguf 和
        // sakura-7b-qwen2.5-v1.0-iq4xs.gguf 的前 4 字节都是 47 47 55 46("GGUF")。
        assert_eq!(sniff_kind([0x47, 0x47, 0x55, 0x46]), Some(FileKind::Gguf));
    }

    #[test]
    fn sniff_kind_returns_none_for_unknown_magic() {
        assert_eq!(sniff_kind([0, 0, 0, 0]), None);
        assert_eq!(sniff_kind(*b"RIFF"), None); // 比如误选了个 wav/avi 文件
    }

    #[test]
    fn trash_destination_uses_original_name_when_no_collision() {
        let trash_dir = PathBuf::from(r"F:\hf_cache\_trash");
        let existing = std::collections::HashSet::new();
        assert_eq!(
            trash_destination(&trash_dir, "sakura-7b.gguf", &existing),
            trash_dir.join("sakura-7b.gguf")
        );
    }

    #[test]
    fn trash_destination_appends_counter_on_collision() {
        let trash_dir = PathBuf::from(r"F:\hf_cache\_trash");
        let mut existing = std::collections::HashSet::new();
        existing.insert("sakura-7b.gguf".to_string());
        assert_eq!(
            trash_destination(&trash_dir, "sakura-7b.gguf", &existing),
            trash_dir.join("1_sakura-7b.gguf")
        );

        existing.insert("1_sakura-7b.gguf".to_string());
        assert_eq!(
            trash_destination(&trash_dir, "sakura-7b.gguf", &existing),
            trash_dir.join("2_sakura-7b.gguf")
        );
    }

    #[test]
    fn is_known_model_file_name_recognizes_all_three_registries() {
        assert!(is_known_model_file_name("ggml-large-v3.bin"));
        assert!(is_known_model_file_name(VAD_FILE_NAME));
        assert!(is_known_model_file_name("sakura-7b-qwen2.5-v1.0-iq4xs.gguf"));
        assert!(is_known_model_file_name("Qwen3-4B-Instruct-2507-Q4_K_M.gguf"));
    }

    #[test]
    fn is_known_model_file_name_rejects_unrelated_files() {
        // 真实场景:models_dir 可能指向用户多个工具共用的 HF 缓存(比如 HF_HOME),
        // 里面会有别的工具的文件(这个会话里实测发现过一个漫画 OCR 模型),不该被
        // 误认成 subgen 自己的东西。
        assert!(!is_known_model_file_name("pytorch_model.bin")); // manga-ocr 用的文件名
        assert!(!is_known_model_file_name("random_file.txt"));
    }

    #[test]
    fn classify_picked_file_plausible_when_kind_and_size_match() {
        let outcome = classify_picked_file([0x6c, 0x6d, 0x67, 0x67], 3_095_033_483, FileKind::Ggml, 3_095_033_483);
        assert_eq!(outcome, PickOutcome::Plausible);
    }

    #[test]
    fn classify_picked_file_wrong_kind_when_magic_mismatches() {
        // 给语音识别模型的行选了个翻译模型(GGUF)文件。
        let outcome = classify_picked_file(*b"GGUF", 3_095_033_483, FileKind::Ggml, 3_095_033_483);
        assert_eq!(outcome, PickOutcome::WrongKind);
    }

    #[test]
    fn classify_picked_file_wrong_kind_when_magic_unrecognized() {
        let outcome = classify_picked_file([0, 0, 0, 0], 3_095_033_483, FileKind::Ggml, 3_095_033_483);
        assert_eq!(outcome, PickOutcome::WrongKind);
    }

    #[test]
    fn classify_picked_file_size_mismatch_when_kind_ok_but_size_off() {
        let outcome = classify_picked_file([0x6c, 0x6d, 0x67, 0x67], 2_000_000_000, FileKind::Ggml, 3_095_033_483);
        assert_eq!(outcome, PickOutcome::SizeMismatch { expected: 3_095_033_483, actual: 2_000_000_000 });
    }

    #[test]
    fn model_status_is_ready_only_matches_ready_variant() {
        let path = PathBuf::from("x");
        assert!(ModelStatus::Ready { path: path.clone(), size: 1 }.is_ready());
        assert!(!ModelStatus::Missing.is_ready());
        assert!(!ModelStatus::PathBroken { path: path.clone() }.is_ready());
        assert!(!ModelStatus::SizeMismatch { path, expected: 1, actual: 2 }.is_ready());
    }
}

