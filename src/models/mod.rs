//! 模型定义的统一入口。眼下只有 whisper 语音识别模型收拢进了这里(见 `whisper.rs`)——
//! 翻译模型的注册表(`TransModel`/`translate::MODELS`)本来就是干净的单一注册表,不需要
//! 挪动。这个模块是 Track B(模型管理重构)剩余步骤(绑定表/清单界面)的落脚点。

pub mod whisper;

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
}
