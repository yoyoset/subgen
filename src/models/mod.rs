//! 模型定义的统一入口。眼下只有 whisper 语音识别模型收拢进了这里(见 `whisper.rs`)——
//! 翻译模型的注册表(`TransModel`/`translate::MODELS`)本来就是干净的单一注册表,不需要
//! 挪动。这个模块是 Track B(模型管理重构)剩余步骤(`ModelId`/绑定表/清单界面)的落脚点。

pub mod whisper;
