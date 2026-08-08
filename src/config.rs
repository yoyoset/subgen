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
    /// 上次添加视频时所在的目录,下次打开"选择视频文件"对话框默认跳到那里——批量处理
    /// 一整个季的用户不用每次都从默认目录翻半天。None = 还没加过,对话框用系统默认位置。
    pub last_dir: Option<String>,
    pub window_w: f32,
    pub window_h: f32,
    /// 模型绑定表:显式指定"某个模型(key 见 models::whisper_binding_key 等)对应磁盘上
    /// 哪个具体文件",绝对路径。空表(默认)= 完全退回 models_dir() 里递归搜索的旧行为,
    /// 不影响任何现有用户。用户通过"定位…"/"使用此文件"手动选中一个文件、且哈希校验
    /// 通过后,UI 会写入一条绑定(见 `ui::model_manifest`/`app::apply_model_bound`)。
    /// 声明在结构体最后一个字段,纯粹是为了让 TOML 输出把这张表(可能较长)放在文件末尾,
    /// 不打断前面那些标量字段的可读性,不影响解析。
    pub model_files: std::collections::BTreeMap<String, String>,
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
            last_dir: None,
            window_w: 760.0,
            window_h: 560.0,
            model_files: std::collections::BTreeMap::new(),
        }
    }
}

impl Config {
    /// 解析配置(可能损坏)并返回人类可读的恢复说明,`None` 表示解析成功没有异常。
    /// 纯函数,不碰文件系统,方便单测覆盖"合法 TOML/未知字段容忍/彻底损坏"这几种情况。
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
    ///
    /// 返回的 `Option<String>` 是给界面的恢复说明(比如"配置文件损坏,已备份并恢复默认"),
    /// `None` 表示配置完好——之前这个信息只写进 logs/app.log,用户完全看不到自己的模型
    /// 目录设置被重置了,属于"后台失败但界面看起来成功"。现在由 main.rs 把它带给界面,
    /// 用一条可关闭的横幅告知用户。
    pub fn load() -> (Self, Option<String>) {
        let path = crate::paths::config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            // 文件不存在是首次运行的正常情况,不是"损坏",不需要备份。
            Err(_) => return (Self::default(), None),
        };
        let (config, warning) = Self::parse_or_recover(&text);
        if let Some(reason) = warning {
            let backup_path = path.with_extension("toml.bak");
            // 备份是否成功要如实告知:备份失败却告诉用户"已备份",下次 save() 就会把坏文件
            // 无声覆盖掉,唯一的诊断线索就没了(这就是"界面声称成功、实际失败")。
            let msg = if std::fs::copy(&path, &backup_path).is_ok() {
                format!(
                    "配置文件已损坏,已备份到 {} 并使用默认设置(模型位置等设置会恢复默认): {reason}",
                    backup_path.display()
                )
            } else {
                format!(
                    "配置文件已损坏,且备份失败(原文件保留,但下次保存会被覆盖;建议手动复制备份): {reason}"
                )
            };
            crate::format::push_log(&mut Vec::new(), msg.clone());
            (config, Some(msg))
        } else {
            (config, None)
        }
    }

    /// 原子写入配置:先写同目录的 `.tmp`,再改名覆盖正式文件——避免写入中途断电/崩溃
    /// 留下半截的 config.toml。虽然 `load()` 对损坏文件有 `.bak` 恢复,但能不让它坏就
    /// 不坏。改名是同一目录内的原子操作(Windows 的 MoveFileEx 带 REPLACE_EXISTING)。
    pub fn save(&self) {
        match toml::to_string_pretty(self) {
            Ok(text) => {
                let path = crate::paths::config_path();
                let tmp = path.with_extension("toml.tmp");
                if let Err(e) = std::fs::write(&tmp, &text) {
                    crate::format::push_log(&mut Vec::new(), format!("保存配置失败: {e}"));
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, &path) {
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
    fn old_config_without_model_files_field_still_parses() {
        // 老用户的 config.toml 里没有 model_files 这个键(这一版才加的),不该导致解析失败
        // ——serde 的 #[serde(default)] 应该让缺失字段落到 Default::default()(空表)。
        let text = r#"
            model = "large-v3"
            language = "auto"
            overwrite_existing = false
            translate = false
            translate_model = "auto"
            bilingual = true
            window_w = 760.0
            window_h = 560.0
        "#;
        let (config, warning) = Config::parse_or_recover(text);
        assert!(warning.is_none(), "{warning:?}");
        assert!(config.model_files.is_empty());
    }

    #[test]
    fn model_files_binding_table_round_trips_through_toml() {
        let mut original = Config::default();
        original
            .model_files
            .insert("trans:sakura-7b".to_string(), r"F:\hf_cache\sakura-7b.gguf".to_string());
        let text = toml::to_string_pretty(&original).unwrap();
        let (parsed, warning) = Config::parse_or_recover(&text);
        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(
            parsed.model_files.get("trans:sakura-7b"),
            Some(&r"F:\hf_cache\sakura-7b.gguf".to_string())
        );
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

    /// 合并成一个测试:save()/load() 读写的是进程级固定的 config_path()(测试 exe 所在
    /// 目录下的 config.toml),拆开写会互相踩文件导致偶发失败。这里把"正常保存→原子写入
    /// 不残留临时文件→load 还原"和"损坏配置→load 返回恢复说明→坏文件被备份"两条路径
    /// 放进同一个测试,先清理、后清理,不会和其它测试冲突。
    #[test]
    fn save_is_atomic_and_load_reports_corruption_recovery() {
        let path = crate::paths::config_path();
        let tmp = path.with_extension("toml.tmp");
        let bak = path.with_extension("toml.bak");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&bak);

        // 1) 正常保存:原子写入(先 .tmp 再改名覆盖),临时文件不残留,load 原样还原。
        let config = Config {
            model: "medium".to_string(),
            models_dir_override: Some(r"Z:\somewhere".to_string()),
            ..Config::default()
        };
        config.save();
        assert!(!tmp.exists(), "原子写入后 .tmp 临时文件不该残留");
        let (loaded, warning) = Config::load();
        assert!(warning.is_none(), "正常保存的配置不该有恢复说明: {warning:?}");
        assert_eq!(loaded.model, "medium");
        assert_eq!(loaded.models_dir_override, Some(r"Z:\somewhere".to_string()));

        // 2) 配置损坏(比如写入中途断电留下的半截文件):load 返回恢复说明、退回默认值,
        //    坏文件备份成 .bak——用户能通过界面知道"模型目录设置被重置了"而不是静默发生。
        std::fs::write(&path, "this is not valid toml {{{").unwrap();
        let (recovered, warning) = Config::load();
        assert!(warning.is_some(), "损坏的配置应该返回恢复说明");
        assert_eq!(recovered.model, Config::default().model, "损坏时退回默认值");
        assert!(bak.exists(), "损坏的配置应该被备份成 .bak");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&bak);
    }
}
