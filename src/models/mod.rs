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
    fn model_status_is_ready_only_matches_ready_variant() {
        let path = PathBuf::from("x");
        assert!(ModelStatus::Ready { path: path.clone(), size: 1 }.is_ready());
        assert!(!ModelStatus::Missing.is_ready());
        assert!(!ModelStatus::PathBroken { path: path.clone() }.is_ready());
        assert!(!ModelStatus::SizeMismatch { path, expected: 1, actual: 2 }.is_ready());
    }
}
