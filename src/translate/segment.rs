//! 把"一句话被 Whisper 切成好几条时间轴"的碎片,在送去翻译前合并成完整句子,翻译回来后
//! 再按比例切回原来的条数。
//!
//! 背景:批量翻译时模型偶尔会把这类碎片当一句话合并输出,但因为提示词要求严格保留编号
//! 格式,它依然会凑齐满编号的输出——编号在,内容却整体错位了一行,而且这个错位会一直
//! 持续到后面某次巧合的重复行才重新对齐(2026-08-01 用真实素材复现过,见项目 memory
//! `translation_quality_status`)。与其反复要求模型"别合并",不如在送去翻译之前就把
//! 该合并的碎片合并好,合并的判断权从模型手里收回到这里的确定性规则。
//!
//! 这个文件目前只有纯函数,还没有接入 `translate_srt` 的实际批量翻译流程(见
//! `plan-sonnet-jazzy-stardust.md` 第 2 步)——先拿真实素材验证分组/切分准不准,数据不
//! 好看就不用往下走。

/// 一句话结尾的标点(结合真实素材:英文台词几乎不会用中文标点收尾,这里只列英文/通用符号)。
/// 命中说明这一行本身就是完整句子的结尾,不该往下一行合并。
const SENTENCE_END: &[char] = &['.', '!', '?', '"', '”', '♪'];

/// 一组最多合并几条原文行。真实素材(`E:\season 11`,约 3400 行)里如果不设上限,会出现
/// 9/13/23 行的超大组——这些全部来自 Whisper 输出完全没有标点、整段小写的区域,不设上限
/// 会把 23 行揉成一坨发给模型,比现在的逐行翻译还糟。
const MAX_GROUP_LINES: usize = 4;

/// 一组合并后的最大字符数(粗略按原文字符数算,和 `MAX_GROUP_LINES` 同一个目的的双保险)。
const MAX_GROUP_CHARS: usize = 200;

/// 判断 `cur`/`next` 是不是同一句话被切成的两条碎片:`cur` 不以句末标点结尾、且 `next`
/// 以小写字母开头。只用这两个信号,不去猜测更复杂的语法结构——宁可漏判(退化成不合并,
/// 就是今天的行为),不误判(把两句独立的话强行粘在一起)。
fn is_fragment_boundary(cur: &str, next: &str) -> bool {
    let cur = cur.trim();
    let next = next.trim();
    let Some(last) = cur.chars().last() else {
        return false;
    };
    if SENTENCE_END.contains(&last) {
        return false;
    }
    let Some(first) = next.chars().next() else {
        return false;
    };
    first.is_alphabetic() && first.is_lowercase()
}

/// 把逐行原文分组:组内下标是同一句话的碎片,应该合并翻译。行数或字符数超过硬上限时
/// 强制断开,即使 `is_fragment_boundary` 判定还该继续合并——见上面两个常量的说明。
/// 无法安全合并的地方每组只有 1 行,和今天逐行翻译的行为完全一致,不会有回归。
#[allow(dead_code)] // 见 plan-sonnet-jazzy-stardust.md 第 2 步,尚未接入 translate_srt
pub(super) fn group_fragments(lines: &[String]) -> Vec<Vec<usize>> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = vec![0];
    let mut current_chars = lines[0].chars().count();

    for i in 0..lines.len() - 1 {
        let next_chars = lines[i + 1].chars().count();
        let within_limits =
            current.len() < MAX_GROUP_LINES && current_chars + next_chars <= MAX_GROUP_CHARS;
        if within_limits && is_fragment_boundary(&lines[i], &lines[i + 1]) {
            current.push(i + 1);
            current_chars += next_chars;
        } else {
            groups.push(std::mem::take(&mut current));
            current = vec![i + 1];
            current_chars = next_chars;
        }
    }
    groups.push(current);
    groups
}

/// 组内几条原文拼成一句完整的话给模型翻译。用空格连接——这些本来就是英文句子的碎片,
/// 空格是正确的词间分隔。
#[allow(dead_code)] // 见 plan-sonnet-jazzy-stardust.md 第 2 步,尚未接入 translate_srt
pub(super) fn merge_group(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|s| s.trim())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 切点优先吸附到的中文/通用标点——译文是中文,切在标点后读起来才不别扭。
const SPLIT_ANCHORS: &[char] = &['，', '。', '、', '；', '：', '！', '？', ',', '.', '!', '?'];

/// 把合并翻译回来的一句话,按 `weights`(通常是各原文行的词数)的比例切回原来的条数。
/// 切点先按比例算出大致位置,再在前后各 4 个字符内找最近的标点吸附,找不到就用比例位置
/// 硬切。**段数恒等于 `weights.len()`**,不管切得好不好看——这是保证 `translate_srt`
/// 里 N 进 N 出的前提,`build_output_blocks` 依赖这个不变量。
#[allow(dead_code)] // 见 plan-sonnet-jazzy-stardust.md 第 2 步,尚未接入 translate_srt
pub(super) fn split_by_weights(translated: &str, weights: &[usize]) -> Vec<String> {
    if weights.is_empty() {
        return Vec::new();
    }
    if weights.len() == 1 {
        return vec![translated.to_string()];
    }

    let chars: Vec<char> = translated.chars().collect();
    let n = chars.len();
    let total_weight: usize = weights.iter().sum::<usize>().max(1);

    let mut cuts: Vec<usize> = Vec::with_capacity(weights.len() - 1);
    let mut acc = 0usize;
    for &w in &weights[..weights.len() - 1] {
        acc += w;
        let target = ((n * acc) as f64 / total_weight as f64).round() as usize;
        let target = target.min(n);

        let mut best = target;
        'search: for off in 0..=4usize {
            for cand in [target.checked_sub(off), target.checked_add(off)].into_iter().flatten() {
                if cand > 0 && cand < n && SPLIT_ANCHORS.contains(&chars[cand - 1]) {
                    best = cand;
                    break 'search;
                }
            }
        }

        // 切点必须不小于上一个切点,否则会产生反向切片——真实数据里权重都是正数、acc
        // 单调递增,理论上不会发生,这里是防止标点吸附把切点拉到前一个切点之前的兜底。
        let prev_cut = cuts.last().copied().unwrap_or(0);
        cuts.push(best.max(prev_cut).min(n));
    }

    let mut parts = Vec::with_capacity(weights.len());
    let mut prev = 0usize;
    for &c in &cuts {
        parts.push(chars[prev..c].iter().collect());
        prev = c;
    }
    parts.push(chars[prev..].iter().collect());
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn group_fragments_empty_input_returns_empty() {
        assert_eq!(group_fragments(&[]), Vec::<Vec<usize>>::new());
    }

    #[test]
    fn group_fragments_single_line_is_its_own_group() {
        assert_eq!(group_fragments(&s(&["Hello there."])), vec![vec![0]]);
    }

    #[test]
    fn group_fragments_merges_sentence_split_across_two_lines() {
        // 真实案例(E03 老挝集第 47-48 行):同一句问句被切成两条时间轴。
        let lines = s(&[
            "...what's the first sort of recognizable smell",
            "that registers?",
        ]);
        assert_eq!(group_fragments(&lines), vec![vec![0, 1]]);
    }

    #[test]
    fn group_fragments_does_not_merge_when_current_ends_with_sentence_punctuation() {
        let lines = s(&["This is complete.", "And so is this."]);
        assert_eq!(group_fragments(&lines), vec![vec![0], vec![1]]);
    }

    #[test]
    fn group_fragments_does_not_merge_when_next_starts_uppercase() {
        // 下一行大写开头,通常是新句子的开始,即使当前行没有句末标点也不合并。
        let lines = s(&["Wait", "The next sentence starts here"]);
        assert_eq!(group_fragments(&lines), vec![vec![0], vec![1]]);
    }

    #[test]
    fn group_fragments_respects_max_lines_cap() {
        // 4 行都满足碎片条件,但硬上限是 4 行一组,第 5 行必须另起一组。
        let lines = s(&[
            "one two three four five and", "six seven eight nine ten and",
            "eleven twelve thirteen and", "fourteen fifteen sixteen and",
            "seventeen eighteen nineteen.",
        ]);
        let groups = group_fragments(&lines);
        assert_eq!(groups[0], vec![0, 1, 2, 3]);
        assert_eq!(groups[1], vec![4]);
    }

    #[test]
    fn group_fragments_respects_max_chars_cap() {
        let long_a = format!("{} and", "a".repeat(150));
        let long_b = "b".repeat(150);
        let lines = s(&[&long_a, &long_b]);
        // 单条已经 150+ 字符,加起来超过 200 上限,不能合并成一组。
        assert_eq!(group_fragments(&lines), vec![vec![0], vec![1]]);
    }

    #[test]
    fn merge_group_joins_with_single_space_and_trims() {
        assert_eq!(
            merge_group(&["  Hello  ", "world."]),
            "Hello world."
        );
    }

    #[test]
    fn split_by_weights_single_weight_returns_whole_string_unsplit() {
        assert_eq!(split_by_weights("完整的一句话", &[3]), vec!["完整的一句话"]);
    }

    #[test]
    fn split_by_weights_returns_exactly_weights_len_segments() {
        // 真实场景兜底:即使模型返回的译文很短、标点很怪,段数也必须精确等于 weights.len(),
        // 这是 translate_srt 里 N 进 N 出的前提。
        for text in ["", "短", "一个没有任何标点的很长句子测试兜底逻辑"] {
            for weights in [vec![1usize, 1], vec![2, 3, 1], vec![1, 1, 1, 1]] {
                let parts = split_by_weights(text, &weights);
                assert_eq!(
                    parts.len(),
                    weights.len(),
                    "text={text:?} weights={weights:?} parts={parts:?}"
                );
            }
        }
    }

    #[test]
    fn split_by_weights_snaps_to_punctuation_near_target() {
        // "好的，你现在看到的是1095钢，完美适用。" 按词数比例 [3, 6] 切,理想切点在标点上。
        let text = "好的，你现在看到的是1095钢，完美适用。";
        let parts = split_by_weights(text, &[3, 6]);
        assert_eq!(parts.len(), 2);
        // 两段拼回去必须是原文,不能丢字/多字。
        assert_eq!(parts.concat(), text);
    }

    #[test]
    fn split_by_weights_concat_always_reconstructs_original_text() {
        // 不管切得好不好看,拼回去必须是原字符串——这是切分正确性的底线。
        let text = "从我第一次听说老挝的时候，我就被吸引住了，充满了去亲眼看看这个地方的渴望。";
        let parts = split_by_weights(text, &[5, 3, 4, 2]);
        assert_eq!(parts.concat(), text);
    }
}
