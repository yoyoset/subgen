use super::server::LlamaServer;
use super::{PromptKind, TransModel};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(super) fn compute_context_tail(
    texts: &[String],
    results: &[String],
    failed_indices: &[usize],
) -> Vec<(String, String)> {
    let failed_set: std::collections::HashSet<usize> = failed_indices.iter().copied().collect();
    let successful_pairs: Vec<(String, String)> = texts
        .iter()
        .cloned()
        .zip(results.iter().cloned())
        .enumerate()
        .filter(|(i, _)| !failed_set.contains(i))
        .map(|(_, pair)| pair)
        .collect();
    let tail_start = successful_pairs.len().saturating_sub(2);
    successful_pairs[tail_start..].to_vec()
}

/// 把原文块和翻译结果拼成最终要写出的字幕块。抽成独立函数是因为这段逻辑不该依赖真实的
/// llama-server 网络调用就能被测到——翻译失败行的 `translated` 本身就是原文的拷贝(见
/// `translate_batch_with_retry` 的单行兜底),双语模式下不能再按正常行的逻辑拼一遍原文,
/// 否则同一句话会重复输出两遍(`{orig}\n{orig}`)——失败行不管双语与否都只输出一遍原文。
pub(super) fn build_output_blocks(
    blocks: &[(String, String, String)],
    translated: &[String],
    failed_indices: &[usize],
    bilingual: bool,
) -> Vec<(String, String, String)> {
    let failed_set: std::collections::HashSet<usize> = failed_indices.iter().copied().collect();
    blocks
        .iter()
        .zip(translated.iter())
        .enumerate()
        .map(|(i, ((start, end, orig), zh))| {
            let text = if failed_set.contains(&i) {
                orig.clone()
            } else if bilingual {
                format!("{zh}\n{orig}")
            } else {
                zh.clone()
            };
            (start.clone(), end.clone(), text)
        })
        .collect()
}

/// 翻译一批字幕行,失败/行数错位时递归对半重试,缩小出错范围;单行仍失败则原样保留该行
/// (计入失败下标,不让整个任务失败——用户仍然拿到完整的字幕,只是个别行没翻译)。
///
/// 返回值第二项是失败行在 `texts` 里的下标(0-based),不是单纯的计数——上层
/// `translate_srt` 需要知道具体是哪几行,才能在双语模式下避免对失败行重复拼接原文,
/// 也能在界面警告里告诉用户具体第几条需要人工核对。
/// `context` 是给模型看的"上一批结尾"参考(见 translate_srt),对半重试时原样传给两个
/// 子调用——重试是格式错位这种偶发情况的兜底路径,没必要为它精细计算"这一半真正紧邻的
/// 上文",用同一个批级上下文对两半都总比完全没有上下文好。
pub(super) fn translate_batch_with_retry(
    server: &LlamaServer,
    model: &'static TransModel,
    texts: &[String],
    cancel: &Arc<AtomicBool>,
    context: &[(String, String)],
) -> Result<(Vec<String>, Vec<usize>), String> {
    if texts.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".to_string());
    }
    match translate_batch_once(server, model, texts, context) {
        Ok(results) if results.len() == texts.len() => {
            retry_untranslated_lines(server, model, texts, results, cancel, context)
        }
        _ if texts.len() == 1 => Ok((vec![texts[0].clone()], vec![0])),
        _ => {
            let mid = texts.len() / 2;
            let (mut out, mut failed) =
                translate_batch_with_retry(server, model, &texts[..mid], cancel, context)?;
            let (b, fb) =
                translate_batch_with_retry(server, model, &texts[mid..], cancel, context)?;
            out.extend(b);
            // fb 是后半段(texts[mid..])内部的下标,加上 mid 才是相对整个 texts 的下标。
            failed.extend(fb.iter().map(|&i| i + mid));
            Ok((out, failed))
        }
    }
}

/// 真实踩过的坑:字幕原文经常是"一句话被切成好几条时间轴"的碎片(比如"...that just
/// gets you feeling good about, you know,"),批量翻译时模型有时会把这类碎片当成不完整
/// 句子,要么原样抄回英文不翻译,要么把好几条的意思压缩合并进一条、导致后面几条只剩
/// 残渣——这两种情况 `parse_numbered_response` 都测不出来(返回的行数是对的,只是内容
/// 不对),之前完全没有机制发现。
///
/// 这里在"格式校验通过"之后再加一层"内容看起来像不像真的翻译过"的检查(`looks_untranslated`,
/// 只抓"整行原样抄回来"这种最确定的信号,不去猜测更微妙的合并错位,避免误伤正常翻译)。
/// 命中的行单独、脱离其它 14 行干扰地再问模型一次——很多时候模型是被同批里的其它碎片
/// 分心了,单独喂反而能给出正确翻译。这次重试仍然不对,才真正计入失败(交给上层
/// `build_output_blocks`/`format_failed_lines_warning` 走既有的"保留原文 + 提醒用户
/// 核对"流程,不静默接受一行没翻译的内容)。
fn retry_untranslated_lines(
    server: &LlamaServer,
    model: &'static TransModel,
    texts: &[String],
    mut results: Vec<String>,
    cancel: &Arc<AtomicBool>,
    context: &[(String, String)],
) -> Result<(Vec<String>, Vec<usize>), String> {
    let suspects: Vec<usize> = texts
        .iter()
        .zip(results.iter())
        .enumerate()
        .filter(|(_, (t, r))| looks_untranslated(t, r))
        .map(|(i, _)| i)
        .collect();
    if suspects.is_empty() {
        return Ok((results, Vec::new()));
    }

    let mut still_failed = Vec::new();
    for i in suspects {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".to_string());
        }
        let single = [texts[i].clone()];
        match translate_batch_once(server, model, &single, context) {
            Ok(retry) if retry.len() == 1 && !looks_untranslated(&texts[i], &retry[0]) => {
                results[i] = retry.into_iter().next().unwrap();
            }
            _ => still_failed.push(i),
        }
    }
    Ok((results, still_failed))
}

/// 判断一行"译文"是不是看起来根本没有被翻译、只是把原文抄了回来(大小写不敏感、首尾
/// 空白不敏感)。只在原文本身含有实际文字内容(字母/汉字/假名等)时才判断——纯符号/
/// 数字/音乐记号(比如"♪♪♪")本来就没有可翻译的文字,原样返回是对的,不该被当成
/// 翻译失败。
pub(crate) fn looks_untranslated(original: &str, translated: &str) -> bool {
    original.chars().any(|c| c.is_alphabetic())
        && original.trim().eq_ignore_ascii_case(translated.trim())
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Serialize)]
struct ChatRequest {
    model: &'static str,
    messages: Vec<ChatMessage>,
    temperature: f32,
    max_tokens: u32,
    /// llama-server 支持的 KV 缓存复用开关。system prompt 在同一批次内的每次请求里都一样
    /// (见 build_prompt),开启后 llama-server 能复用之前请求已经算过的那部分 KV 缓存,
    /// 不用每次都从头重新计算,是白捡的速度,不影响翻译结果。
    cache_prompt: bool,
}

fn translate_batch_once(
    server: &LlamaServer,
    model: &'static TransModel,
    texts: &[String],
    context: &[(String, String)],
) -> Result<Vec<String>, String> {
    let numbered: String = texts
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}. {}", i + 1, t.replace('\n', " ")))
        .collect::<Vec<_>>()
        .join("\n");

    let (system, user) = build_prompt(model, &numbered, context);
    let body = ChatRequest {
        model: model.key,
        messages: vec![
            ChatMessage {
                role: "system",
                content: system,
            },
            ChatMessage {
                role: "user",
                content: user,
            },
        ],
        temperature: 0.2,
        max_tokens: (texts.len() as u32 * 100 + 200).min(4096),
        cache_prompt: true,
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("创建翻译请求客户端失败: {e}"))?;
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/chat/completions", server.port))
        .json(&body)
        .send()
        .map_err(|e| format!("翻译请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("翻译请求失败: HTTP {}", resp.status()));
    }
    let json: serde_json::Value = resp
        .json()
        .map_err(|e| format!("解析翻译响应失败: {e}"))?;
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("翻译响应格式异常(缺少 choices[0].message.content 字段)")?;

    parse_numbered_response(content, texts.len())
}

/// 提示词按模型的 PromptKind 区分:
/// - Sakura 官方推荐提示词是单段文本(不带编号),这里为了能批量翻译多行字幕、又要保证
///   行与行对齐,在官方提示词基础上追加了"保留编号"的指令 —— 与官方单行格式略有出入,
///   是吞吐量(不然一集字幕要发几百次请求)和对齐可靠性之间的权衡,v1 先这样,
///   如果实测发现 Sakura 在编号格式下翻译质量明显下降,再改成小批量(如 3~5 行一批)。
/// - General(Qwen)本来就是通用指令模型,直接要求保留编号格式即可。
///
/// `context` 是上一批最后几行的(原文, 译文),不空时拼进 user 消息最前面,给模型一个
/// "紧接在前的上文"参考——现在每批是独立翻译的,人称代词、称呼、专有名词在批边界处
/// 经常漂移(比如上一批刚确定某角色用"她",下一批又变回中性称呼),喂一点上文能让模型
/// 在批与批之间保持一致,不需要为此改动批量大小或编号对齐解析逻辑(上下文只是参考信息,
/// 不算进正文编号,parse_numbered_response 完全不受影响)。明确要求模型不要把这段上下文
/// 也翻译/复述出来,避免它把参考内容当成本批正文的一部分。
fn build_prompt(model: &TransModel, numbered: &str, context: &[(String, String)]) -> (String, String) {
    let context_block = if context.is_empty() {
        String::new()
    } else {
        let lines: String = context
            .iter()
            .map(|(orig, zh)| format!("{orig} → {zh}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "(以下是紧接在前的上文原文及其译文,仅供参考、用来保持人称和称呼前后一致,\
             不要翻译它、也不要在你的回复里重复输出它:\n{lines}\n参考上文到此结束,\
             正式要翻译的正文从下一行开始:)\n"
        )
    };

    match model.prompt_kind {
        PromptKind::Sakura => (
            "你是一个轻小说翻译模型，可以流畅通顺地以日本轻小说的风格将日文翻译成简体中文，\
             并联系上下文正确使用人称代词，不擅自添加原文中没有的代词。\n\
             输入是带编号的多行日文字幕，请严格逐行翻译，保留原有的“编号.”格式，每行只输出一条\
             译文，不要合并、拆分、增删行，不要输出编号之外的任何解释。字幕原文经常是被截断的\
             半句话，即使某一行看起来意思不完整，也只翻译这一行本身给出的文字，不要把它的内容\
             并入上一行或下一行、也不要为了让译文通顺而擅自补全整句。"
                .to_string(),
            format!("{context_block}将下面的日文字幕文本翻译成中文：\n{numbered}"),
        ),
        PromptKind::General => (
            "你是专业的字幕翻译员。将用户给出的带编号字幕文本翻译成简体中文。要求:逐行对应翻译,\
             严格保留原有的“编号.”格式,每行只输出一条译文,不要合并或拆分行,不要添加编号之外的\
             任何解释、注释或原文。字幕原文经常是被截断的半句话,即使某一行看起来意思不完整,\
             也只翻译这一行本身给出的文字,不要把它的内容并入上一行或下一行、也不要为了让译文\
             通顺而擅自补全整句。"
                .to_string(),
            format!("{context_block}{numbered}"),
        ),
    }
}

/// 解析模型输出里的编号行(如 "3. 译文" / "3、译文" / "3) 译文"),返回 编号→译文 映射。
/// 行数或编号连续性对不上时返回 Err,交给上层 translate_batch_with_retry 拆小重试。
fn parse_numbered_response(content: &str, expected: usize) -> Result<Vec<String>, String> {
    let mut map: std::collections::BTreeMap<usize, String> = std::collections::BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((num, rest)) = split_leading_number(line) {
            map.insert(num, rest.to_string());
        }
    }
    if map.len() != expected {
        return Err(format!(
            "翻译输出行数不匹配(期望 {expected},实际 {})",
            map.len()
        ));
    }
    let mut out = Vec::with_capacity(expected);
    for i in 1..=expected {
        match map.get(&i) {
            Some(t) => out.push(t.clone()),
            None => return Err(format!("翻译输出缺少第 {i} 行")),
        }
    }
    Ok(out)
}

pub(crate) fn split_leading_number(line: &str) -> Option<(usize, &str)> {
    let digits_len = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_len == 0 {
        return None;
    }
    let (num_str, after_digits) = line.split_at(digits_len);
    let num: usize = num_str.parse().ok()?;
    let after_sep = after_digits
        .strip_prefix('.')
        .or_else(|| after_digits.strip_prefix('、'))
        .or_else(|| after_digits.strip_prefix(')'))
        .or_else(|| after_digits.strip_prefix('）'))?;
    Some((num, after_sep.trim_start()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{QWEN3_4B, SAKURA_7B};

    #[test]
    fn compute_context_tail_takes_last_two_successful_pairs() {
        let texts = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let results = vec!["甲".to_string(), "乙".to_string(), "丙".to_string()];
        let tail = compute_context_tail(&texts, &results, &[]);
        assert_eq!(
            tail,
            vec![("B".to_string(), "乙".to_string()), ("C".to_string(), "丙".to_string())]
        );
    }

    #[test]
    fn compute_context_tail_skips_failed_lines() {
        // 第 2 行(下标 1)失败,results[1] 是原文的拷贝——不该被当成"已确定的译文"带进
        // 下一批上下文,真正该取的是跳过它之后剩下的最后两行成功翻译。
        let texts = vec!["A".to_string(), "B".to_string(), "C".to_string(), "D".to_string()];
        let results = vec![
            "甲".to_string(),
            "B".to_string(), // 失败,原样保留原文
            "丙".to_string(),
            "丁".to_string(),
        ];
        let tail = compute_context_tail(&texts, &results, &[1]);
        assert_eq!(
            tail,
            vec![("C".to_string(), "丙".to_string()), ("D".to_string(), "丁".to_string())],
            "应该跳过失败的第 2 行,只取成功翻译里最后两行: {tail:?}"
        );
    }


    #[test]
    fn compute_context_tail_returns_fewer_than_two_when_not_enough_successful_lines() {
        let texts = vec!["A".to_string()];
        let results = vec!["甲".to_string()];
        let tail = compute_context_tail(&texts, &results, &[]);
        assert_eq!(tail, vec![("A".to_string(), "甲".to_string())]);

        let texts_all_failed = vec!["A".to_string(), "B".to_string()];
        let results_all_failed = vec!["A".to_string(), "B".to_string()];
        let tail_empty = compute_context_tail(&texts_all_failed, &results_all_failed, &[0, 1]);
        assert!(tail_empty.is_empty(), "全部失败时不该有任何上下文可用: {tail_empty:?}");
    }


    #[test]
    fn build_prompt_without_context_has_no_reference_block() {
        let (_, user) = build_prompt(&QWEN3_4B, "1. Hello", &[]);
        assert_eq!(user, "1. Hello", "没有上下文时正文不该被额外包装");
        assert!(!user.contains("紧接在前的上文"));
    }


    #[test]
    fn build_prompt_with_context_prepends_reference_block_before_body() {
        let context = vec![("Hello there.".to_string(), "你好。".to_string())];
        let (_, user) = build_prompt(&QWEN3_4B, "1. World", &context);
        assert!(user.contains("紧接在前的上文"), "应该包含上下文说明: {user}");
        assert!(user.contains("Hello there. → 你好。"), "应该包含具体的上文内容: {user}");
        let context_pos = user.find("Hello there.").unwrap();
        let body_pos = user.find("1. World").unwrap();
        assert!(context_pos < body_pos, "上下文应该在正文之前: {user}");
    }


    #[test]
    fn build_prompt_sakura_also_gets_context_block() {
        // Sakura 和 General 是两套不同的提示词模板,上下文注入要对两边都生效,不能只改
        // 其中一个分支就当作做完了。
        let context = vec![("こんにちは".to_string(), "你好".to_string())];
        let (system, user) = build_prompt(&SAKURA_7B, "1. さようなら", &context);
        assert!(user.contains("こんにちは → 你好"));
        assert!(user.contains("将下面的日文字幕文本翻译成中文"));
        assert!(system.contains("轻小说"));
    }


    /// 真实踩过的坑:失败行的 `translated` 值就是原文本身(见 translate_batch_with_retry
    /// 的单行兜底),按正常行的逻辑拼接会变成"原文\n原文",双语字幕里同一句话重复两遍。
    #[test]
    fn build_output_blocks_does_not_duplicate_original_text_for_failed_lines() {
        let blocks = vec![
            ("00:00:01,000".to_string(), "00:00:02,000".to_string(), "Hello".to_string()),
            ("00:00:02,000".to_string(), "00:00:03,000".to_string(), "World".to_string()),
        ];
        // 第 0 行翻译成功;第 1 行失败,translated[1] 是原文的拷贝(和真实失败路径一致)
        let translated = vec!["你好".to_string(), "World".to_string()];
        let out = build_output_blocks(&blocks, &translated, &[1], true);
        assert_eq!(out[0].2, "你好\nHello");
        assert_eq!(out[1].2, "World", "失败行双语模式下不应该重复输出原文");
    }


    #[test]
    fn build_output_blocks_success_lines_respect_bilingual_flag() {
        let blocks = vec![("00:00:01,000".to_string(), "00:00:02,000".to_string(), "Hello".to_string())];
        let translated = vec!["你好".to_string()];

        let bilingual = build_output_blocks(&blocks, &translated, &[], true);
        assert_eq!(bilingual[0].2, "你好\nHello");

        let zh_only = build_output_blocks(&blocks, &translated, &[], false);
        assert_eq!(zh_only[0].2, "你好");
    }


    #[test]
    fn parses_well_formed_numbered_response() {
        let content = "1. 你好\n2. 世界\n3. 再见";
        let out = parse_numbered_response(content, 3).unwrap();
        assert_eq!(out, vec!["你好", "世界", "再见"]);
    }


    #[test]
    fn accepts_fullwidth_and_alternate_separators() {
        let content = "1、第一行\n2）第二行\n3. 第三行";
        let out = parse_numbered_response(content, 3).unwrap();
        assert_eq!(out, vec!["第一行", "第二行", "第三行"]);
    }


    #[test]
    fn rejects_mismatched_line_count() {
        let content = "1. 只有一行";
        assert!(parse_numbered_response(content, 3).is_err());
    }


    /// 真实数据回归测试:这段内容是开发时用真实 Sakura-7B-Qwen2.5-v1.0(iq4xs 量化)模型跑
    /// llama-server 实测出来的原始响应(15 行,批大小和生产环境的 BATCH_SIZE 一致),不是
    /// 手写的理想化样例——用来确认真实模型输出里的全角标点(。！？，)、混合中英文标点
    /// 都能被正确解析、编号连续 1~15 不缺行。同一次实测也确认了两件事(不在本测试里体现,
    /// 记录在此供后续参照):llama-server 通过复用 whisper 引擎目录里的 CUDA 12.4 DLL 成功用
    /// 上了 GPU(生成速度约 69 token/s,3060 上纯 CPU 跑 7B 模型不可能有这个速度),且批量编号
    /// 提示词格式确实按预期让 Sakura 严格保留了编号、没有合并或跳过任何一行。
    #[test]
    fn parses_real_sakura_batch_response() {
        let content = "1. 早安。\n2. 今天天气真好。\n3. 一起去上学吧。\n4. 肚子饿了。\n\
                        5. 去吃点东西吧。\n6. 我觉得这是个好主意。\n7. 那间拉面店怎么样？\n\
                        8. 好像很好吃！\n9. 我也想去。\n10. 那就决定咯。\n\
                        11. 先问过老师之后再去吧。\n12. 好啊，毕竟迟到就糟了。\n\
                        13. 放学后再集合吧。\n14. 我很期待。\n15. 嗯，待会儿见。";
        let out = parse_numbered_response(content, 15).unwrap();
        assert_eq!(out.len(), 15);
        assert_eq!(out[0], "早安。");
        assert_eq!(out[6], "那间拉面店怎么样？");
        assert_eq!(out[11], "好啊，毕竟迟到就糟了。");
        assert_eq!(out[14], "嗯，待会儿见。");
    }


    #[test]
    fn rejects_missing_middle_line() {
        // 有 2 条内容但编号是 1 和 3,中间缺 2 —— 长度凑巧对上但不能被接受
        let content = "1. 第一行\n3. 第三行";
        assert!(parse_numbered_response(content, 2).is_err());
    }

    /// 真实踩过的坑:第 16 集连续 4 行原文是"you know, somewhere where you just take,
    /// you know,"这类口语碎片,模型原样把英文抄了回来当"译文",行数对得上、
    /// `parse_numbered_response` 完全测不出来。
    #[test]
    fn looks_untranslated_detects_verbatim_echo_of_english_source() {
        assert!(looks_untranslated(
            " somewhere where you just take, you know,",
            "somewhere where you just take, you know,"
        ));
    }

    #[test]
    fn looks_untranslated_is_case_and_whitespace_insensitive() {
        assert!(looks_untranslated("Hello World", "  hello world  "));
    }

    #[test]
    fn looks_untranslated_false_for_real_translation() {
        assert!(!looks_untranslated("Hello there.", "你好。"));
    }

    /// 纯符号/音乐记号(比如 whisper 给背景音乐标的"♪♪♪")本来就没有可翻译的文字内容,
    /// 模型原样返回是对的,不该被误判成"翻译失败"。
    #[test]
    fn looks_untranslated_false_for_content_without_letters() {
        assert!(!looks_untranslated("♪♪♪", "♪♪♪"));
        assert!(!looks_untranslated("123", "123"));
    }
}
