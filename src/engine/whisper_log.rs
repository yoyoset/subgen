use std::path::Path;


/// 断点续跑复用已有 srt、跳过重新识别时,这次没有真实跑 whisper-cli,拿不到"auto-detected
/// language"这行日志——尝试从同一个哈希对应的旧 `.log` 文件(如果存在,比如上一次是完整跑完
/// 才留下的)里找。找不到就是 None,退化成 "auto",翻译阶段会落到兜底的通用模型,不算错误。
pub(super) fn recover_detected_language_from_log(log_file: &Path) -> Option<String> {
    let content = std::fs::read_to_string(log_file).ok()?;
    content.lines().find_map(parse_detected_language)
}

/// 解析 whisper.cpp 的语言自动检测日志行,例如:
/// "whisper_full_with_state: auto-detected language: ja (p = 0.987654)"
/// 返回 ISO 639-1 语言代码(如 "ja"),解析失败返回 None。
pub(super) fn parse_detected_language(line: &str) -> Option<String> {
    let idx = line.find("auto-detected language:")?;
    let rest = &line[idx + "auto-detected language:".len()..];
    let code: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if code.is_empty() {
        None
    } else {
        Some(code)
    }
}


/// 把子进程退出状态格式化成人能看懂、也方便拿去搜索的形式。十进制退出码之外额外给一个
/// 十六进制形式(按 u32 重新解释,不是简单地给负数加个 0x 前缀)——Windows 的已知状态码
/// 习惯用十六进制表示(比如 STATUS_ACCESS_VIOLATION 是 0xc0000005),十进制形式基本没人
/// 认得出来,不转成十六进制的话,记了退出码也等于没记。
pub(super) fn describe_exit_status(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("退出码: {code}(十六进制 0x{:08x})", code as u32),
        None => "退出码: 无法获取(进程可能是被终止的,不是自己正常退出)".to_string(),
    }
}

pub(super) fn parse_progress_percent(line: &str) -> Option<f32> {
    // "whisper_print_progress_callback: progress = 42%"
    let idx = line.find("progress = ")?;
    let rest = &line[idx + "progress = ".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用真实拉起的子进程验证 describe_exit_status,不是构造一个假的 ExitStatus——
    /// 标准库在稳定版 Rust 里没有给 ExitStatus 提供任意退出码的公开构造方式,拉一个
    /// `cmd /C exit N` 真实进程是最直接、最贴近实际用法的验证方式。
    #[test]
    fn describe_exit_status_reports_decimal_and_hex_for_crash_style_code() {
        // -1073741819 是 STATUS_ACCESS_VIOLATION(0xC0000005)以 i32 表示的值,
        // 是一种真实会在 Windows 上遇到的崩溃退出码,不是随便挑的数字。
        let status = std::process::Command::new("cmd")
            .args(["/C", "exit -1073741819"])
            .status()
            .expect("无法拉起 cmd 做测试");
        let desc = describe_exit_status(&status);
        assert!(
            desc.contains("-1073741819"),
            "应该包含十进制退出码: {desc}"
        );
        assert!(
            desc.contains("0xc0000005"),
            "应该包含对应的十六进制形式,方便对照已知的 Windows 状态码: {desc}"
        );
    }

    #[test]
    fn describe_exit_status_reports_zero_for_success() {
        let status = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .status()
            .expect("无法拉起 cmd 做测试");
        let desc = describe_exit_status(&status);
        assert!(desc.contains("退出码: 0"), "成功退出应该显示退出码 0: {desc}");
        assert!(desc.contains("0x00000000"), "十六进制形式也应该是全 0: {desc}");
    }

    #[test]
    fn recovers_detected_language_from_old_log_file() {
        let log = std::env::temp_dir().join("subgen_test_recover_lang.log");
        std::fs::write(
            &log,
            "whisper_model_load: some other line\n\
             whisper_full_with_state: auto-detected language: ja (p = 0.987654)\n\
             more lines after\n",
        )
        .unwrap();
        assert_eq!(
            recover_detected_language_from_log(&log).as_deref(),
            Some("ja")
        );
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn recover_detected_language_returns_none_when_log_missing_or_lacks_the_line() {
        let missing = std::env::temp_dir().join("subgen_test_recover_lang_missing.log");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(recover_detected_language_from_log(&missing), None);

        let no_lang_line = std::env::temp_dir().join("subgen_test_recover_lang_no_line.log");
        std::fs::write(&no_lang_line, "just some unrelated log content\n").unwrap();
        assert_eq!(recover_detected_language_from_log(&no_lang_line), None);
        let _ = std::fs::remove_file(&no_lang_line);
    }

    #[test]
    fn parses_detected_language_from_whisper_log_line() {
        let line = "whisper_full_with_state: auto-detected language: ja (p = 0.987654)";
        assert_eq!(parse_detected_language(line).as_deref(), Some("ja"));
    }

    #[test]
    fn detected_language_parse_ignores_unrelated_lines() {
        let line = "whisper_print_progress_callback: progress = 42%";
        assert_eq!(parse_detected_language(line), None);
    }
}
