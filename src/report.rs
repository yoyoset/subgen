//! 翻译复核报告:从已生成的 `Video.srt`(原文)+ `Video.zh.srt`(译文)里粗筛可疑行,
//! 生成一份人能看懂、也能回灌的报告。
//!
//! 宽松粗筛,宁可多报——这里的用途是"人工核对/粘贴给 LLM 复核",不是自动重试。
//! `docs/issue-2026-07-25-翻译内容错位问题.md` 里否掉了"长度比值粗筛 → 自动检测并
//! 自动重试"这条路,原因是误报会直接污染字幕、必须低误报;但换成"导出报告 → 人工确认",
//! 误报只是多看一眼,同一个粗筛在新用途下重新可用。

use crate::translate::looks_untranslated;
use std::path::{Path, PathBuf};

/// 一行译文命中的可疑原因,人话描述,直接用于报告文案。
#[derive(Debug, Clone, PartialEq)]
pub struct SuspectLine {
    /// 0-based,对应 `parse_srt_blocks` 返回的顺序。
    pub index: usize,
    pub reasons: Vec<&'static str>,
}

/// 逐行比对原文/译文,标记疑似有问题的行。`original`/`translated` 必须是同一份字幕的
/// 原文块和译文块、按下标一一对应——`translate_srt` 本来就是 1:1 顺序写出的
/// (见 `translate/mod.rs`),行数不一致说明两个文件不是一对,直接报错而不是猜测对齐。
pub fn detect_suspects(
    original: &[(String, String, String)],
    translated: &[(String, String, String)],
) -> Result<Vec<SuspectLine>, String> {
    if original.len() != translated.len() {
        return Err(format!(
            "原文与译文行数不一致(原文 {} 行,译文 {} 行),可能不是同一份字幕,无法比对",
            original.len(),
            translated.len()
        ));
    }

    let mut reasons_by_index: std::collections::BTreeMap<usize, Vec<&'static str>> =
        std::collections::BTreeMap::new();

    for i in 0..original.len() {
        let orig = &original[i].2;
        let trans = &translated[i].2;
        let mut reasons = Vec::new();
        if looks_untranslated(orig, trans) {
            reasons.push("原样未译");
        }
        if has_residual_latin(trans) {
            reasons.push("译文残留英文");
        }
        if is_residue(orig, trans) {
            reasons.push("译文明显过短(疑似内容被前一行吞并)");
        }
        if is_length_outlier(orig, trans) {
            reasons.push("长度比例异常");
        }
        if !reasons.is_empty() {
            reasons_by_index.insert(i, reasons);
        }
    }

    // 相邻行重复要单独一趟扫描:判断第 i 行时需要看 i-1/i+1,不能和上面那趟单行检查合并。
    let translated_text: Vec<&str> = translated.iter().map(|(_, _, t)| t.as_str()).collect();
    for i in 0..translated_text.len() {
        if is_adjacent_duplicate(&translated_text, i) {
            reasons_by_index.entry(i).or_default().push("与相邻行译文重复");
        }
    }

    Ok(reasons_by_index
        .into_iter()
        .map(|(index, reasons)| SuspectLine { index, reasons })
        .collect())
}

/// 译文里混了一大段拉丁字母——真实案例(见 docs/issue-2026-07-25-*.md):内容错位时
/// 残渣行可能是"rim。"这种只剩一两个英文单词混进中文标点的样子。用字母占比而不是
/// "只要有字母就算"——避免误伤正常中文句子里夹的品牌名/术语这种少量英文。
fn has_residual_latin(translated: &str) -> bool {
    let letters = translated.chars().filter(|c| c.is_ascii_alphabetic()).count();
    let total = translated.chars().filter(|c| !c.is_whitespace()).count();
    total > 0 && letters >= 3 && (letters as f64 / total as f64) > 0.4
}

/// 原文一整句、译文只剩几个字——真实案例:19 个词的原文对应"中心位置。"这种 5 个字的
/// 残渣,内容大概率被并入了前一行。阈值宽松,故意多报。
fn is_residue(original: &str, translated: &str) -> bool {
    let orig_words = original.split_whitespace().count();
    let trans_chars = translated.chars().filter(|c| !c.is_whitespace()).count();
    orig_words >= 8 && trans_chars <= 6
}

/// 译文字数相对原文词数的比例明显偏离正常翻译的区间。字幕天生是半句碎片,短原文的比例
/// 天然噪音大,原文词数太少(< 3)时不参与判断,避免制造大量没有意义的噪音。
fn is_length_outlier(original: &str, translated: &str) -> bool {
    let orig_words = original.split_whitespace().count();
    if orig_words < 3 {
        return false;
    }
    let trans_chars = translated.chars().filter(|c| !c.is_whitespace()).count();
    let ratio = trans_chars as f64 / orig_words as f64;
    !(0.6..=3.2).contains(&ratio)
}

/// 和前一行或后一行译文(去除首尾空白后)完全相同——真实用户反馈"有一些翻译有重复"
/// 里最直接对应的情况。单字符的重复(比如两行都只是句号)没有信息量,不算。
fn is_adjacent_duplicate(translated: &[&str], i: usize) -> bool {
    let cur = translated[i].trim();
    if cur.chars().count() <= 1 {
        return false;
    }
    let prev_dup = i > 0 && translated[i - 1].trim() == cur;
    let next_dup = i + 1 < translated.len() && translated[i + 1].trim() == cur;
    prev_dup || next_dup
}

/// 把命中下标按前后各 `padding` 行的邻近范围合并成连续区间,相邻/重叠的区间会被拼成一个
/// 更大的区间,避免报告里出现大量互相紧挨着的碎小片段。`indices` 必须已经按升序排列。
fn group_into_intervals(indices: &[usize], total_len: usize, padding: usize) -> Vec<(usize, usize)> {
    let mut intervals: Vec<(usize, usize)> = Vec::new();
    for &i in indices {
        let start = i.saturating_sub(padding);
        let end = (i + padding).min(total_len.saturating_sub(1));
        if let Some(last) = intervals.last_mut()
            && start <= last.1 + 1 {
                last.1 = last.1.max(end);
                continue;
            }
        intervals.push((start, end));
    }
    intervals
}

/// 生成自足的复核报告文本:按连续区间列出可疑行(前后各带 1 行上下文),每行给序号、
/// 时间轴、原文、译文,可疑行额外标出触发原因。序号格式(`"129. 文本"`)和
/// `translate::prompt::split_leading_number` 能解析的格式一致,方便把修正结果整行粘贴回来。
pub fn format_review_report(
    video_label: &str,
    original: &[(String, String, String)],
    translated: &[(String, String, String)],
    suspects: &[SuspectLine],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("=== {video_label} 翻译复核报告 ===\n"));

    if suspects.is_empty() {
        out.push_str(&format!(
            "共 {} 行,粗筛未发现明显可疑的行。这只是粗筛,不代表一定没有问题,\
             如果观感上仍觉得某处翻译不对,建议直接播放对应时间点核对。\n",
            translated.len()
        ));
        return out;
    }

    out.push_str(&format!(
        "共 {} 行,标记 {} 处疑似问题(宽松粗筛,可能有误报,请人工确认;\
         需要修正时,直接按 “编号. 修正后的译文” 逐行整行回复即可)\n",
        translated.len(),
        suspects.len()
    ));

    let indices: Vec<usize> = suspects.iter().map(|s| s.index).collect();
    let intervals = group_into_intervals(&indices, translated.len(), 1);
    let reasons_by_index: std::collections::HashMap<usize, &Vec<&'static str>> =
        suspects.iter().map(|s| (s.index, &s.reasons)).collect();

    for (start, end) in intervals {
        out.push_str(&format!("\n--- 第 {}-{} 行 ---\n", start + 1, end + 1));
        for i in start..=end {
            let (ts_start, ts_end, _) = &translated[i];
            let orig_text = &original[i].2;
            let trans_text = &translated[i].2;
            match reasons_by_index.get(&i) {
                Some(reasons) => {
                    out.push_str(&format!(
                        "{}. [疑似问题: {}] {ts_start} --> {ts_end}\n",
                        i + 1,
                        reasons.join("、")
                    ));
                }
                None => {
                    out.push_str(&format!("{}. [上下文] {ts_start} --> {ts_end}\n", i + 1));
                }
            }
            out.push_str(&format!("     原文: {orig_text}\n"));
            out.push_str(&format!("     译文: {trans_text}\n"));
        }
    }

    out
}

/// 一次生成好的复核报告的成品数据,交给界面持有(`SubgenApp.review`)。和
/// `format_review_report` 的纯文本返回不同,这里把"需要界面临时保存、再供粘贴修正回灌"
/// 的所有东西一次带齐。
pub struct ReviewFileData {
    pub text: String,
    pub suspect_indices: Vec<usize>,
    pub saved_path: PathBuf,
}

/// 为 `video_path` 对应任务生成复核报告:读原文/译文两份 srt、跑粗筛、格式化成文本、
/// 写到视频旁的 `.zh.review.txt`,一步到位返回结果。
///
/// 中文字幕文件可能是双语的("中文\n原文"两行一块)也可能是仅中文单行——不管哪种,
/// 写出时中文永远是每块的第一行(见 `translate::build_output_blocks`),所以这里用
/// `parse_srt_blocks_lines` 取每块的行数组、只拿第一行,而不是 `parse_srt_blocks`
/// (它会把多行块用空格拼接,双语文件会把中文和原文拼成一坨、失去边界)。
///
/// 报告文件写入失败会作为 Err 返回(而不是静默吞掉)——报告写到视频旁本来就是为了
/// 随时能翻看,写不进去只让用户在界面里看一眼,是"后台失败但界面看起来成功"的一种。
pub fn generate_review_report(video_path: &Path, label: &str) -> Result<ReviewFileData, String> {
    let original_path = crate::paths::sidecar_srt_path(video_path);
    let zh_path = crate::paths::sidecar_srt_lang_path(video_path, "zh");

    let original_content = std::fs::read_to_string(&original_path)
        .map_err(|e| format!("找不到原文字幕: {e}"))?;
    let zh_content =
        std::fs::read_to_string(&zh_path).map_err(|e| format!("找不到中文字幕: {e}"))?;

    let original_blocks = crate::engine::parse_srt_blocks(&original_content);
    let zh_blocks: Vec<(String, String, String)> =
        crate::engine::parse_srt_blocks_lines(&zh_content)
            .into_iter()
            .map(|(start, end, lines)| (start, end, lines.into_iter().next().unwrap_or_default()))
            .collect();

    let suspects = detect_suspects(&original_blocks, &zh_blocks)?;
    let suspect_indices: Vec<usize> = suspects.iter().map(|s| s.index).collect();
    let text = format_review_report(label, &original_blocks, &zh_blocks, &suspects);

    let saved_path = crate::paths::sidecar_review_path(video_path);
    std::fs::write(&saved_path, &text).map_err(|e| format!("保存复核报告失败: {e}"))?;

    Ok(ReviewFileData {
        text,
        suspect_indices,
        saved_path,
    })
}

/// 把用户粘贴回来的编号行("129. 修正后的译文")应用到 `.zh.srt`。`allowed` 是这次报告
/// 真正标记为可疑的下标,不认识的编号一律忽略并在返回值里列出来——不该让一次误粘贴
/// 覆盖到没有被标记过的行。
///
/// 双语字幕每块有"中文\n原文"两行,只替换第一行(中文),保留后面的原文行不动;
/// 原文件先备份成 `.zh.srt.bak` 再覆盖写入(备份/写入失败都作为 Err 返回,不让用户
/// 以为改成功了其实文件根本没动)。
///
/// 返回 `(成功应用了几处, 被忽略的编号)`。
pub fn apply_corrections(
    zh_path: &Path,
    allowed: &std::collections::HashSet<usize>,
    pasted: &str,
) -> Result<(usize, Vec<usize>), String> {
    let zh_content =
        std::fs::read_to_string(zh_path).map_err(|e| format!("读取中文字幕失败: {e}"))?;
    let mut blocks = crate::engine::parse_srt_blocks_lines(&zh_content);

    let mut applied = 0usize;
    let mut ignored: Vec<usize> = Vec::new();
    for line in pasted.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((num, text)) = crate::translate::split_leading_number(line) else {
            continue;
        };
        let index = num.saturating_sub(1);
        if !allowed.contains(&index) {
            ignored.push(num);
            continue;
        }
        if let Some((_, _, lines)) = blocks.get_mut(index) {
            if lines.is_empty() {
                lines.push(text.to_string());
            } else {
                lines[0] = text.to_string();
            }
            applied += 1;
        }
    }

    if applied == 0 {
        return Ok((0, ignored));
    }

    let backup_path = PathBuf::from(format!("{}.bak", zh_path.display()));
    std::fs::write(&backup_path, &zh_content).map_err(|e| format!("备份原字幕文件失败: {e}"))?;

    let out_blocks: Vec<(String, String, String)> = blocks
        .into_iter()
        .map(|(start, end, lines)| (start, end, lines.join("\n")))
        .collect();
    // 写临时文件 + 原子改名覆盖,而不是直接 truncate+write——后者中途失败(比如磁盘满)
    // 会把用户的有效字幕截断成半截。改名是同一目录内的原子操作,失败时原文件保持完好。
    let tmp_path = PathBuf::from(format!("{}.new", zh_path.display()));
    std::fs::write(&tmp_path, crate::engine::write_srt_blocks(&out_blocks))
        .map_err(|e| format!("写入中文字幕失败: {e}"))?;
    std::fs::rename(&tmp_path, zh_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("替换中文字幕失败: {e}")
    })?;

    Ok((applied, ignored))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(start: &str, end: &str, text: &str) -> (String, String, String) {
        (start.to_string(), end.to_string(), text.to_string())
    }

    #[test]
    fn detect_suspects_errors_on_length_mismatch() {
        let original = vec![block("00:00:01,000", "00:00:02,000", "Hello")];
        let translated = vec![
            block("00:00:01,000", "00:00:02,000", "你好"),
            block("00:00:02,000", "00:00:03,000", "多余的一行"),
        ];
        let err = detect_suspects(&original, &translated).unwrap_err();
        assert!(err.contains("行数不一致"), "{err}");
    }

    #[test]
    fn detect_suspects_flags_real_empty_translation_case() {
        // 真实案例(端到端跑真实文件时发现,02 集第 66 块,机械摘录):译文整行是空字符串
        // ——之前完全没有已知的测试覆盖这种"彻底没译出来"的情况(和"原样抄回英文"是不同
        // 的失败模式,looks_untranslated 要求原文含字母且和译文完全相同,空字符串不满足)。
        let original = vec![block(
            "00:05:26,120",
            "00:05:30,800",
            " feet you basically take the arch of your foot and you plant that into the into",
        )];
        let translated = vec![block("00:05:26,120", "00:05:30,800", "")];
        let suspects = detect_suspects(&original, &translated).unwrap();
        assert_eq!(suspects.len(), 1);
        assert!(suspects[0].reasons.contains(&"译文明显过短(疑似内容被前一行吞并)"));
    }

    #[test]
    fn detect_suspects_flags_verbatim_echo() {
        // 真实案例(16 集修复前):连续几行原样把英文抄回来当译文。
        let original = vec![block(
            "00:03:20,360",
            "00:03:22,180",
            "somewhere where you just take, you know,",
        )];
        let translated = vec![block(
            "00:03:20,360",
            "00:03:22,180",
            "somewhere where you just take, you know,",
        )];
        let suspects = detect_suspects(&original, &translated).unwrap();
        assert_eq!(suspects.len(), 1);
        assert!(suspects[0].reasons.contains(&"原样未译"));
    }

    #[test]
    fn detect_suspects_flags_real_residue_case() {
        // 真实案例(02 集第 131 块,机械摘录):19 个词的原文只剩 5 个字的中文残渣,
        // 内容被并入了前一行(129/130 块)。
        let original = vec![block(
            "00:09:46,340",
            "00:09:50,500",
            "rim. Some people say they look at the square and trying to line up the ball right in between the",
        )];
        let translated = vec![block("00:09:46,340", "00:09:50,500", "中心位置。")];
        let suspects = detect_suspects(&original, &translated).unwrap();
        assert_eq!(suspects.len(), 1);
        assert!(suspects[0].reasons.contains(&"译文明显过短(疑似内容被前一行吞并)"));
        assert!(suspects[0].reasons.contains(&"长度比例异常"));
    }

    #[test]
    fn detect_suspects_flags_residual_latin_case() {
        // 真实案例(02 集第 130 块修复前,机械摘录):译文残渣里混了英文单词。
        let original = vec![block("00:09:41,240", "00:09:46,340", "lock in on as a target?")];
        let translated = vec![block("00:09:41,240", "00:09:46,340", "rim。")];
        let suspects = detect_suspects(&original, &translated).unwrap();
        assert!(suspects[0].reasons.contains(&"译文残留英文"));
    }

    #[test]
    fn detect_suspects_flags_adjacent_duplicate() {
        let original = vec![
            block("00:00:01,000", "00:00:02,000", "First line here"),
            block("00:00:02,000", "00:00:03,000", "Second line here"),
        ];
        let translated = vec![
            block("00:00:01,000", "00:00:02,000", "重复的译文内容"),
            block("00:00:02,000", "00:00:03,000", "重复的译文内容"),
        ];
        let suspects = detect_suspects(&original, &translated).unwrap();
        assert_eq!(suspects.len(), 2);
        assert!(suspects[0].reasons.contains(&"与相邻行译文重复"));
        assert!(suspects[1].reasons.contains(&"与相邻行译文重复"));
    }

    #[test]
    fn detect_suspects_does_not_flag_normal_translation() {
        // 真实案例(02 集第 128 块,已确认是正常翻译,机械摘录),不该被误标。
        let original = vec![block(
            "00:09:27,680",
            "00:09:30,240",
            "center position if you train yourself what that feels like.",
        )];
        let translated = vec![block(
            "00:09:27,680",
            "00:09:30,240",
            "中锋位置如果你能训练自己感受那种感觉。",
        )];
        let suspects = detect_suspects(&original, &translated).unwrap();
        assert!(suspects.is_empty(), "{suspects:?}");
    }

    #[test]
    fn detect_suspects_ignores_short_fragments_without_noise() {
        // 原文词数太少(< 3)时不该参与长度比例判断,字幕里大量存在这种短碎片。
        let original = vec![block("00:00:01,000", "00:00:02,000", "Yeah.")];
        let translated = vec![block("00:00:01,000", "00:00:02,000", "是的。")];
        let suspects = detect_suspects(&original, &translated).unwrap();
        assert!(suspects.is_empty(), "{suspects:?}");
    }

    #[test]
    fn group_into_intervals_merges_overlapping_ranges() {
        // 下标 2 和 4 各自扩展 1 行是 [1,3] 和 [3,5],在 3 处重叠,应该合并成一个区间。
        let intervals = group_into_intervals(&[2, 4], 10, 1);
        assert_eq!(intervals, vec![(1, 5)]);
    }

    #[test]
    fn group_into_intervals_keeps_far_apart_ranges_separate() {
        let intervals = group_into_intervals(&[1, 8], 10, 1);
        assert_eq!(intervals, vec![(0, 2), (7, 9)]);
    }

    #[test]
    fn group_into_intervals_clamps_to_valid_range() {
        // 下标 0 往前扩 1 行会下溢(usize),下标末尾往后扩会越界,都要被夹紧。
        let intervals = group_into_intervals(&[0, 4], 5, 1);
        assert_eq!(intervals, vec![(0, 1), (3, 4)]);
    }

    #[test]
    fn format_review_report_lists_no_suspects_message_when_empty() {
        let original = vec![block("00:00:01,000", "00:00:02,000", "Hello there")];
        let translated = vec![block("00:00:01,000", "00:00:02,000", "你好")];
        let report = format_review_report("测试视频", &original, &translated, &[]);
        assert!(report.contains("未发现明显可疑的行"), "{report}");
    }

    #[test]
    fn format_review_report_includes_context_and_reason_and_reingestible_numbering() {
        let original = vec![
            block("00:00:01,000", "00:00:02,000", "Context line before it"),
            block(
                "00:00:02,000",
                "00:00:03,000",
                "somewhere where you just take, you know,",
            ),
            block("00:00:03,000", "00:00:04,000", "Context line after it"),
        ];
        let translated = vec![
            block("00:00:01,000", "00:00:02,000", "前面的上下文行"),
            block(
                "00:00:02,000",
                "00:00:03,000",
                "somewhere where you just take, you know,",
            ),
            block("00:00:03,000", "00:00:04,000", "后面的上下文行"),
        ];
        let suspects = detect_suspects(&original, &translated).unwrap();
        let report = format_review_report("测试视频", &original, &translated, &suspects);

        assert!(report.contains("=== 测试视频 翻译复核报告 ==="));
        // 整行原样抄回英文,本来就会同时命中好几条启发式(原样未译/残留英文/长度比例),
        // 不需要恰好只命中一条——这里只确认关键的那条("原样未译")确实出现在原因列表里。
        let line2 = report.lines().find(|l| l.starts_with("2. [疑似问题:")).expect("应该有第2行");
        assert!(line2.contains("原样未译"), "{line2}");
        assert!(report.contains("1. [上下文]"), "上下文行应该被带出: {report}");
        assert!(report.contains("3. [上下文]"), "上下文行应该被带出: {report}");
        assert!(report.contains("原文: somewhere where you just take, you know,"));
        assert!(report.contains("译文: somewhere where you just take, you know,"));
    }

    fn temp_subdir(name: &str) -> PathBuf {
        let unique = format!(
            "{}_{:x}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join("subgen_report_test").join(unique)
    }

    /// 双语字幕(中文\n原文 两行一块)修正时只替换第一行(中文),原文行保留不动,
    /// 并且原文件先备份成 .bak。
    #[test]
    fn apply_corrections_replaces_first_line_only_in_bilingual_blocks() {
        let dir = temp_subdir("apply_corrections_bilingual");
        std::fs::create_dir_all(&dir).unwrap();
        let zh = dir.join("Episode.zh.srt");
        std::fs::write(
            &zh,
            "1\n00:00:01,000 --> 00:00:02,000\n旧译文\nHello there.\n\n",
        )
        .unwrap();
        let allowed: std::collections::HashSet<usize> = [0usize].into_iter().collect();

        let (applied, ignored) =
            apply_corrections(&zh, &allowed, "1. 新译文\n999. 不认识的编号").unwrap();

        assert_eq!(applied, 1);
        assert_eq!(ignored, vec![999], "不在标记范围内的编号应该被忽略并列出");
        let content = std::fs::read_to_string(&zh).unwrap();
        assert!(
            content.contains("新译文\nHello there."),
            "应该只替换第一行(中文),原文行保留: {content}"
        );
        assert!(
            dir.join("Episode.zh.srt.bak").exists(),
            "覆盖写入前应该先备份原文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没有任何编号命中 allowed 时不应改动文件、不应产生备份。
    #[test]
    fn apply_corrections_with_no_matching_number_leaves_file_untouched() {
        let dir = temp_subdir("apply_corrections_none");
        std::fs::create_dir_all(&dir).unwrap();
        let zh = dir.join("Episode.zh.srt");
        std::fs::write(&zh, "1\n00:00:01,000 --> 00:00:02,000\n旧译文\n\n").unwrap();
        let allowed: std::collections::HashSet<usize> = std::collections::HashSet::new();

        let (applied, ignored) = apply_corrections(&zh, &allowed, "1. 新译文").unwrap();

        assert_eq!(applied, 0);
        assert_eq!(ignored, vec![1]);
        assert_eq!(
            std::fs::read_to_string(&zh).unwrap(),
            "1\n00:00:01,000 --> 00:00:02,000\n旧译文\n\n",
            "没有可应用的修正时文件不该被改动"
        );
        assert!(
            !dir.join("Episode.zh.srt.bak").exists(),
            "没有写入就不该产生备份"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 生成报告时,原样未译的行应该被粗筛标记,报告文件写到视频旁,
    /// 返回的 suspect_indices 和报告内容一致。
    #[test]
    fn generate_review_report_round_trips_and_marks_verbatim_echo() {
        let dir = temp_subdir("generate_report_roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("Episode.mkv");
        // sidecar_srt_path/sidecar_srt_lang_path 按视频名推导出同目录的 .srt/.zh.srt。
        std::fs::write(
            dir.join("Episode.srt"),
            "1\n00:00:01,000 --> 00:00:02,000\nsomewhere where you just take, you know,\n\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("Episode.zh.srt"),
            "1\n00:00:01,000 --> 00:00:02,000\nsomewhere where you just take, you know,\n\n",
        )
        .unwrap();

        let data = generate_review_report(&video, "Episode").unwrap();

        assert_eq!(data.suspect_indices, vec![0], "原样未译的行应该被标记");
        assert!(data.text.contains("原样未译"), "报告里应该说明触发原因: {}", data.text);
        assert_eq!(data.saved_path, dir.join("Episode.zh.review.txt"));
        assert!(data.saved_path.is_file(), "报告文件应该真的写到了视频旁边");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
