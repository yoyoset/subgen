//! 本地字幕翻译:调用 llama-server(llama.cpp 预编译二进制)子进程,通过其 OpenAI 兼容
//! HTTP API 把原文字幕翻译成中文,生成 `Video.zh.srt`(双语或仅中文,见 Config::bilingual)。
//!
//! 模型选择策略(关键事实,写代码时核实过,不是猜测):
//! - SakuraLLM 系列模型**只训练了日→中方向**,是专门用海量日中轻小说平行语料微调的,译文风格、
//!   人名/术语处理明显强于同规模通用模型;但喂英文等其他语言效果很差(不在其训练分布内)。
//! - 所以采用"语言 → 模型"映射表:日语走 Sakura,其余语言走通用指令模型(Qwen3-4B-Instruct)。
//!   `langs` 里用 `"*"` 表示兜底模型,匹配不到具体语言时用它。
//! - 与 engine.rs 里 whisper-cli 的子进程调用方式(便携二进制、子进程、CREATE_NO_WINDOW)保持
//!   一致,不引入 FFI/CUDA 编译依赖。

mod prompt;
mod segment;
mod server;

pub use server::LlamaServer;

use crate::engine;
use crate::paths;
use prompt::{build_output_blocks, compute_context_tail, translate_batch_with_retry};
use segment::{group_fragments, merge_group, split_by_weights};
pub(crate) use prompt::{looks_untranslated, split_leading_number};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 每批喂给模型的字幕行数。数值来自实测权衡:太大容易在长输出中丢行/错位编号,
/// 太小则 HTTP 往返开销主导总耗时、拖慢整体处理速度。
const BATCH_SIZE: usize = 15;

/// 把一批合并句的译文按原始字幕块展开。返回值的第一个元素是
/// `(原始块下标, 译文)`，第二个元素是失败组对应的所有原始块下标，第三个元素是本批覆盖的
/// 原始块数。这个映射不依赖模型或文件 IO，避免 `translate_srt` 里的分组/切回逻辑只能靠
/// 真实 llama-server 才能检查。
fn expand_group_results(
    source_texts: &[String],
    groups: &[Vec<usize>],
    group_offset: usize,
    results: &[String],
    failed_groups: &[usize],
) -> (Vec<(usize, String)>, Vec<usize>, usize) {
    let mut expanded = Vec::new();
    let mut failed_indices = Vec::new();
    let mut block_count = 0usize;

    for (local_group, result) in results.iter().enumerate() {
        let group = &groups[group_offset + local_group];
        let weights: Vec<usize> = group
            .iter()
            .map(|&i| source_texts[i].split_whitespace().count().max(1))
            .collect();
        let parts = split_by_weights(result, &weights);
        expanded.extend(group.iter().copied().zip(parts));
        block_count += group.len();
    }
    for &failed_group in failed_groups {
        failed_indices.extend(groups[group_offset + failed_group].iter().copied());
    }

    (expanded, failed_indices, block_count)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// SakuraLLM 官方提示词风格(轻小说翻译)
    Sakura,
    /// 通用指令模型的字幕翻译提示词
    General,
}

pub struct TransModel {
    /// 供 Config::translate_model / 界面下拉框使用的稳定标识符
    pub key: &'static str,
    pub label: &'static str,
    pub file_name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    /// 文件字节数,来自 HuggingFace LFS 指针文件核实过的真实值(不是估算),用于下载前的
    /// 磁盘空间提示(见 setup.rs 的 estimated_size_bytes)。
    pub size_bytes: u64,
    /// 该模型适用的源语言(ISO 639-1),`"*"` 表示兜底/通用
    pub langs: &'static [&'static str],
    pub prompt_kind: PromptKind,
}

pub const SAKURA_7B: TransModel = TransModel {
    key: "sakura-7b",
    label: "Sakura 7B(日→中,轻小说风格)",
    file_name: "sakura-7b-qwen2.5-v1.0-iq4xs.gguf",
    url: "https://huggingface.co/SakuraLLM/Sakura-7B-Qwen2.5-v1.0-GGUF/resolve/main/sakura-7b-qwen2.5-v1.0-iq4xs.gguf",
    sha256: "30e82ca13bc1e321f4f7b77459d533d483d46200522dc5c31eef9d2f2a6335d6",
    size_bytes: 4_250_298_208,
    langs: &["ja"],
    prompt_kind: PromptKind::Sakura,
};

pub const QWEN3_4B: TransModel = TransModel {
    key: "qwen3-4b",
    label: "Qwen3 4B(通用,多语言→中)",
    file_name: "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
    url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
    sha256: "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597",
    size_bytes: 2_497_281_120,
    langs: &["*"],
    prompt_kind: PromptKind::General,
};

pub const MODELS: &[TransModel] = &[SAKURA_7B, QWEN3_4B];

pub fn model_by_key(key: &str) -> Option<&'static TransModel> {
    MODELS.iter().find(|m| m.key == key)
}

/// 挑选翻译模型:`selected` 不是 "auto" 时尊重用户手动指定;否则按 `source_lang`
/// 精确匹配某个模型的 `langs`,匹配不到就落到 `langs` 含 `"*"` 的兜底模型。
/// 兜底/通用翻译模型(`langs` 含 `"*"` 的那个,目前是 Qwen3-4B)。除了 `pick_model` 自己用,
/// `setup::missing_components` 也用它——"auto" 策略下不该强制要求所有翻译模型都在(见那边
/// 的说明),只有这一个是必须的。
pub fn fallback_model() -> &'static TransModel {
    MODELS
        .iter()
        .find(|m| m.langs.contains(&"*"))
        .expect("MODELS 中必须有一个兜底(langs 含 \"*\")模型")
}

pub fn pick_model(source_lang: &str, selected: &str) -> &'static TransModel {
    if selected != "auto"
        && let Some(m) = model_by_key(selected) {
            return m;
        }
    let lang = source_lang.to_ascii_lowercase();
    MODELS
        .iter()
        .find(|m| m.langs.contains(&lang.as_str()))
        .unwrap_or_else(fallback_model)
}

pub enum TranslateResult {
    /// `failed_indices` 是翻译失败、原样保留原文的字幕块下标(0-based,对应 `parse_srt_blocks`
    /// 返回的顺序),不只是一个数量——界面据此能告诉用户具体是第几条,而不是让用户对着几百
    /// 条字幕从头找起。
    Written { failed_indices: Vec<usize> },
    Skipped(String),
}

/// 读取 `original_srt`,逐批翻译,按 `bilingual` 写出 `Video.zh.srt`(中文在上、原文在下,
/// 或仅中文)。`overwrite=false` 且目标已存在时返回 Skipped,不覆盖(与 engine::place_sidecar_srt
/// 的跳过语义保持一致)。
#[allow(clippy::too_many_arguments)]
pub fn translate_srt(
    server: &LlamaServer,
    model: &'static TransModel,
    original_srt: &Path,
    video_path: &Path,
    bilingual: bool,
    overwrite: bool,
    cancel: &Arc<AtomicBool>,
    max_subtitle_width: usize,
    mut progress_cb: impl FnMut(String, Option<f32>),
) -> Result<TranslateResult, String> {
    let target = paths::sidecar_srt_lang_path(video_path, "zh");
    if target.exists() && !overwrite {
        // 只显示文件名,不显示完整路径,和 engine.rs::place_sidecar_srt 的跳过消息保持
        // 同样的简洁度——裸露一整条绝对路径又长又没必要。
        let filename = target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        return Ok(TranslateResult::Skipped(format!(
            "已有同名中文字幕,未覆盖({filename})"
        )));
    }

    let content =
        std::fs::read_to_string(original_srt).map_err(|e| format!("读取原文字幕失败: {e}"))?;
    let blocks = engine::parse_srt_blocks(&content);
    if blocks.is_empty() {
        // 没有对白(空字幕)是合理结果,直接写一份同样为空的中文字幕,不算失败
        std::fs::write(&target, "").map_err(|e| format!("写入字幕文件失败: {e}"))?;
        return Ok(TranslateResult::Written {
            failed_indices: Vec::new(),
        });
    }

    let total = blocks.len();
    let mut translated: Vec<String> = vec![String::new(); total];
    let mut failed_indices: Vec<usize> = Vec::new();
    let mut done_blocks = 0usize;
    // 上一批最后几行(原文, 译文)——现在每批是独立翻译的,人称/称呼/术语在批边界处经常
    // 漂移(比如上一批刚确定用"她",这一批又变回泛指)。把上一批的结尾喂给下一批当参考
    // 上下文(不要求模型翻译或复述它),是这类问题里最便宜也最有效的缓解办法,不需要改动
    // 批量大小或对齐解析逻辑。
    let mut prev_tail: Vec<(String, String)> = Vec::new();

    let source_texts: Vec<String> = blocks.iter().map(|(_, _, text)| text.clone()).collect();
    let groups = group_fragments(&source_texts);
    let grouped_texts: Vec<String> = groups
        .iter()
        .map(|indices| {
            let fragments: Vec<&str> = indices.iter().map(|&i| source_texts[i].as_str()).collect();
            merge_group(&fragments)
        })
        .collect();
    let mut done_groups = 0usize;

    for chunk in grouped_texts.chunks(BATCH_SIZE) {
        if cancel.load(Ordering::Relaxed) {
            // 取消:批边界之外,正在请求中的单次 HTTP 请求也会被 send_cancelable 每 100ms
            // 检查一次取消标志并立即中断(见 prompt.rs),所以这里能很快走到。
            return Err("已取消".to_string());
        }
        let (results, failed) =
            translate_batch_with_retry(server, model, chunk, cancel, &prev_tail)?;

        prev_tail = compute_context_tail(chunk, &results, &failed);

        let (expanded, failed_blocks, block_count) =
            expand_group_results(&source_texts, &groups, done_groups, &results, &failed);
        for (block_index, text) in expanded {
            translated[block_index] = text;
        }
        failed_indices.extend(failed_blocks);
        done_groups += chunk.len();
        done_blocks += block_count;
        progress_cb(
            "正在翻译字幕...".to_string(),
            Some((done_blocks as f32 / total as f32) * 100.0),
        );
    }

    let out_blocks = build_output_blocks(&blocks, &translated, &failed_indices, bilingual);
    // 排版层折行:双语块文本是 `中文\n原文` 两行一块,wrap_subtitle_text 对每行独立折、
    // 不会把两种语言混在一起重排。宽度上限 0 表示关闭,原样返回。
    let wrapped: Vec<(String, String, String)> = out_blocks
        .into_iter()
        .map(|(start, end, text)| {
            let wrapped_text = engine::wrap_subtitle_text(&text, max_subtitle_width);
            (start, end, wrapped_text)
        })
        .collect();

    std::fs::write(&target, engine::write_srt_blocks(&wrapped))
        .map_err(|e| format!("写入字幕文件失败: {e}"))?;

    Ok(TranslateResult::Written { failed_indices })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_group_results_restores_original_block_indices() {
        let source = vec![
            "what do your eyes".to_string(),
            "lock in as a target?".to_string(),
            "Next sentence.".to_string(),
        ];
        let groups = group_fragments(&source);
        let results = vec!["你的目光会锁定在什么目标上吗？".to_string(), "下一句。".to_string()];

        let (expanded, failed, block_count) =
            expand_group_results(&source, &groups, 0, &results, &[1]);

        assert_eq!(expanded.len(), 3);
        assert_eq!(expanded.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(failed, vec![2]);
        assert_eq!(block_count, source.len());
        assert_eq!(expanded[0].1.clone() + &expanded[1].1, "你的目光会锁定在什么目标上吗？");
    }

    #[test]
    fn fallback_model_is_qwen3_4b() {
        // setup::missing_components 依赖这个返回值来决定"auto"策略下只要求哪一个翻译模型
        // 齐全,固定断言具体是哪个,避免以后改注册表顺序时这个假设悄悄漂移。
        assert_eq!(fallback_model().key, "qwen3-4b");
    }

    #[test]
    fn pick_model_matches_japanese_to_sakura() {
        let m = pick_model("ja", "auto");
        assert_eq!(m.key, "sakura-7b");
    }


    #[test]
    fn pick_model_falls_back_to_catchall_for_other_languages() {
        let m = pick_model("en", "auto");
        assert_eq!(m.key, "qwen3-4b");
    }


    #[test]
    fn pick_model_respects_manual_selection() {
        let m = pick_model("ja", "qwen3-4b");
        assert_eq!(m.key, "qwen3-4b");
    }
}
