//! 便携配置:存在程序目录下的 config.toml,不写系统目录/注册表。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// whisper 模型名,如 "large-v3" / "large-v3-turbo" / "medium"
    pub model: String,
    /// 识别语言,"auto" 为自动检测,或指定如 "en"
    pub language: String,
    /// 已存在同名 .srt 时是否覆盖(false = 跳过)
    pub overwrite_existing: bool,
    /// 是否在转写后额外用本地模型生成中文翻译字幕(Video.zh.srt)
    pub translate: bool,
    /// 翻译模型选择:"auto"(按源语言自动选,见 translate::pick_model)或具体模型 key
    /// (如 "sakura-7b" / "qwen3-4b",见 translate::MODELS)
    pub translate_model: String,
    /// 中文翻译字幕是否双语(中文在上、原文在下);false = 仅中文
    pub bilingual: bool,
    /// 模型存放目录的用户覆盖值(绝对路径)。None = 用默认值(exe 同目录下的 models/),
    /// 保持便携行为;Some(path) = 用户在界面里显式指定过,不再跟着 exe 走(模型文件动辄
    /// 几个 GB 到十几个 GB,不应该被迫跟着 exe 放的位置走,比如很多人图方便把 exe 放 C 盘)。
    pub models_dir_override: Option<String>,
    pub window_w: f32,
    pub window_h: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "large-v3".to_string(),
            language: "auto".to_string(),
            overwrite_existing: false,
            translate: false,
            translate_model: "auto".to_string(),
            bilingual: true,
            models_dir_override: None,
            window_w: 760.0,
            window_h: 560.0,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let path = crate::paths::config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(crate::paths::config_path(), text);
        }
    }

    /// 可选模型列表(供界面下拉框使用),数值为估计的相对速度/质量取舍说明。
    pub const AVAILABLE_MODELS: &'static [(&'static str, &'static str)] = &[
        ("large-v3", "最准确(推荐,3060 上约 8 倍实时速度)"),
        ("large-v3-turbo", "更快,准确率略降"),
        ("medium", "更快更省显存,准确率进一步降低"),
    ];
}
