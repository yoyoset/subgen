//! 人话格式化小函数 + 日志落盘助手:把数字/状态/枚举值转成界面显示用的人话字符串,或对
//! 数据做简单的业务逻辑判断(比如"这条任务应不应该纳入这一批处理"),顺带负责应用日志
//! 的落盘与轮转(见 `push_log`/`rotate_log_file`)。
//! 这些函数不依赖 SubgenApp 的私有状态,搬家到独立文件不涉及可见性改动,也不涉及
//! 新的跨文件依赖(只依赖既有的 job/paths 等模块)。

use crate::job::{Job, JobStatus};
use crate::paths;
use crate::ui::theme;
use std::path::PathBuf;

/// 判断某个任务是否应该被这一次"开始处理"纳入批次:还没到成功终态的任务本来就该跑
/// (失败/跳过/取消都值得重试),或者用户在任务行显式点了"重新处理"——即使已经是
/// Done/DoneWithWarning 终态也要纳入,并且 job::run_batch 会用 force 标志让它真正
/// 重新跑一遍,不是复用旧结果。
pub fn should_include_in_batch(job: &Job) -> bool {
    job.force_reprocess
        || !matches!(job.status, JobStatus::Done(_) | JobStatus::DoneWithWarning(..))
}

/// "打开文件夹"要选中的文件:完成态(不管有没有警告)优先选中生成的字幕——双语/仅中文
/// 场景下是 `Video.zh.srt`,没有翻译则是同名 `Video.srt`;其它状态(还没跑完/失败/跳过)
/// 没有稳定可用的产物,选中视频本身本来就是用户想找的东西。
pub fn reveal_path_for(job: &Job) -> PathBuf {
    if matches!(job.status, JobStatus::Done(_) | JobStatus::DoneWithWarning(..)) {
        let zh = paths::sidecar_srt_lang_path(&job.video_path, "zh");
        if zh.is_file() {
            return zh;
        }
        let srt = paths::sidecar_srt_path(&job.video_path);
        if srt.is_file() {
            return srt;
        }
    }
    job.video_path.clone()
}

/// 单个日志文件超过这个体积后,下次写入前把它改名成 `{name}.1`(丢弃更早的 .1),让日志
/// 体积长期有界——`logs/app.log` 和 `logs/crash.log` 都是只增不删的追加写,时间长了会无限
/// 膨胀(一台长期用的机器上攒几个 GB 的日志很常见)。
pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// 判断日志文件是否已经超过体积上限、需要轮转。纯函数,方便单测。
pub fn should_rotate_log(current_len: u64, max_bytes: u64) -> bool {
    current_len > max_bytes
}

/// 把超过体积上限的日志文件轮转:改名成 `{name}.1`,腾出位置让新日志从头写。
/// 改名失败时静默忽略——日志只是附加的排障能力,轮转失败不该成为新的失败点。
pub fn rotate_log_file(path: &std::path::Path, max_bytes: u64) {
    if std::fs::metadata(path)
        .map(|m| should_rotate_log(m.len(), max_bytes))
        .unwrap_or(false)
    {
        let rotated = path.with_extension("log.1");
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(path, &rotated);
    }
}

/// 追加一行日志:既存进界面日志区(`log_lines`),也同步落盘到 `logs/app.log`——`log_lines`
/// 只在内存里,关窗即丢,用户报障时"把 logs 文件夹发我"这句话要成立,日志就得先落盘。
/// 时间戳格式和 crash.log(见 install_panic_logger)保持一致,用 unix 时间戳而不是拼日历
/// 时间,没有额外引入日期时间处理依赖。写盘失败(比如目录还没建出来)静默忽略,这只是个
/// 附加的排障能力,不该反过来影响主流程。
///
/// 写成自由函数、只接收 `&mut Vec<String>` 而不是 `&mut SubgenApp` 方法——调用方常常是
/// 先 `if let Some(rx) = &self.xxx_rx { while ... }` 借着 `rx` 读事件、循环体里再要记日志,
/// 如果写成 `&mut self` 方法,会和外层对 `self.xxx_rx` 的借用冲突(借用检查器认为方法调用
/// 需要整个 `self` 的独占访问权);只借 `log_lines` 这一个字段就没有这个问题。
pub fn push_log(log_lines: &mut Vec<String>, line: String) {
    let _ = std::fs::create_dir_all(paths::logs_dir());
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let log_path = paths::logs_dir().join("app.log");
    rotate_log_file(&log_path, MAX_LOG_BYTES);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        use std::io::Write;
        let _ = writeln!(file, "[unix {timestamp}] {line}");
    }
    log_lines.push(line);
}

/// 把秒数格式化成 "12秒" / "1分23秒" / "2小时5分" 这种人可读的形式,用于界面上的"已运行"
/// 计时。整小时以上的批量任务(几十集可能要跑一两个小时)显示"XX分YY秒"会失去规模感,
/// 所以 ≥1 小时时切换成小时制。
pub fn format_elapsed(total_secs: u64) -> String {
    let s = total_secs % 60;
    let total_min = total_secs / 60;
    let m = total_min % 60;
    let h = total_min / 60;
    if h > 0 {
        format!("{h}小时{m}分")
    } else if total_secs >= 60 {
        format!("{m}分{s}秒")
    } else {
        format!("{s}秒")
    }
}

/// 批次结束时拼一句汇总,比如"本批完成:17 个成功、2 个建议核对、1 个失败(共用时
/// 23分14秒)"。之前批次跑完只能靠肉眼扫一遍任务列表数有几个失败/警告,20 集里混着看很
/// 容易漏掉;现在结束时直接给一个数字上的总结。只统计 `batch_indices` 里的任务(这一批
/// 真正处理过的),不是整个任务列表——列表里可能还有上一批已经成功、这一批没碰过的文件。
pub fn build_batch_summary(jobs: &[Job], batch_indices: &[usize], elapsed: std::time::Duration) -> String {
    let mut done = 0u32;
    let mut warned = 0u32;
    let mut failed = 0u32;
    let mut skipped = 0u32;
    let mut cancelled = 0u32;
    for &i in batch_indices {
        if let Some(status) = jobs.get(i).map(|j| &j.status) {
            match status {
                JobStatus::Done(_) => done += 1,
                JobStatus::DoneWithWarning(..) => warned += 1,
                JobStatus::Failed(..) => failed += 1,
                JobStatus::Skipped(_) => skipped += 1,
                JobStatus::Cancelled => cancelled += 1,
                JobStatus::Pending | JobStatus::Running(..) => {}
            }
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if done > 0 {
        parts.push(format!("{done} 个成功"));
    }
    if warned > 0 {
        parts.push(format!("{warned} 个建议核对"));
    }
    if failed > 0 {
        parts.push(format!("{failed} 个失败"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} 个跳过"));
    }
    if cancelled > 0 {
        parts.push(format!("{cancelled} 个已取消"));
    }
    let counts = if parts.is_empty() {
        "没有任务被处理".to_string()
    } else {
        parts.join("、")
    };

    let mut summary = format!("本批完成:{counts}(共用时 {})", format_elapsed(elapsed.as_secs()));
    // 有需要用户看一眼的项时,顺手提示一句去哪找——不重排列表顺序(用户对列表顺序有
    // 预期,批次跑完突然打乱反而更难找),颜色区分现有机制已经覆盖,只是加一句指引。
    if warned > 0 || failed > 0 {
        summary.push_str("(建议核对与失败的项已在列表中用颜色标出)");
    }
    summary
}

/// 把字节数格式化成下载页面看着顺眼的大小单位:≥1GB 显示到 GB(1 位小数),否则显示到
/// MB(1 位小数)。只用于下载组件大小提示,不追求通用场景。
pub fn format_download_size(bytes: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else {
        format!("{:.1} MB", bytes / MB)
    }
}

/// 模型清单界面(`ui/model_manifest.rs`)每行的状态文案+颜色,和 `job_status_line` 是同一种
/// "人话 + 颜色"配对风格。`Ready`/`SizeMismatch` 都带上大小信息(复用 `format_download_size`),
/// 让用户不用另外去文件管理器里看属性就知道这个文件对不对。
pub fn model_status_line(status: &crate::models::ModelStatus) -> (String, eframe::egui::Color32) {
    use crate::models::ModelStatus;
    match status {
        ModelStatus::Ready { size, .. } => (
            format!("已就绪 · {}", format_download_size(*size)),
            theme::SUCCESS,
        ),
        ModelStatus::Missing => ("未安装".to_string(), theme::NEUTRAL),
        ModelStatus::PathBroken { .. } => (
            "绑定的文件已不在原位置(可能被移动或删除了)".to_string(),
            theme::WARN,
        ),
        ModelStatus::SizeMismatch { expected, actual, .. } => (
            format!(
                "文件大小异常(期望 {},实际 {}),可能下载不完整",
                format_download_size(*expected),
                format_download_size(*actual)
            ),
            theme::ERROR,
        ),
    }
}

/// 把 ISO 639-1 语言代码翻成用户能看懂的中文名,覆盖 `ui/settings.rs` 语言下拉框的选项列表
/// (auto/en/zh/ja/ko/fr/de/es)。没覆盖到的代码原样返回而不是显示"未知"——保留原始信息
/// 总比丢掉信息更安全,万一以后 whisper 检测出一个列表之外的语言代码,用户至少还能看到
/// 那串代码去查。
pub fn lang_display(code: &str) -> &str {
    match code {
        "auto" => "自动检测",
        "en" => "英语",
        "zh" => "中文",
        "ja" => "日语",
        "ko" => "韩语",
        "fr" => "法语",
        "de" => "德语",
        "es" => "西班牙语",
        other => other,
    }
}

/// 把一次成功处理的 JobReport 拼成"完成"后面跟着的补充信息,比如
/// " · 日语 · Sakura 7B(日→中,轻小说风格)(复用了上次的识别结果)"。让用户在结果不理想时
/// 至少知道用的是哪个语言、哪个翻译模型、这次是不是复用了旧结果——不用被动接受一个孤零零
/// 的"完成"、猜不出问题出在哪个环节。
pub fn format_job_report_suffix(report: &crate::job::JobReport) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(lang) = &report.detected_language {
        parts.push(lang_display(lang));
    }
    if let Some(model) = report.translate_model_label {
        parts.push(model);
    }
    let mut suffix = if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
    };
    if report.reused_transcript {
        suffix.push_str("(复用了上次的识别结果)");
    }
    suffix
}

/// 生成任务行第二行的详细状态文案 + 配色。`cancelling` 为 true 时(用户点了取消、批次还没
/// 真正停下来),正在跑的那一条任务额外附一句"正在取消,等当前步骤结束"——不然用户点完
/// 取消后看到这一行还在正常显示进度百分比,会以为取消没生效。
pub fn job_status_line(job: &Job, cancelling: bool) -> (String, eframe::egui::Color32) {
    match &job.status {
        JobStatus::Pending => ("等待中".to_string(), theme::NEUTRAL),
        JobStatus::Running(_, text, pct) => {
            let elapsed = job.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            let elapsed_str = format_elapsed(elapsed);
            // 百分比太小或刚开始跑时,线性外推出来的剩余时间波动极大(比如 1% 时算出来的
            // "约剩" 可能离谱到几十分钟),没有参考价值,只在有一定进度基础后才显示。
            let mut text = match pct {
                Some(pct) if *pct >= 5.0 && elapsed >= 10 => {
                    let eta_secs = (elapsed as f32 * (100.0 - pct) / pct).round() as u64;
                    format!(
                        "{text} {pct:.0}%(已运行 {elapsed_str},约剩 {})",
                        format_elapsed(eta_secs)
                    )
                }
                Some(pct) => format!("{text} {pct:.0}%(已运行 {elapsed_str})"),
                None => format!("{text}(已运行 {elapsed_str})"),
            };
            if cancelling {
                text.push_str("(正在取消,等当前步骤结束)");
            }
            (text, eframe::egui::Color32::LIGHT_GRAY)
        }
        JobStatus::Done(report) => {
            if report.empty_subtitle() {
                // 空字幕是合理结果(片段确实没有对白),不是软件坏了——但如果只显示"完成"
                // 两个字,用户在播放器里加载不出字幕文件时,唯一的结论会是"这个软件坏了"。
                (
                    "完成(未检测到对白,字幕为空)".to_string(),
                    theme::SUCCESS,
                )
            } else {
                (
                    format!("完成{}", format_job_report_suffix(report)),
                    theme::SUCCESS,
                )
            }
        }
        JobStatus::DoneWithWarning(reason, report) => (
            format!("完成,但有提示: {reason}{}", format_job_report_suffix(report)),
            theme::WARN,
        ),
        JobStatus::Skipped(reason) => (format!("跳过: {reason}"), theme::NEUTRAL),
        JobStatus::Failed(stage, reason, _) => (
            format!("{}阶段失败: {reason}", stage.label()),
            theme::ERROR,
        ),
        JobStatus::Cancelled => ("已取消".to_string(), theme::NEUTRAL),
    }
}

/// 队列里单行状态点的颜色,和 `job_status_line`/步骤条用的是同一套配色语义(绿=完成、
/// 橙=有提示、红=失败、灰=中性/终止态),Running 单独用步骤条的"进行中"蓝色而不是
/// `job_status_line` 文字用的浅灰——队列行缩成一行后,这个点是唯一能一眼分辨"正在跑的是
/// 哪一条"的视觉元素,不能和"等待中"用同一个灰。
pub fn status_dot_color(status: &JobStatus) -> eframe::egui::Color32 {
    match status {
        JobStatus::Pending => theme::NEUTRAL,
        JobStatus::Running(..) => theme::ACCENT_LIGHT,
        JobStatus::Done(report) if report.empty_subtitle() => theme::NEUTRAL,
        JobStatus::Done(_) => theme::SUCCESS,
        JobStatus::DoneWithWarning(..) => theme::WARN,
        JobStatus::Skipped(_) => theme::NEUTRAL,
        JobStatus::Failed(..) => theme::ERROR,
        JobStatus::Cancelled => theme::NEUTRAL,
    }
}

/// 队列缩成一行后,行内只留得下几个字的状态摘要(完整文案留给详情区的 `job_status_line`)。
pub fn compact_status_text(job: &Job) -> String {
    match &job.status {
        JobStatus::Pending => "等待中".to_string(),
        JobStatus::Running(stage, _, pct) => match pct {
            Some(pct) => format!("{} {pct:.0}%", stage.label()),
            None => stage.label().to_string(),
        },
        JobStatus::Done(report) if report.empty_subtitle() => "完成(字幕为空)".to_string(),
        JobStatus::Done(_) => "完成".to_string(),
        JobStatus::DoneWithWarning(..) => "完成 · 建议核对".to_string(),
        JobStatus::Skipped(_) => "已跳过".to_string(),
        JobStatus::Failed(stage, ..) => format!("{}失败", stage.label()),
        JobStatus::Cancelled => "已取消".to_string(),
    }
}

/// 当前正在处理的任务在 `jobs` 里的下标——串行单 worker,任何时刻至多一个 Running。
/// 详情区"自动跟随"就是跟着这个下标走;找不到(没有任务在跑)时返回 None。
pub fn active_job_index(jobs: &[Job]) -> Option<usize> {
    jobs.iter().position(|j| matches!(j.status, JobStatus::Running(..)))
}

/// 从 `jobs` 里移除下标 `removed` 之后,原来钉住的 `selected` 该指向哪:钉住的正好是被
/// 移除的那一行 → 回到 None(自动跟随);钉住的在被移除行之后 → 整体前移一位跟着补位;
/// 钉住的在之前 → 不受影响。`new_len` 兜底越界情况(防御性:理论上不会发生,但避免
/// 万一上游调用顺序错了导致后续 `jobs.get(selected)` panic)。
pub fn reconcile_selection(selected: Option<usize>, removed: usize, new_len: usize) -> Option<usize> {
    let adjusted = match selected {
        None => return None,
        Some(i) if i == removed => return None,
        Some(i) if i > removed => i - 1,
        Some(i) => i,
    };
    if adjusted < new_len {
        Some(adjusted)
    } else {
        None
    }
}

/// 线性外推批次剩余时间:用"已完成 done 个用了 elapsed"的平均速度估算剩下 (total-done)
/// 个还要多久。`done == 0` 时没有速度样本,不给出估算(强行外推第一个还没完成的样本
/// 波动极大,没有参考价值,和 `job_status_line` 里单文件 ETA 至少要 5% 进度才显示是
/// 同一个道理)。
pub fn estimate_batch_remaining(
    elapsed: std::time::Duration,
    done: usize,
    total: usize,
) -> Option<std::time::Duration> {
    if done == 0 || total <= done {
        return None;
    }
    let per_item = elapsed / done as u32;
    Some(per_item * (total - done) as u32)
}

/// 从 `jobs` + 这一批的原始下标列表算出"第几个/共几个":`total` 是批次大小,`done` 是
/// 已经到终态(不管成功/失败/跳过/取消)的数量。窗口标题(`app.rs::poll_worker`)和批次条
/// 都要用同一套计数,抽成一个纯函数,避免两处各写一遍、以后改了口径只改一处漏了另一处。
pub fn batch_progress_counts(jobs: &[Job], batch_indices: &[usize]) -> (usize, usize) {
    let total = batch_indices.len();
    let done = batch_indices
        .iter()
        .filter(|&&i| jobs.get(i).is_some_and(|j| j.status.is_terminal()))
        .count();
    (done, total)
}

/// 批次条上的一行进度文案,比如"第 7/20 个 · 预计剩 48 分钟 · 已用 31 分钟"。`done` 是
/// 已经到终态(不管成功/失败/跳过/取消)的数量,`current` 是"正在进行的是第几个"
/// (`done + 1`,封顶在 `total`)。
pub fn format_batch_progress_line(current: usize, total: usize, elapsed: std::time::Duration, done: usize) -> String {
    let mut line = format!("第 {current}/{total} 个 · 已用 {}", format_elapsed(elapsed.as_secs()));
    if let Some(remaining) = estimate_batch_remaining(elapsed, done, total) {
        line = format!(
            "第 {current}/{total} 个 · 预计剩 {} · 已用 {}",
            format_elapsed(remaining.as_secs()),
            format_elapsed(elapsed.as_secs())
        );
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_download_size_switches_between_mb_and_gb_at_one_gb() {
        assert_eq!(format_download_size(885_098), "0.8 MB");
        assert_eq!(format_download_size(1_073_741_824), "1.0 GB");
        assert_eq!(format_download_size(3_095_033_483), "2.9 GB");
    }

    #[test]
    fn format_elapsed_handles_seconds_minutes_and_hours() {
        assert_eq!(format_elapsed(0), "0秒");
        assert_eq!(format_elapsed(12), "12秒");
        assert_eq!(format_elapsed(60), "1分0秒");
        assert_eq!(format_elapsed(83), "1分23秒");
        // 一小时以上切成小时制:几十集的批次可能跑一两个小时,"60分5秒"没有规模感。
        assert_eq!(format_elapsed(3600), "1小时0分");
        assert_eq!(format_elapsed(7325), "2小时2分");
    }

    fn job_with_status(status: JobStatus) -> Job {
        Job {
            video_path: PathBuf::from("test.mkv"),
            status,
            started_at: None,
            force_reprocess: false,
            preview_open: false,
        }
    }

    #[test]
    fn should_include_in_batch_retries_non_success_states_without_force() {
        assert!(should_include_in_batch(&job_with_status(JobStatus::Pending)));
        assert!(should_include_in_batch(&job_with_status(JobStatus::Skipped(
            "s".to_string()
        ))));
        assert!(should_include_in_batch(&job_with_status(JobStatus::Failed(
            crate::job::Stage::Transcribe,
            "f".to_string(),
            None
        ))));
        assert!(should_include_in_batch(&job_with_status(JobStatus::Cancelled)));
    }

    #[test]
    fn should_include_in_batch_excludes_finished_success_states_unless_forced() {
        let done = job_with_status(JobStatus::Done(crate::job::JobReport::default()));
        assert!(
            !should_include_in_batch(&done),
            "普通重跑不该把已经成功的任务再跑一遍"
        );

        let mut forced_done = done;
        forced_done.force_reprocess = true;
        assert!(
            should_include_in_batch(&forced_done),
            "用户点了'重新处理'即使是 Done 状态也该纳入这一批"
        );

        let mut forced_warned =
            job_with_status(JobStatus::DoneWithWarning("w".to_string(), crate::job::JobReport::default()));
        forced_warned.force_reprocess = true;
        assert!(should_include_in_batch(&forced_warned));
    }

    #[test]
    fn reveal_path_for_prefers_generated_subtitle_when_done() {
        let dir = std::env::temp_dir().join(format!(
            "subgen_test_reveal_path_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let video_path = dir.join("Episode.mkv");
        std::fs::write(&video_path, b"fake video").unwrap();

        // 完成态但还没有任何字幕文件落地(理论上不该发生,但防御性地验证:找不到字幕就
        // 退回视频本身,而不是返回一个不存在的路径)。
        let mut job = job_with_status(JobStatus::Done(crate::job::JobReport::default()));
        job.video_path = video_path.clone();
        assert_eq!(reveal_path_for(&job), video_path);

        // 只有原文字幕(没开翻译的场景)
        let plain_srt = paths::sidecar_srt_path(&video_path);
        std::fs::write(&plain_srt, "1\n00:00:01,000 --> 00:00:02,000\nHello.\n\n").unwrap();
        assert_eq!(reveal_path_for(&job), plain_srt);

        // 有中文翻译字幕时优先选中它,而不是原文字幕
        let zh_srt = paths::sidecar_srt_lang_path(&video_path, "zh");
        std::fs::write(&zh_srt, "1\n00:00:01,000 --> 00:00:02,000\n你好。\n\n").unwrap();
        assert_eq!(reveal_path_for(&job), zh_srt);

        // 非完成态(比如 Failed)不管字幕文件在不在,都该选中视频本身
        let mut failed_job =
            job_with_status(JobStatus::Failed(crate::job::Stage::Transcribe, "f".to_string(), None));
        failed_job.video_path = video_path.clone();
        assert_eq!(reveal_path_for(&failed_job), video_path);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_batch_summary_counts_each_status_and_lists_them_in_order() {
        let jobs = vec![
            job_with_status(JobStatus::Done(crate::job::JobReport::default())),
            job_with_status(JobStatus::Done(crate::job::JobReport::default())),
            job_with_status(JobStatus::DoneWithWarning(
                "w".to_string(),
                crate::job::JobReport::default(),
            )),
            job_with_status(JobStatus::Failed(crate::job::Stage::Transcribe, "f".to_string(), None)),
            job_with_status(JobStatus::Skipped("s".to_string())),
        ];
        let batch_indices = vec![0, 1, 2, 3, 4];
        let summary =
            build_batch_summary(&jobs, &batch_indices, std::time::Duration::from_secs(74));
        assert_eq!(
            summary,
            "本批完成:2 个成功、1 个建议核对、1 个失败、1 个跳过(共用时 1分14秒)\
             (建议核对与失败的项已在列表中用颜色标出)"
        );
    }

    #[test]
    fn build_batch_summary_omits_zero_categories_and_hint_when_all_succeeded() {
        let jobs = vec![
            job_with_status(JobStatus::Done(crate::job::JobReport::default())),
            job_with_status(JobStatus::Done(crate::job::JobReport::default())),
        ];
        let batch_indices = vec![0, 1];
        let summary =
            build_batch_summary(&jobs, &batch_indices, std::time::Duration::from_secs(5));
        assert_eq!(summary, "本批完成:2 个成功(共用时 5秒)");
        assert!(!summary.contains("建议核对"), "全部成功时不该出现其它类别: {summary}");
        assert!(!summary.contains("已在列表中用颜色标出"), "全部成功时不该出现核对提示: {summary}");
    }

    #[test]
    fn build_batch_summary_only_counts_jobs_in_this_batch() {
        // 列表里可能还有上一批已经成功、这一批完全没碰过的文件,不该被算进这一批的汇总。
        let jobs = vec![
            job_with_status(JobStatus::Done(crate::job::JobReport::default())), // 上一批的,不在这次 batch_indices 里
            job_with_status(JobStatus::Failed(crate::job::Stage::ExtractAudio, "f".to_string(), None)),
        ];
        let batch_indices = vec![1];
        let summary =
            build_batch_summary(&jobs, &batch_indices, std::time::Duration::from_secs(1));
        assert_eq!(summary, "本批完成:1 个失败(共用时 1秒)(建议核对与失败的项已在列表中用颜色标出)");
    }

    fn running_job_with_elapsed(pct: Option<f32>, elapsed_secs: u64) -> Job {
        Job {
            video_path: PathBuf::from("test.mkv"),
            status: JobStatus::Running(crate::job::Stage::Transcribe, "正在识别语音...".to_string(), pct),
            started_at: Some(
                std::time::Instant::now() - std::time::Duration::from_secs(elapsed_secs),
            ),
            force_reprocess: false,
            preview_open: false,
        }
    }

    #[test]
    fn job_status_line_shows_eta_once_progress_and_elapsed_pass_thresholds() {
        // 60 秒跑了 25%,线性外推:剩余 75% 还要 60 * 75/25 = 180 秒 = 3 分 0 秒。
        let job = running_job_with_elapsed(Some(25.0), 60);
        let (text, _) = job_status_line(&job, false);
        assert!(text.contains("约剩 3分0秒"), "应该显示外推出的剩余时间: {text}");
    }

    #[test]
    fn job_status_line_hides_eta_when_progress_too_low() {
        // 1% 时线性外推出来的剩余时间波动极大,没有参考价值,不该显示。
        let job = running_job_with_elapsed(Some(1.0), 60);
        let (text, _) = job_status_line(&job, false);
        assert!(!text.contains("约剩"), "百分比太低时不该显示约剩: {text}");
    }

    #[test]
    fn job_status_line_hides_eta_when_just_started() {
        // 刚开始跑(已运行时间很短)时,即使百分比够高,外推同样不可靠。
        let job = running_job_with_elapsed(Some(50.0), 3);
        let (text, _) = job_status_line(&job, false);
        assert!(!text.contains("约剩"), "刚开始跑时不该显示约剩: {text}");
    }

    #[test]
    fn job_status_line_hides_eta_when_no_percent_available() {
        let job = running_job_with_elapsed(None, 60);
        let (text, _) = job_status_line(&job, false);
        assert!(!text.contains("约剩"), "没有百分比时无法外推,不该显示约剩: {text}");
    }

    #[test]
    fn job_status_line_appends_cancelling_suffix_when_cancelling() {
        // 用户点了"取消"但批次还没停(worker 要等当前步骤结束),状态行必须附一句
        // "正在取消",不然进度还在走会让用户以为取消没生效。
        let job = running_job_with_elapsed(Some(50.0), 30);
        let (text, _) = job_status_line(&job, true);
        assert!(
            text.contains("正在取消,等当前步骤结束"),
            "取消中应附加提示: {text}"
        );
        let (normal, _) = job_status_line(&job, false);
        assert!(
            !normal.contains("正在取消"),
            "非取消中不该出现取消提示: {normal}"
        );
    }

    #[test]
    fn push_log_writes_to_app_log_and_in_memory_vec() {
        let app_log = paths::logs_dir().join("app.log");
        let _ = std::fs::remove_file(&app_log);

        let mut lines: Vec<String> = Vec::new();
        push_log(&mut lines, "测试日志行".to_string());

        assert_eq!(lines, vec!["测试日志行".to_string()]);
        let content = std::fs::read_to_string(&app_log).expect("app.log 应该已经被写出来");
        assert!(content.contains("测试日志行"), "app.log 应该包含这行日志: {content}");
        assert!(content.contains("[unix "), "应该带时间戳前缀: {content}");

        let _ = std::fs::remove_file(&app_log);
    }

    #[test]
    fn should_rotate_log_flips_only_after_the_threshold() {
        assert!(!should_rotate_log(0, MAX_LOG_BYTES));
        assert!(!should_rotate_log(MAX_LOG_BYTES, MAX_LOG_BYTES), "恰好等于上限不该轮转");
        assert!(should_rotate_log(MAX_LOG_BYTES + 1, MAX_LOG_BYTES));
        assert!(should_rotate_log(MAX_LOG_BYTES * 10, MAX_LOG_BYTES));
    }

    #[test]
    fn rotate_log_file_renames_oversized_log_to_sibling() {
        let dir = std::env::temp_dir().join(format!(
            "subgen_test_rotate_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("app.log");
        std::fs::write(&log, vec![b'x'; 100]).unwrap();

        // 还没超过上限,不该轮转。
        rotate_log_file(&log, 1000);
        assert!(log.exists(), "未超限时原文件应该原样保留");
        assert!(!dir.join("app.log.1").exists(), "未超限时不该产生轮转文件");

        // 超过上限,轮转成 app.log.1,原位置腾出来让新日志从头写。
        rotate_log_file(&log, 50);
        assert!(!log.exists(), "轮转后原文件应该被改名走");
        assert!(dir.join("app.log.1").exists(), "旧内容应该落在 app.log.1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lang_display_translates_known_codes_and_passes_through_unknown() {
        assert_eq!(lang_display("ja"), "日语");
        assert_eq!(lang_display("auto"), "自动检测");
        assert_eq!(lang_display("xx"), "xx", "没覆盖到的代码应该原样返回,而不是丢掉信息");
    }

    #[test]
    fn format_job_report_suffix_combines_language_model_and_reuse_note() {
        let report = crate::job::JobReport {
            detected_language: Some("ja".to_string()),
            translate_model_label: Some("Sakura 7B(日→中,轻小说风格)"),
            subtitle_count: 42,
            reused_transcript: true,
            repetition_at: None,
        };
        assert_eq!(
            format_job_report_suffix(&report),
            " · 日语 · Sakura 7B(日→中,轻小说风格)(复用了上次的识别结果)"
        );
    }

    #[test]
    fn format_job_report_suffix_empty_when_nothing_to_show() {
        let report = crate::job::JobReport {
            detected_language: None,
            translate_model_label: None,
            subtitle_count: 10,
            reused_transcript: false,
            repetition_at: None,
        };
        assert_eq!(format_job_report_suffix(&report), "");
    }

    #[test]
    fn format_job_report_suffix_shows_language_only_when_translate_not_used() {
        let report = crate::job::JobReport {
            detected_language: Some("en".to_string()),
            translate_model_label: None,
            subtitle_count: 10,
            reused_transcript: false,
            repetition_at: None,
        };
        assert_eq!(format_job_report_suffix(&report), " · 英语");
    }

    fn job_with(status: JobStatus) -> Job {
        Job {
            video_path: PathBuf::from("test.mkv"),
            status,
            started_at: None,
            force_reprocess: false,
            preview_open: false,
        }
    }

    #[test]
    fn active_job_index_finds_the_single_running_job() {
        let jobs = vec![
            job_with(JobStatus::Done(crate::job::JobReport::default())),
            job_with(JobStatus::Running(crate::job::Stage::Transcribe, "x".to_string(), None)),
            job_with(JobStatus::Pending),
        ];
        assert_eq!(active_job_index(&jobs), Some(1));
    }

    #[test]
    fn active_job_index_none_when_nothing_running() {
        let jobs = vec![job_with(JobStatus::Pending), job_with(JobStatus::Cancelled)];
        assert_eq!(active_job_index(&jobs), None);
    }

    #[test]
    fn reconcile_selection_clears_when_removed_row_was_selected() {
        assert_eq!(reconcile_selection(Some(2), 2, 4), None);
    }

    #[test]
    fn reconcile_selection_shifts_down_when_removed_row_was_before_selection() {
        assert_eq!(reconcile_selection(Some(3), 1, 4), Some(2));
    }

    #[test]
    fn reconcile_selection_unaffected_when_removed_row_was_after_selection() {
        assert_eq!(reconcile_selection(Some(1), 3, 4), Some(1));
    }

    #[test]
    fn reconcile_selection_passes_through_none() {
        assert_eq!(reconcile_selection(None, 0, 3), None);
    }

    #[test]
    fn estimate_batch_remaining_none_without_any_completed_sample() {
        assert_eq!(
            estimate_batch_remaining(std::time::Duration::from_secs(60), 0, 10),
            None
        );
    }

    #[test]
    fn estimate_batch_remaining_extrapolates_linearly_from_average_pace() {
        // 4 个用了 60 秒(每个 15 秒),剩 6 个 → 90 秒。
        let remaining = estimate_batch_remaining(std::time::Duration::from_secs(60), 4, 10).unwrap();
        assert_eq!(remaining, std::time::Duration::from_secs(90));
    }

    #[test]
    fn format_batch_progress_line_includes_eta_once_available() {
        let line = format_batch_progress_line(5, 10, std::time::Duration::from_secs(60), 4);
        assert_eq!(line, "第 5/10 个 · 预计剩 1分30秒 · 已用 1分0秒");
    }

    #[test]
    fn format_batch_progress_line_omits_eta_before_first_sample() {
        let line = format_batch_progress_line(1, 10, std::time::Duration::from_secs(5), 0);
        assert_eq!(line, "第 1/10 个 · 已用 5秒");
    }

    #[test]
    fn batch_progress_counts_counts_only_terminal_states_within_the_batch() {
        let jobs = vec![
            job_with(JobStatus::Done(crate::job::JobReport::default())), // 下标 0,不在这批里
            job_with(JobStatus::Done(crate::job::JobReport::default())), // 下标 1,这批的
            job_with(JobStatus::Running(crate::job::Stage::Transcribe, "x".to_string(), None)), // 下标 2,这批的,还没到终态
        ];
        let (done, total) = batch_progress_counts(&jobs, &[1, 2]);
        assert_eq!((done, total), (1, 2));
    }
}
