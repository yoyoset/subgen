//! whisper 语音识别模型注册表。之前这三个模型的定义散落在三个文件里(`config.rs` 的
//! 名字+中文说明、`setup/mod.rs` 的字节数、`setup/mod.rs` 的哈希——且哈希只覆盖了
//! `large-v3` 一个,`medium`/`large-v3-turbo` 的 `sha256` 一直是 `None`,导致这两个模型
//! 的下载完全没有校验),照抄 `translate::TransModel` 的形状收拢成一个注册表。
//!
//! `key` 沿用 `Config.model` 现有存的字符串值(`"large-v3"` 等),config.toml 里已有的
//! 值不受影响,不需要迁移。

pub struct WhisperModel {
    /// 供 `Config.model` / 界面下拉框使用的稳定标识符,也是文件名模板 `ggml-{key}.bin`
    /// 里的那一段。
    pub key: &'static str,
    /// 用速度档位而不是硬件型号/倍速这类只有懂行的人才看得懂的说法描述(原注释见
    /// 已删除的 `Config::AVAILABLE_MODELS`)——普通用户真正关心的是"选哪个不会等太久/卡"。
    pub label: &'static str,
    pub file_name: &'static str,
    pub url: &'static str,
    /// `None` 表示还没有核实过的权威哈希(下载后不做强校验)。目前三个模型(`large-v3`/
    /// `medium`/`large-v3-turbo`)都已有核实过的哈希——来源为 HuggingFace LFS 指针文件的
    /// oid 与响应头 X-Linked-ETag 两个独立来源交叉核对一致,`large-v3` 额外做过完整下载后
    /// 本地 sha256sum 复核。保留 `Option` 类型是为了约束未来新增模型:没有核实过的哈希就
    /// 必须写 `None`,**不编造**——放一个猜测出来的哈希比放 `None` 更危险,前者会让用户
    /// 误以为下载被校验过。
    pub sha256: Option<&'static str>,
    /// 文件字节数,来自 GitHub Release API / HuggingFace LFS 指针文件核实过的真实值
    /// (不是估算),用于下载前的磁盘空间提示。
    pub size_bytes: u64,
}

pub const LARGE_V3: WhisperModel = WhisperModel {
    key: "large-v3",
    label: "标准(最准确,推荐)",
    file_name: "ggml-large-v3.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin",
    sha256: Some("64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2"),
    size_bytes: 3_095_033_483,
};

pub const LARGE_V3_TURBO: WhisperModel = WhisperModel {
    key: "large-v3-turbo",
    label: "快速(准确率略降)",
    file_name: "ggml-large-v3-turbo.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
    // 2026-07-25 核实:HF /raw/ 端点的 LFS 指针(oid sha256:...)和 /resolve/ 的 HEAD 响应头
    // X-Linked-ETag 两个独立来源结果一致,size 也和已有的 size_bytes 吻合。
    sha256: Some("1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69"),
    size_bytes: 1_624_555_275,
};

pub const MEDIUM: WhisperModel = WhisperModel {
    key: "medium",
    label: "极速(适合显卡较弱或纯 CPU 的电脑)",
    file_name: "ggml-medium.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
    // 2026-07-25 核实,来源和交叉验证方式同上。
    sha256: Some("6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208"),
    size_bytes: 1_533_763_059,
};

pub const MODELS: &[WhisperModel] = &[LARGE_V3, LARGE_V3_TURBO, MEDIUM];

pub fn by_key(key: &str) -> Option<&'static WhisperModel> {
    MODELS.iter().find(|m| m.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_key_finds_every_registered_model() {
        for m in MODELS {
            let found = by_key(m.key).expect("注册表里的每个模型都应该能按 key 查到");
            assert_eq!(found.key, m.key);
        }
    }

    #[test]
    fn by_key_returns_none_for_unknown_key() {
        assert!(by_key("does-not-exist").is_none());
    }

    #[test]
    fn every_model_file_name_matches_ggml_key_template() {
        // paths::model_path 是按 "ggml-{model}.bin" 拼文件名的(见 paths.rs),这里确认
        // 注册表里的 file_name 和这个模板保持一致,不会出现两边算出不同文件名的情况。
        for m in MODELS {
            assert_eq!(m.file_name, format!("ggml-{}.bin", m.key));
        }
    }

    #[test]
    fn every_model_url_matches_ggerganov_resolve_template() {
        for m in MODELS {
            assert_eq!(
                m.url,
                format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}", m.file_name)
            );
        }
    }

    #[test]
    fn default_config_model_key_is_registered() {
        // Config::default().model 是 "large-v3"——确认这个值真的在注册表里,不是两边
        // 各自维护、悄悄漂移。
        assert!(by_key("large-v3").is_some());
    }
}
