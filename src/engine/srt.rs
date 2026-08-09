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

/// 字幕折行的显示宽度上限(列)。CJK/全角字符计 2 列、其余计 1 列,所以 42 列 ≈ 中文 21 字。
/// 数值来自真实数据分布(`E:\season 11` 四集 3464 个块:中文 p90=20 字、英文 p90=59 字符),
/// 配合"允许超 2 行"的兜底能装下绝大多数块;0 表示关闭折行(见 `wrap_subtitle_text`)。
pub(crate) const DEFAULT_SUBTITLE_MAX_WIDTH: usize = 42;

/// 折行的断点优先级:空格 > 在这些标点之后 > 硬断。空格断点覆盖英文(词边界);中文没有
/// 空格,只能靠这些标点之后断开。
const WRAP_PUNCT: &[char] = &['，', '。', '、', '；', '：', '！', '？', ',', '.', '!', '?'];

/// 字符的显示宽度:CJK/全角字符算 2 列,其余算 1 列。
/// 覆盖的"算 2 列"码点区间:
/// - U+1100–U+115F 谚文声母
/// - U+2E80–U+303F CJK 部首 / 康熙部首 / CJK 符号标点
/// - U+3041–U+33FF 平假名 / 片假名 / 注音 / CJK 笔画等
/// - U+3400–U+4DBF、U+4E00–U+9FFF CJK 统一表意文字(扩展 A / 基本区)
/// - U+A000–U+A4CF 彝文
/// - U+F900–U+FAFF CJK 兼容表意文字
/// - U+FE30–U+FE4F CJK 兼容形式
/// - U+FF00–U+FF60 全角形式
///
/// 明确按 1 列算、没覆盖的:组合附加符(U+0300 区)、零宽字符(U+200B 等)——字幕文本里
/// 几乎不出现;就算出现按 1 列算也只是可能比实际渲染少 1 列,不会造成超宽。
pub(crate) fn char_width(c: char) -> usize {
    let cp = c as u32;
    if matches!(cp,
        0x1100..=0x115F
        | 0x2E80..=0x303F
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
    ) {
        2
    } else {
        1
    }
}

/// 整段文本的显示宽度(所有字符宽度之和)。
pub(crate) fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 把整块字幕文本按显示宽度折行。**只插入 `\n`,不增删改任何非空白字符**——把结果和
/// 输入的所有 `\n`/空格归一化后必须逐字符一致(有单测锁死)。`\n` 分隔的每一行独立折:
/// 双语块是 `中文\n原文` 两行一块(见 `translate::build_output_blocks`),两种语言不能混
/// 在一起重排,否则行边界就丢了。
/// 每行首尾空白先 trim 掉:whisper 输出的 srt 文本行真实样本里带前导空格(如
/// `" My name is..."`),不 trim 的话空格占的 1 列会把可用宽度吃掉、诱发不必要的折行。
/// `max_width == 0` 表示关闭折行,原样返回。
pub(crate) fn wrap_subtitle_text(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return text.to_string();
    }
    let mut out_lines = Vec::new();
    for line in text.split('\n') {
        out_lines.extend(wrap_line(line, max_width));
    }
    out_lines.join("\n")
}

/// 单行折行,返回折好的若干行(不含内部 `\n`)。行首尾空白被 trim 掉;整行纯空白返回空。
fn wrap_line(line: &str, max_width: usize) -> Vec<String> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    if display_width(line) <= max_width {
        return vec![line.to_string()];
    }
    let tokens: Vec<&str> = line.split(' ').filter(|t| !t.is_empty()).collect();
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for token in tokens {
        let token_width = display_width(token);
        if token_width <= max_width {
            // 单词本身装得下:优先塞进当前行(空格是最高优先级断点),塞不下才另起一行。
            let sep = if current.is_empty() { 0 } else { 1 };
            if current_width + sep + token_width <= max_width {
                if sep > 0 {
                    current.push(' ');
                }
                current.push_str(token);
                current_width += sep + token_width;
            } else {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                current.push_str(token);
                current_width = token_width;
            }
        } else {
            // 单个词(中文整行没有空格,整行就相当于一个"词")长于上限:先结清当前行,
            // 再把这个词按"标点之后 > 硬断"切成若干片。最后一片留在 current 里,好让
            // 紧随的词有机会并上来(词间补回空格,内容不丢)。
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            let pieces = break_long_token(token, max_width);
            let last = pieces.len() - 1;
            for (i, piece) in pieces.into_iter().enumerate() {
                if i < last {
                    out.push(piece);
                } else {
                    current = piece;
                    current_width = display_width(&current);
                }
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// 把一个不含空格的长词(通常是整行中文)切成宽度不超过 `max_width` 的若干片,每片拼回
/// 来等于原词(不丢字)。断点优先"标点之后":在装得下的范围内找最后一个可断的标点,切在
/// 它后面;找不到标点就在装得下的最远处硬切——硬切只对"单个词本身长于上限"的极少数情况
/// 生效(比如一个超长的单词),正常内容的词都装得下,不会从词中间断。
/// 标点断点要求切在 start+2 之后(即每片至少 2 个字符),避免把紧贴上一片开头、本该跟着
/// 上一片走的标点单独切出来成一行孤标点。
fn break_long_token(token: &str, max_width: usize) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    let mut pieces = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        // 从 start 起最多能装到哪(严格 <= max_width)。
        let mut w = 0usize;
        let mut max_end = start;
        while max_end < chars.len() && w + char_width(chars[max_end]) <= max_width {
            w += char_width(chars[max_end]);
            max_end += 1;
        }
        if max_end == start {
            // 单个字符就超上限(max_width >= 2 时理论上不会发生),强制推进避免死循环。
            max_end = start + 1;
        }
        // 还没到末尾才需要找标点断点;已经装完整个 token 就直接收尾,不必再切。
        if max_end < chars.len() {
            let mut punct_end = None;
            for i in (start + 2)..max_end {
                if WRAP_PUNCT.contains(&chars[i - 1]) {
                    punct_end = Some(i);
                }
            }
            if let Some(p) = punct_end {
                max_end = p;
            }
        }
        pieces.push(chars[start..max_end].iter().collect());
        start = max_end;
    }
    pieces
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

    /// 折行的核心不变量:结果和输入把所有 `\n`/空格归一化后必须逐字符一致——折行只插入
    /// 换行、不增删改任何非空白字符。纯英文超宽/纯中文超宽/双语/含前导空格/需要 3 行/
    /// 单词超长这些形态都跑一遍,任何一条违反都说明折行丢字了。
    #[test]
    fn wrap_preserves_characters_after_normalizing_whitespace() {
        let cases = [
            "This is a fairly long English sentence that will wrap onto several lines.",
            "我们见过达姆斯克图案从最简单到最复杂",
            "我们见过达姆斯克图案，从最简单到最复杂。\nThis is a fairly long English sentence that will wrap.",
            " My name is Bob.",
            "短",
            "onewordthatisverylongindeed beyond the limit",
            "",
        ];
        for text in cases {
            let out = wrap_subtitle_text(text, 20);
            let norm =
                |s: &str| s.chars().filter(|c| *c != '\n' && *c != ' ').collect::<String>();
            assert_eq!(
                norm(&out),
                norm(text),
                "折行改变了非空白字符: input={text:?} out={out:?}"
            );
        }
    }

    /// 折行后每一显示行都不超过上限(除了"单个字符本身超上限"这种理论上不会出现的病理)。
    /// 上限默认 42 列,这里用 20 列把所有折行路径都触发一遍。
    #[test]
    fn wrap_never_exceeds_max_width_per_line() {
        let cases = [
            "This is a fairly long English sentence that will wrap onto several lines.",
            "我们见过达姆斯克图案，从最简单到最复杂。",
            "onewordthatisverylongindeed beyond the limit",
            "我们见过达姆斯克图案从最简单到最复杂\nThis is a fairly long English sentence that will wrap.",
        ];
        for text in cases {
            let out = wrap_subtitle_text(text, 20);
            for line in out.lines() {
                assert!(
                    display_width(line) <= 20,
                    "折行后出现超宽行: {line:?} (out={out:?})"
                );
            }
        }
    }

    /// 恰好等于上限的文本原样返回,不折、不改。42 列 = 中文 21 字。
    #[test]
    fn wrap_leaves_text_at_limit_unchanged() {
        let exactly_21_zh = "一二三四五六七八九十一二三四五六七八九十一";
        assert_eq!(
            char_count(exactly_21_zh),
            21,
            "测试数据应该有 21 个汉字(42 列)"
        );
        assert_eq!(
            wrap_subtitle_text(exactly_21_zh, DEFAULT_SUBTITLE_MAX_WIDTH),
            exactly_21_zh,
            "恰好 42 列不该被折行"
        );
        // 多 1 个字就该折成两行,且每行都不超 21 字。
        let over_21_zh = format!("{exactly_21_zh}二");
        let out = wrap_subtitle_text(&over_21_zh, DEFAULT_SUBTITLE_MAX_WIDTH);
        assert!(out.contains('\n'), "超过上限必须折行: {out:?}");
        for line in out.lines() {
            assert!(display_width(line) <= DEFAULT_SUBTITLE_MAX_WIDTH);
        }
    }

    /// whisper 输出的 srt 文本行带前导空格(真实样本: `" My name is..."`),折行前要
    /// trim 掉,否则空格占的 1 列会诱发不必要的折行;短文本也不该保留这个前导空格。
    #[test]
    fn wrap_trims_leading_and_trailing_whitespace_per_line() {
        assert_eq!(
            wrap_subtitle_text(" My name is Bob. ", 20),
            "My name is Bob."
        );
        // 双语块第一行(中文)也可能带空白,同样要 trim。
        assert_eq!(
            wrap_subtitle_text(" 我们见过的达姆斯克图案 \n Here in the forge, we've seen Damascus patterns",
                100),
            "我们见过的达姆斯克图案\nHere in the forge, we've seen Damascus patterns"
        );
    }

    /// 双语块是 `中文\n原文` 两行一块,折行后两种语言不能混进同一行——各自独立折,
    /// 语言边界保留。
    #[test]
    fn wrap_keeps_bilingual_language_lines_separate() {
        let zh = "一二三四五六七八九十一二三四五六七八九十一二三";
        let en = "This is a fairly long English sentence.";
        let text = format!("{zh}\n{en}");
        let out = wrap_subtitle_text(&text, 20);
        assert!(out.contains('\n'), "中英文各自都超了 20 列,必然要折行: {out:?}");
        for line in out.lines() {
            let has_cjk = line.chars().any(|c| char_width(c) == 2);
            let has_ascii = line.chars().any(|c| c.is_ascii());
            assert!(
                !(has_cjk && has_ascii),
                "中英文被混进同一行,语言边界丢了: {line:?} (out={out:?})"
            );
        }
    }

    /// 超长文本允许超过 2 行(绝不丢字),一直折到每行都装得下为止。
    #[test]
    fn wrap_allows_third_line_instead_of_dropping_text() {
        let en = "This is a fairly long English sentence that will wrap onto several lines.";
        let out = wrap_subtitle_text(en, 20);
        assert!(out.lines().count() >= 3, "这段英文在 20 列下应该需要 3 行以上: {out:?}");
        for line in out.lines() {
            assert!(display_width(line) <= 20);
        }
    }

    /// 单个词本身长于上限时的最后手段:硬断(从词中间切),前提是没有任何标点可断。
    /// 每片拼回 = 原词,不丢字。这是"英文不得从单词中间断开"唯一允许的例外。
    #[test]
    fn wrap_hard_breaks_overlong_single_word() {
        let out = wrap_subtitle_text("antidisestablishmentarianism", 10);
        assert_eq!(out, "antidisest\nablishment\narianism");
        let norm = |s: &str| s.chars().filter(|c| *c != '\n').collect::<String>();
        assert_eq!(norm(&out), "antidisestablishmentarianism");
    }

    /// 中文没有空格,超长整行按"标点之后"优先断,而不是硬切。
    #[test]
    fn wrap_breaks_chinese_after_punctuation_not_mid_sentence() {
        // 断点应落在"，"后面,而不是把词从中切开。
        let text = "我们见过的达姆斯克图案，从最简单到最复杂的各种形式。";
        let out = wrap_subtitle_text(text, 12);
        assert!(out.contains('\n'));
        // 若"标点之后"断生效,某一行应以"，"结尾;不该有任何行从中间把"最复杂"这种词切开
        // (这里没有空格,硬切会落在任意字符间,而标点断会优先)。
        assert!(
            out.lines().any(|l| l.ends_with('，') || l.ends_with('。')),
            "应该优先断在标点之后: {out:?}"
        );
    }

    fn char_count(s: &str) -> usize {
        s.chars().count()
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
