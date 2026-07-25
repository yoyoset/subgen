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
    /// 尝试把 `text` 解析成 Config,失败时返回默认配置 + 一句人话说明原因(而不是像旧版
    /// `load()` 那样直接 `unwrap_or_default()` 静默吞掉)。纯函数,不碰文件系统,方便单测
    /// 覆盖"合法 TOML/未知字段容忍/彻底损坏"这几种情况。
    pub fn parse_or_recover(text: &str) -> (Config, Option<String>) {
        match toml::from_str::<Config>(text) {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(format!("配置文件解析失败: {e}"))),
        }
    }

    /// 真实踩过的坑(推演,未在生产环境复现,但代价一旦发生就是灾难性的):旧版
    /// `unwrap_or_default()` 会在任何解析错误时悄悄把整份配置退回默认值,包括
    /// `models_dir_override`——对把模型放在几十 GB 共享缓存目录里的用户来说,意味着
    /// 下次打开程序会被要求重新下载十几个 GB,而不是提示"配置文件坏了"。这里解析失败时
    /// 先把坏文件另存一份 `.bak`,再返回默认配置——保留诊断线索,而不是让它在下一次
    /// `save()`(几乎任何设置变更都会触发)时被默认配置无声覆盖、永久丢失。
    pub fn load() -> Self {
        let path = crate::paths::config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            // 文件不存在是首次运行的正常情况,不是"损坏",不需要备份。
            Err(_) => return Self::default(),
        };
        let (config, warning) = Self::parse_or_recover(&text);
        if let Some(reason) = warning {
            let backup_path = path.with_extension("toml.bak");
            let _ = std::fs::copy(&path, &backup_path);
            crate::format::push_log(
                &mut Vec::new(),
                format!("{reason},已备份为 {} 并使用默认设置", backup_path.display()),
            );
        }
        config
    }

    pub fn save(&self) {
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(crate::paths::config_path(), text) {
                    crate::format::push_log(&mut Vec::new(), format!("保存配置失败: {e}"));
                }
            }
            Err(e) => {
                crate::format::push_log(&mut Vec::new(), format!("序列化配置失败: {e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_or_recover_accepts_valid_toml_without_warning() {
        let text = r#"
            model = "medium"
            language = "en"
            overwrite_existing = true
            translate = true
            translate_model = "qwen3-4b"
            bilingual = false
            window_w = 800.0
            window_h = 600.0
        "#;
        let (config, warning) = Config::parse_or_recover(text);
        assert!(warning.is_none());
        assert_eq!(config.model, "medium");
        assert_eq!(config.translate_model, "qwen3-4b");
        assert!(!config.bilingual);
    }

    #[test]
    fn parse_or_recover_tolerates_unknown_fields_for_forward_compatibility() {
        // 以后加新字段时,旧版本(理论上)或者用户手改配置文件混入了不认识的键,不该导致
        // 整份配置报废——serde 默认就会忽略未知字段,这里显式确认这个假设成立。
        let text = r#"
            model = "large-v3"
            language = "auto"
            overwrite_existing = false
            translate = false
            translate_model = "auto"
            bilingual = true
            window_w = 760.0
            window_h = 560.0
            some_future_field_not_yet_known = "whatever"
        "#;
        let (config, warning) = Config::parse_or_recover(text);
        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(config.model, "large-v3");
    }

    #[test]
    fn parse_or_recover_falls_back_to_default_on_garbage_with_warning() {
        let (config, warning) = Config::parse_or_recover("this is not valid toml at all {{{");
        assert!(warning.is_some());
        assert_eq!(config.model, Config::default().model);
        assert_eq!(config.models_dir_override, None);
    }

    #[test]
    fn parse_or_recover_falls_back_when_a_field_has_the_wrong_type() {
        // 类型错误(比如 window_w 被手改成字符串)也算解析失败,不能只挡纯语法错误。
        let text = r#"
            model = "large-v3"
            window_w = "not a number"
        "#;
        let (_, warning) = Config::parse_or_recover(text);
        assert!(warning.is_some());
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        // 序列化后立刻反序列化应该逐字段还原——这是 save()/load() 配对正确工作的前提。
        let original = Config::default();
        let text = toml::to_string_pretty(&original).unwrap();
        let (parsed, warning) = Config::parse_or_recover(&text);
        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(parsed.model, original.model);
        assert_eq!(parsed.models_dir_override, original.models_dir_override);
        assert_eq!(parsed.window_w, original.window_w);
    }
}
