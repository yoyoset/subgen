use std::path::Path;

/// 数一份 srt 里有多少条字幕块,读不到就当 0(不该因为这个附加信息让整个任务失败)。
pub(super) fn count_subtitle_blocks(srt_path: &Path) -> usize {
    std::fs::read_to_string(srt_path)
        .map(|content| parse_srt_blocks(&content).len())
        .unwrap_or(0)
}

/// 判断复用的 srt 是不是"看起来完整"的产物,而不是上次处理中断留下的半成品:内容为空
/// (有些片段确实没有对白,空字幕合理)或者能解析出至少一块有效字幕,都算完整。
pub(super) fn srt_looks_complete(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(content) => content.trim().is_empty() || !parse_srt_blocks(&content).is_empty(),
        Err(_) => false,
    }
}


/// whisper.cpp 已知的"复读机"故障模式:长文件连续解码偶发进入死循环,同一句话原样
/// 重复几十上百次(实测过,真实出现:同一句 983 次)。这里检测连续 >= 4 次完全相同的
/// 字幕文本,视为异常并折叠成一条(时间跨度取首尾),避免把成百上千行垃圾原样交付给用户。
/// 返回首个被折叠片段的起始时间戳(如 "00:08:06,000")——不只是"有没有",界面据此能告诉
/// 用户具体去哪个时间点抽查,而不是让用户对着几十分钟的视频从头听起。没检测到返回 None。
pub(super) fn collapse_repetitions(srt_path: &Path) -> Result<Option<String>, String> {
    const REPEAT_THRESHOLD: usize = 4;

    let content =
        std::fs::read_to_string(srt_path).map_err(|e| format!("读取字幕文件失败: {e}"))?;
    let blocks = parse_srt_blocks(&content);

    let mut collapsed = Vec::with_capacity(blocks.len());
    let mut first_repetition_at: Option<String> = None;
    let mut i = 0;
    while i < blocks.len() {
        let text = blocks[i].2.trim();
        let mut j = i + 1;
        while j < blocks.len() && blocks[j].2.trim() == text {
            j += 1;
        }
        let run_len = j - i;
        if run_len >= REPEAT_THRESHOLD {
            if first_repetition_at.is_none() {
                first_repetition_at = Some(blocks[i].0.clone());
            }
            let start = blocks[i].0.clone();
            let end = blocks[j - 1].1.clone();
            collapsed.push((start, end, blocks[i].2.clone()));
        } else {
            collapsed.extend_from_slice(&blocks[i..j]);
        }
        i = j;
    }

    if first_repetition_at.is_some() {
        std::fs::write(srt_path, write_srt_blocks(&collapsed))
            .map_err(|e| format!("写回字幕文件失败: {e}"))?;
    }
    Ok(first_repetition_at)
}

/// 和 `parse_srt_blocks` 共享同一套块切分逻辑,但保留每块内部原始的多行结构、不拿空格
/// 拼接——`parse_srt_blocks` 拼接是给"重复检测/整块喂给翻译模型"这类只关心整块文本的
/// 场景用的,但双语字幕("中文\n原文"两行一块,见 `translate::build_output_blocks`)拼接后
/// 会把中文和原文糊成一坨、丢掉行边界。`report.rs` 的复核报告需要单独取出第一行(不管
/// 双语还是仅中文模式,写出时中文永远是每块的第一行)时就需要这份没拼接过的版本。
pub(crate) fn parse_srt_blocks_lines(content: &str) -> Vec<(String, String, Vec<String>)> {
    let mut out = Vec::new();
    // 去掉开头的 UTF-8 BOM(\u{feff})——Windows 记事本"另存为 UTF-8"会在文件头写 BOM,
    // 不清掉的话第一行的 "1"(块编号)会变成 "\u{feff}1",`parse::<u64>()` 失败,第一个字幕
    // 块被整个跳过。真实踩过:带 BOM 的两块 srt 只解析出 1 块。
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim().parse::<u64>().is_ok() {
            let ts_line_idx = i + 1;
            if ts_line_idx < lines.len() && lines[ts_line_idx].contains("-->") {
                let parts: Vec<&str> = lines[ts_line_idx].splitn(2, "-->").collect();
                let start = parts[0].trim().to_string();
                let end = parts.get(1).map(|s| s.trim().to_string()).unwrap_or_default();
                let mut k = ts_line_idx + 1;
                let mut text_lines = Vec::new();
                while k < lines.len() && !lines[k].trim().is_empty() {
                    text_lines.push(lines[k].to_string());
                    k += 1;
                }
                out.push((start, end, text_lines));
                i = k;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// 极简 SRT 解析:按空行分块,每块取"起止时间戳"+"文本行拼接"。
/// 不追求处理所有 SRT 边缘格式,够用来做重复检测/折叠即可。
/// pub(crate) 因为 translate.rs 复用它来读取原文 srt、按块喂给翻译模型。
pub(crate) fn parse_srt_blocks(content: &str) -> Vec<(String, String, String)> {
    parse_srt_blocks_lines(content)
        .into_iter()
        .map(|(start, end, lines)| (start, end, lines.join(" ")))
        .collect()
}

pub(crate) fn write_srt_blocks(blocks: &[(String, String, String)]) -> String {
    let mut out = String::new();
    for (idx, (start, end, text)) in blocks.iter().enumerate() {
        out.push_str(&format!("{}\n{start} --> {end}\n{text}\n\n", idx + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 复现真实出现过的"复读机"故障形状(同一句话连续出现 983 次)并验证折叠逻辑:
    /// 折叠后应该只剩 1 条(跨度覆盖首尾时间戳),且不影响前后正常的字幕行。
    /// 用程序合成数据而不是依赖外部真实文件——之前用外部文件做过一次测试,结果那份文件
    /// 后来被 `collapse_repetitions` 本身(在用户的正常使用中)原地改过了,导致测试
    /// 因外部状态漂移而失败,这是测试设计的问题,换成自包含的合成数据即可避免。
    #[test]
    fn collapses_real_repetition_failure() {
        let repeated_text = "I'm going to try and get it.";
        let mut srt = String::new();
        srt.push_str("1\n00:00:01,000 --> 00:00:02,000\nSee if this one stays on the line.\n\n");
        for i in 0..983 {
            let start_ms = 2_000 + i * 2_000;
            let end_ms = start_ms + 2_000;
            srt.push_str(&format!(
                "{}\n{}--> {}\n{repeated_text}\n\n",
                i + 2,
                format_ms(start_ms),
                format_ms(end_ms)
            ));
        }
        srt.push_str(&format!(
            "985\n{}--> {}\nToo fast for me to follow.\n\n",
            format_ms(2_000 + 983 * 2_000),
            format_ms(2_000 + 984 * 2_000)
        ));

        let tmp = std::env::temp_dir().join("subgen_test_repetition.srt");
        std::fs::write(&tmp, &srt).expect("写入测试文件失败");

        let before = parse_srt_blocks(&std::fs::read_to_string(&tmp).unwrap());
        let repeated_count = before
            .iter()
            .filter(|(_, _, t)| t.trim() == repeated_text)
            .count();
        assert_eq!(repeated_count, 983, "构造的测试数据应该有 983 条重复");

        let repetition_at = collapse_repetitions(&tmp).expect("折叠失败");
        assert_eq!(
            repetition_at.as_deref(),
            Some("00:00:02,000"),
            "应该检测到复读并返回首个折叠片段的起始时间戳"
        );

        let after = parse_srt_blocks(&std::fs::read_to_string(&tmp).unwrap());
        let after_repeated_count = after
            .iter()
            .filter(|(_, _, t)| t.trim() == repeated_text)
            .count();
        assert_eq!(after_repeated_count, 1, "折叠后应该只剩 1 条,而不是几百条");
        assert_eq!(after.len(), 3, "折叠后应该是:正常行 + 折叠后的 1 条 + 正常行,共 3 条");

        let _ = std::fs::remove_file(&tmp);
    }

    fn format_ms(total_ms: u64) -> String {
        let ms = total_ms % 1000;
        let total_s = total_ms / 1000;
        let s = total_s % 60;
        let total_m = total_s / 60;
        let m = total_m % 60;
        let h = total_m / 60;
        format!("{h:02}:{m:02}:{s:02},{ms:03} ")
    }

    /// 用真实遗留数据回归测试:这份文件是本机真实出现过的"主程序崩溃、whisper-cli 孤儿
    /// 进程自己跑完"场景留下的产物(dist/tmp 里的 Deadliest.Catch.S22E05...srt),不是构造的
    /// 理想样例——断点续跑机制要正确识别出这种真实场景下的遗留文件是完整可用的,才谈得上
    /// "复用"而不是把它当垃圾重新识别一遍。
    #[test]
    fn srt_looks_complete_accepts_real_leftover_file_from_a_crashed_run() {
        let content = "1\n00:00:00,000 --> 00:00:08,100\n ♪♪\n\n\
                        2\n00:00:08,100 --> 00:00:10,880\n Last night, I had a dream.\n\n\
                        3\n00:00:10,880 --> 00:00:13,640\n ♪♪\n\n\
                        4\n00:00:13,640 --> 00:00:16,140\n I was underwater, breathing underwater.\n\n\
                        5\n00:00:16,140 --> 00:00:17,260\n I don't know how I did that.\n\n";
        let path = std::env::temp_dir().join("subgen_test_real_leftover.srt");
        std::fs::write(&path, content).unwrap();
        assert!(
            srt_looks_complete(&path),
            "真实崩溃场景遗留下来的完整 srt 应该被识别为可复用"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn srt_looks_complete_accepts_empty_and_parseable_content() {
        let dir = std::env::temp_dir().join("subgen_test_srt_complete");
        std::fs::create_dir_all(&dir).unwrap();

        let empty = dir.join("empty.srt");
        std::fs::write(&empty, "").unwrap();
        assert!(srt_looks_complete(&empty), "空字幕是合理结果,应该算完整");

        let valid = dir.join("valid.srt");
        std::fs::write(&valid, "1\n00:00:01,000 --> 00:00:02,000\nHello.\n\n").unwrap();
        assert!(srt_looks_complete(&valid), "能解析出块的应该算完整");

        let garbage = dir.join("garbage.srt");
        std::fs::write(&garbage, "not an srt file at all, just some junk text").unwrap();
        assert!(!srt_looks_complete(&garbage), "解析不出任何块的垃圾内容不该算完整");

        let missing = dir.join("does_not_exist.srt");
        assert!(!srt_looks_complete(&missing), "文件不存在不该算完整");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn does_not_touch_normal_short_repeats() {
        // "Yeah, yeah, yeah." 这种真实内容里的自然重复(单条字幕内,不是跨条目连续重复)不该被误伤
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nHello there.\n\n\
                   2\n00:00:02,000 --> 00:00:03,000\nYeah, yeah, yeah, yeah.\n\n\
                   3\n00:00:03,000 --> 00:00:04,000\nGoodbye now.\n\n";
        let tmp = std::env::temp_dir().join("subgen_test_normal.srt");
        std::fs::write(&tmp, srt).unwrap();
        let repetition_at = collapse_repetitions(&tmp).unwrap();
        assert_eq!(repetition_at, None, "正常内容不应该被判定为复读故障");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn parse_srt_blocks_strips_utf8_bom_on_first_block() {
        // Windows 记事本"另存为 UTF-8"会在文件头写 BOM,不清掉会让第一块编号 "\u{feff}1"
        // 解析失败、第一个字幕块被整个跳过。两块的字幕只解析出 1 块是真实踩过的现象。
        let mut content = String::from('\u{feff}');
        content.push_str("1\n00:00:01,000 --> 00:00:02,000\nHello\n\n");
        content.push_str("2\n00:00:02,000 --> 00:00:03,000\nWorld\n\n");
        let blocks = parse_srt_blocks(&content);
        assert_eq!(
            blocks.len(),
            2,
            "带 BOM 的两块字幕应该完整解析出 2 块,而不是第一块被跳过: {blocks:?}"
        );
        assert_eq!(blocks[0].2, "Hello");
        assert_eq!(blocks[1].2, "World");
    }

    #[test]
    fn parse_srt_blocks_lines_keeps_bilingual_lines_separate() {
        // 双语字幕每块是"中文\n原文"两行(见 translate::build_output_blocks),
        // parse_srt_blocks 会把它们拼成 "中文 原文" 一坨;parse_srt_blocks_lines 不该这样,
        // 报告/回灌功能需要能单独拿到第一行(中文)。
        let content = "1\n00:00:01,000 --> 00:00:02,000\n你好。\nHello there.\n\n";
        let blocks = parse_srt_blocks_lines(content);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].2, vec!["你好。".to_string(), "Hello there.".to_string()]);

        // parse_srt_blocks 本身的拼接行为必须逐字节保持不变(既有测试/调用方都依赖这个)。
        let joined = parse_srt_blocks(content);
        assert_eq!(joined[0].2, "你好。 Hello there.");
    }

    #[test]
    fn count_subtitle_blocks_counts_parseable_blocks_and_defaults_to_zero_when_missing() {
        let dir = std::env::temp_dir().join("subgen_test_count_blocks");
        std::fs::create_dir_all(&dir).unwrap();

        let srt = dir.join("three.srt");
        std::fs::write(
            &srt,
            "1\n00:00:01,000 --> 00:00:02,000\nHello.\n\n\
             2\n00:00:02,000 --> 00:00:03,000\nWorld.\n\n\
             3\n00:00:03,000 --> 00:00:04,000\nBye.\n\n",
        )
        .unwrap();
        assert_eq!(count_subtitle_blocks(&srt), 3);

        let missing = dir.join("does_not_exist.srt");
        assert_eq!(count_subtitle_blocks(&missing), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
