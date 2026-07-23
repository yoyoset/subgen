//! 调用 ffmpeg 提取音轨、调用 whisper-cli 识别,产出 .srt。
//!
//! 关键事实(均已用真实二进制在本机 RTX 3060 上实测验证,不是猜测):
//! - whisper-cli.exe 内部用 miniaudio 解码,只支持 flac/mp3/ogg/wav,**不支持 mp4/mkv 等视频容器**,
//!   必须先用 ffmpeg 抽取音轨为 wav。
//! - GPU 是否真正用上,看输出里是否出现 "using CUDA0 backend" 这一行;
//!   显式禁用 GPU(-ng)时会输出 "no GPU found"。已知 whisper.cpp 在部分 Windows 环境下会不报错地
//!   静默回退到纯 CPU,所以必须解析这一行来判断,不能假设"没报错就是用了 GPU"。
//! - 进度行格式为 "whisper_print_progress_callback: progress = NN%",且百分比可能超过 100
//!   (whisper.cpp 已知的估算误差),界面展示时需要 clamp 到 100。
//! - **默认不开 `--vad`**。实测(同一段真实纪录片音频对比)发现开 VAD 会导致输出文本丢失全部
//!   大小写和标点(比如 "Jeremy Wade, explorer" 变成 "jeremy wade explorer"),而不开 VAD 时
//!   格式完全正常、且在这类持续有人声的内容上没有观察到幻听/回退重试(fallbacks=0)。VAD 的
//!   防幻听收益主要体现在长静音/纯音调场景(单独测过,确实能避免瞎编),但对纪录片/剧集这种
//!   持续对白内容,VAD 弊大于利,所以默认关闭。
//! - **必须传 `-mc 0`(max-context 0)**。真实故障案例:一个 44 分钟的文件在 00:08:06 处进入
//!   死循环,同一句字幕原样重复 983 次直到文件结束,后面 32 分钟内容全部丢失;在同一文件上
//!   两次独立运行,故障位置完全一致(确定性复现),但截出的 4 分钟/15 分钟短片段单独测试
//!   都无法复现——说明问题不在音频本身,而是 whisper 默认把前一窗口识别出的文本作为下一
//!   窗口解码的上下文("condition on previous text"),一旦某句重复进入上下文就会自我强化,
//!   越喂越确信"接下来还是这句",从而卡死到文件结束。`-mc 0` 禁止跨窗口携带文本上下文,
//!   切断了这个自我强化的通路。已在同一份真实故障音频上验证:加 `-mc 0` 后重复计数从 983
//!   降到 0,fallbacks 从 5 降到 0,全片 44 分钟内容完整识别到位。代价很小:跨窗口的
//!   风格/标点一致性略降(实测中只在全片末尾约 68 秒观察到大小写丢失,规模远小于此前的
//!   983 行复读,暂未处理)。`collapse_repetitions` 安全网继续保留,作为万一还有其他
//!   失效模式时的兜底,不依赖它作为主要防线。

use crate::job::Stage;
use crate::job_guard;
use crate::paths;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Windows 进程创建标志:不给子进程分配控制台窗口。
/// 主程序自己用 `windows_subsystem = "windows"` 关掉了控制台后,子进程(ffmpeg/whisper-cli/
/// llama-server)反而会被 Windows 单独弹一个新控制台窗口(实测确认过,不加这个标志真的会弹窗)——
/// 这里显式禁止,子进程的 stdout/stderr 依然能通过管道正常捕获,不影响读取输出。
/// pub(crate) 因为 translate.rs 启动 llama-server 子进程时也要用同一个标志。
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct EngineOutcome {
    pub srt_path: PathBuf,
    /// None 表示这次没有真实观测(复用了之前识别好的结果,见"断点续跑"),不代表"没用上
    /// GPU"——调用方只应该在 `Some(false)` 时提示 GPU 警告。
    pub used_gpu: Option<bool>,
    /// 是否检测到并自动折叠了"复读机"故障(见 collapse_repetitions)。
    pub repetition_collapsed: bool,
    /// whisper 自动检测到的源语言(language="auto" 时来自日志解析;显式指定语言时就是那个值)。
    /// 用于翻译阶段挑选对应的翻译模型(如日语走 Sakura)。
    pub detected_language: Option<String>,
    /// 音轨是否复用了 `tmp/` 里已有的产物,而不是这次真实提取出来的。
    pub reused_wav: bool,
    /// 字幕是否复用了 `tmp/` 里已有的产物,而不是这次真实识别出来的——复用时完全没有
    /// 真实跑 whisper-cli,调用方(界面)应该让用户知道这次"完成"其实是"沿用了上次的结果",
    /// 而不是让用户误以为换了模型/语言重新识别过。
    pub reused_srt: bool,
}

/// 处理单个视频文件,progress_cb 用于把当前所在阶段/状态文案/百分比回传给调用方(界面线程)。
/// cancel 在关键节点(音频提取前、识别子进程运行中)会被检查。
///
/// **支持断点续跑**:提取音轨、语音识别这两步,如果 `tmp/` 里已经有对应哈希的产物且看起来
/// 完整,会直接复用、跳过重新跑——这样即使上次处理到一半崩溃/被打断,这次重新处理同一个
/// 文件也不用把已经做完的工作再做一遍。哈希由路径 + `model_key` + `language` 一起算出
/// (见 `cache_key`),换模型或换语言会自然算出不同的哈希、不会误命中旧结果。完全自动判断,
/// 不需要开关;想强制重新跑,手动删掉 `tmp/` 里对应的 `{文件名}_{哈希}.wav`/`.srt` 即可。
pub fn process_video(
    video_path: &Path,
    model_key: &str,
    language: &str,
    cancel: &Arc<AtomicBool>,
    mut progress_cb: impl FnMut(Stage, String, Option<f32>),
) -> Result<EngineOutcome, (Stage, String)> {
    if cancel.load(Ordering::Relaxed) {
        return Err((Stage::ExtractAudio, "已取消".to_string()));
    }

    let whisper_cli = paths::whisper_cli_exe()
        .ok_or_else(|| (Stage::ExtractAudio, "找不到 whisper-cli.exe,请先完成组件下载".to_string()))?;
    let ffmpeg = paths::ffmpeg_exe()
        .ok_or_else(|| (Stage::ExtractAudio, "找不到 ffmpeg.exe,请先完成组件下载".to_string()))?;
    let model_path = paths::model_path(model_key);
    if !model_path.is_file() {
        return Err((
            Stage::ExtractAudio,
            format!("模型文件不存在: {}", model_path.display()),
        ));
    }

    std::fs::create_dir_all(paths::tmp_dir())
        .map_err(|e| (Stage::ExtractAudio, format!("创建临时目录失败: {e}")))?;
    std::fs::create_dir_all(paths::logs_dir())
        .map_err(|e| (Stage::ExtractAudio, format!("创建日志目录失败: {e}")))?;

    let unique = cache_key(video_path, model_key, language);
    let wav_path = paths::tmp_dir().join(format!("{unique}.wav"));
    let srt_out_prefix = paths::tmp_dir().join(&unique); // whisper -of 不带扩展名,会自动加 .srt
    let log_file = paths::logs_dir().join(format!("{unique}.log"));

    // ---- 1. 提取音轨(支持复用已有的临时 wav) ----
    let wav_reused = wav_path.is_file() && wav_path.metadata().map(|m| m.len() > 0).unwrap_or(false);
    if wav_reused {
        progress_cb(
            Stage::ExtractAudio,
            "检测到已提取的音轨,跳过重新提取".to_string(),
            None,
        );
    } else {
        progress_cb(Stage::ExtractAudio, "正在提取音轨...".to_string(), None);
        let mut ff_cmd = Command::new(&ffmpeg);
        ff_cmd
            .arg("-y")
            .arg("-i")
            .arg(video_path)
            .args(["-vn", "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"])
            .arg(&wav_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        ff_cmd.creation_flags(CREATE_NO_WINDOW);
        let ff_child = ff_cmd
            .spawn()
            .map_err(|e| (Stage::ExtractAudio, format!("无法启动 ffmpeg: {e}")))?;
        job_guard::bind_child_to_job(&ff_child);
        let ff_output = ff_child
            .wait_with_output()
            .map_err(|e| (Stage::ExtractAudio, format!("等待 ffmpeg 失败: {e}")))?;

        if !ff_output.status.success() {
            let stderr = String::from_utf8_lossy(&ff_output.stderr);
            let tail: String = stderr.lines().rev().take(15).collect::<Vec<_>>().join("\n");
            return Err((
                Stage::ExtractAudio,
                format!("音频提取失败(该文件可能损坏或无音轨):\n{tail}"),
            ));
        }
        if !wav_path.is_file() {
            return Err((Stage::ExtractAudio, "音频提取未产生输出文件".to_string()));
        }
    }

    if cancel.load(Ordering::Relaxed) {
        // 不删 wav——它是这一步的完整产物,不是半成品。取消不该比崩溃损失更大:崩溃后
        // 断点续跑能直接复用已提取的音轨,主动取消却把它删掉、下次还要重新提取一遍,
        // 是反直觉的(用户点"取消"通常是想暂停/改主意,不是想作废已经做完的部分)。
        return Err((Stage::ExtractAudio, "已取消".to_string()));
    }

    // ---- 2. 语音识别(支持复用已有的 srt) ----
    let generated_srt = srt_out_prefix.with_extension("srt");
    if generated_srt.is_file() && srt_looks_complete(&generated_srt) {
        progress_cb(
            Stage::Transcribe,
            "检测到已识别的字幕,跳过重新识别".to_string(),
            None,
        );
        let detected_language = if language != "auto" {
            Some(language.to_string())
        } else {
            recover_detected_language_from_log(&log_file)
        };
        let repetition_collapsed = collapse_repetitions(&generated_srt).unwrap_or_else(|e| {
            let _ = std::fs::write(
                paths::logs_dir().join(format!("{unique}_collapse_error.log")),
                e,
            );
            false
        });
        return Ok(EngineOutcome {
            srt_path: generated_srt,
            used_gpu: None, // 这次没有真实跑 whisper-cli,不知道用没用上 GPU,不该瞎猜
            repetition_collapsed,
            detected_language,
            reused_wav: wav_reused,
            reused_srt: true,
        });
    }

    // 注意:这里传 None 而不是 Some(0.0) —— 真正的百分比要等 whisper-cli 输出第一条
    // progress 回调才有,大文件/复杂音频这条第一次更新可能要等一两分钟,期间界面靠
    // "已运行 XXs" 计时器(而不是卡在 0% 不动)让用户知道它没有卡死。
    progress_cb(Stage::Transcribe, "正在识别语音...".to_string(), None);

    let mut cmd = Command::new(&whisper_cli);
    cmd.arg("-m").arg(&model_path);
    cmd.arg("-f").arg(&wav_path);
    // 不加 --vad:实测会导致输出丢失大小写/标点(见本文件顶部说明),对持续对白类内容弊大于利。
    // -mc 0:禁止跨窗口携带文本上下文,避免长文件卡进复读死循环(见本文件顶部说明,已用真实
    // 故障文件验证有效)。
    cmd.arg("-mc").arg("0");
    cmd.arg("-osrt");
    cmd.arg("-of").arg(&srt_out_prefix);
    cmd.arg("-pp"); // 打印进度,用于界面展示
    if language != "auto" {
        cmd.arg("-l").arg(language);
    } else {
        cmd.arg("-l").arg("auto");
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd
        .spawn()
        .map_err(|e| (Stage::Transcribe, format!("无法启动识别引擎: {e}")))?;
    job_guard::bind_child_to_job(&child);

    let mut used_gpu = false;
    let mut detected_language: Option<String> = None;
    let mut full_log = String::new();

    // whisper.cpp 把大部分日志打到 stderr,把两路都读出来(分别开线程避免管道缓冲区打满死锁)
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(s) = stdout {
        let tx2 = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(s).lines().map_while(Result::ok) {
                let _ = tx2.send(line);
            }
        }));
    }
    if let Some(s) = stderr {
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(s).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    drop(cmd); // 释放原 cmd 的所有权引用(child 已持有实际进程)

    // 供两处(轮询中 / 进程退出后排水)复用的单行处理逻辑,避免此前两处实现不一致
    // (排水那份曾经漏了 CUDA1 判断和进度解析)。
    let mut handle_line = |line: &str,
                            full_log: &mut String,
                            used_gpu: &mut bool,
                            detected_language: &mut Option<String>| {
        full_log.push_str(line);
        full_log.push('\n');
        if line.contains("using CUDA0 backend") || line.contains("using CUDA1 backend") {
            *used_gpu = true;
        }
        if let Some(lang) = parse_detected_language(line) {
            *detected_language = Some(lang);
        }
        if let Some(pct) = parse_progress_percent(line) {
            progress_cb(
                Stage::Transcribe,
                "正在识别语音...".to_string(),
                Some(pct.min(100.0)),
            );
        }
    };

    loop {
        // 非阻塞地把已有的日志行都取出来处理,同时保持对取消信号的响应
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(line) => handle_line(&line, &mut full_log, &mut used_gpu, &mut detected_language),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            // wav 不删:它是完整产物,断点续跑用得上(见上面提取音轨那一步的取消分支)。
            // 半成品 srt 要删:whisper-cli 被杀掉时输出文件可能只写了一半,留着会被
            // srt_looks_complete 误判成"能解析出块所以算完整"而被断点续跑错误复用。
            let srt_partial = srt_out_prefix.with_extension("srt");
            let _ = std::fs::remove_file(&srt_partial);
            return Err((Stage::Transcribe, "已取消".to_string()));
        }

        if let Ok(Some(_status)) = child.try_wait() {
            // 进程已退出,把管道里剩余的行读完再跳出(用同一份 handle_line,不再遗漏 CUDA1/进度)
            while let Ok(line) = rx.try_recv() {
                handle_line(&line, &mut full_log, &mut used_gpu, &mut detected_language);
            }
            break;
        }
    }

    let status = child
        .wait()
        .map_err(|e| (Stage::Transcribe, format!("等待识别进程失败: {e}")))?;

    let _ = std::fs::remove_file(&wav_path); // 清理临时 wav,不留在磁盘上

    // 写日志文件,便于事后排查(界面只展示精简错误)。记录子进程真实退出码——遇到过
    // whisper-cli 输出显示完全正常(progress=100%、output_srt 已保存)但退出码非 0 的情况,
    // 当时完全没法知道具体是几号错误,只能干猜。
    let exit_desc = describe_exit_status(&status);
    let log_content = format!("{full_log}\n--- subgen 记录 ---\n{exit_desc}\n");
    let _ = std::fs::write(&log_file, &log_content);

    if !status.success() {
        let tail: String = full_log.lines().rev().take(15).collect::<Vec<_>>().join("\n");
        return Err((
            Stage::Transcribe,
            format!(
                "识别引擎返回错误({exit_desc},详见日志 {}):\n{tail}",
                log_file.display()
            ),
        ));
    }

    // 成功判据要严格:退出码为 0 还不够,必须确认目标 srt 文件确实生成(存在即可,允许内容为空 ——
    // 有些片段确实没有对白,空字幕是合理结果,不当作失败,但文件必须真实存在)
    if !generated_srt.is_file() {
        return Err((
            Stage::Transcribe,
            format!(
                "识别流程结束但未生成字幕文件(详见日志 {})",
                log_file.display()
            ),
        ));
    }

    let repetition_collapsed = collapse_repetitions(&generated_srt).unwrap_or_else(|e| {
        // 折叠失败不应该让整个任务失败——顶多是没清理掉复读,原样交付
        let _ = std::fs::write(
            paths::logs_dir().join(format!("{unique}_collapse_error.log")),
            e,
        );
        false
    });

    // language != "auto" 时用户已经明确指定了源语言,不依赖日志解析(whisper 在这种情况下
    // 也不会打印 "auto-detected language" 那一行)。
    let detected_language = if language != "auto" {
        Some(language.to_string())
    } else {
        detected_language
    };

    Ok(EngineOutcome {
        srt_path: generated_srt,
        used_gpu: Some(used_gpu),
        repetition_collapsed,
        detected_language,
        reused_wav: wav_reused,
        reused_srt: false,
    })
}

/// 判断复用的 srt 是不是"看起来完整"的产物,而不是上次处理中断留下的半成品:内容为空
/// (有些片段确实没有对白,空字幕合理)或者能解析出至少一块有效字幕,都算完整。
fn srt_looks_complete(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(content) => content.trim().is_empty() || !parse_srt_blocks(&content).is_empty(),
        Err(_) => false,
    }
}

/// 断点续跑复用已有 srt、跳过重新识别时,这次没有真实跑 whisper-cli,拿不到"auto-detected
/// language"这行日志——尝试从同一个哈希对应的旧 `.log` 文件(如果存在,比如上一次是完整跑完
/// 才留下的)里找。找不到就是 None,退化成 "auto",翻译阶段会落到兜底的通用模型,不算错误。
fn recover_detected_language_from_log(log_file: &Path) -> Option<String> {
    let content = std::fs::read_to_string(log_file).ok()?;
    content.lines().find_map(parse_detected_language)
}

/// 解析 whisper.cpp 的语言自动检测日志行,例如:
/// "whisper_full_with_state: auto-detected language: ja (p = 0.987654)"
/// 返回 ISO 639-1 语言代码(如 "ja"),解析失败返回 None。
fn parse_detected_language(line: &str) -> Option<String> {
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

/// whisper.cpp 已知的"复读机"故障模式:长文件连续解码偶发进入死循环,同一句话原样
/// 重复几十上百次(实测过,真实出现:同一句 983 次)。这里检测连续 >= 4 次完全相同的
/// 字幕文本,视为异常并折叠成一条(时间跨度取首尾),避免把成百上千行垃圾原样交付给用户。
/// 返回 true 表示确实检测到并处理过至少一处。
fn collapse_repetitions(srt_path: &Path) -> Result<bool, String> {
    const REPEAT_THRESHOLD: usize = 4;

    let content =
        std::fs::read_to_string(srt_path).map_err(|e| format!("读取字幕文件失败: {e}"))?;
    let blocks = parse_srt_blocks(&content);

    let mut collapsed = Vec::with_capacity(blocks.len());
    let mut found = false;
    let mut i = 0;
    while i < blocks.len() {
        let text = blocks[i].2.trim();
        let mut j = i + 1;
        while j < blocks.len() && blocks[j].2.trim() == text {
            j += 1;
        }
        let run_len = j - i;
        if run_len >= REPEAT_THRESHOLD {
            found = true;
            let start = blocks[i].0.clone();
            let end = blocks[j - 1].1.clone();
            collapsed.push((start, end, blocks[i].2.clone()));
        } else {
            collapsed.extend_from_slice(&blocks[i..j]);
        }
        i = j;
    }

    if found {
        std::fs::write(srt_path, write_srt_blocks(&collapsed))
            .map_err(|e| format!("写回字幕文件失败: {e}"))?;
    }
    Ok(found)
}

/// 极简 SRT 解析:按空行分块,每块取"起止时间戳"+"文本行拼接"。
/// 不追求处理所有 SRT 边缘格式,够用来做重复检测/折叠即可。
/// pub(crate) 因为 translate.rs 复用它来读取原文 srt、按块喂给翻译模型。
pub(crate) fn parse_srt_blocks(content: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
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
                    text_lines.push(lines[k]);
                    k += 1;
                }
                out.push((start, end, text_lines.join(" ")));
                i = k;
                continue;
            }
        }
        i += 1;
    }
    out
}

pub(crate) fn write_srt_blocks(blocks: &[(String, String, String)]) -> String {
    let mut out = String::new();
    for (idx, (start, end, text)) in blocks.iter().enumerate() {
        out.push_str(&format!("{}\n{start} --> {end}\n{text}\n\n", idx + 1));
    }
    out
}

/// 目标已存在且不允许覆盖("跳过",不算失败)与真正的 IO 错误("失败")需要在界面上
/// 用不同颜色/文案区分,所以用专门的错误类型而不是笼统的 String。
pub enum PlaceError {
    Skipped(String),
    Failed(String),
}

/// 把生成的 srt 从临时目录移动到视频同目录、同名(标准外挂字幕命名规则)。
/// overwrite=false 且目标已存在时返回 Skipped 而不是覆盖。
pub fn place_sidecar_srt(
    video_path: &Path,
    generated_srt: &Path,
    overwrite: bool,
) -> Result<PathBuf, PlaceError> {
    let target = paths::sidecar_srt_path(video_path);
    if target.exists() && !overwrite {
        let _ = std::fs::remove_file(generated_srt);
        return Err(PlaceError::Skipped(format!(
            "已存在同名字幕,已跳过: {}",
            target.display()
        )));
    }
    std::fs::copy(generated_srt, &target)
        .map_err(|e| PlaceError::Failed(format!("写入字幕文件失败: {e}")))?;
    let _ = std::fs::remove_file(generated_srt);
    Ok(target)
}

/// 把子进程退出状态格式化成人能看懂、也方便拿去搜索的形式。十进制退出码之外额外给一个
/// 十六进制形式(按 u32 重新解释,不是简单地给负数加个 0x 前缀)——Windows 的已知状态码
/// 习惯用十六进制表示(比如 STATUS_ACCESS_VIOLATION 是 0xc0000005),十进制形式基本没人
/// 认得出来,不转成十六进制的话,记了退出码也等于没记。
fn describe_exit_status(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("退出码: {code}(十六进制 0x{:08x})", code as u32),
        None => "退出码: 无法获取(进程可能是被终止的,不是自己正常退出)".to_string(),
    }
}

fn parse_progress_percent(line: &str) -> Option<f32> {
    // "whisper_print_progress_callback: progress = 42%"
    let idx = line.find("progress = ")?;
    let rest = &line[idx + "progress = ".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<f32>().ok()
}

fn simple_hash(s: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

/// tmp/logs 里中间产物的文件名前缀,同时也是断点续跑的"是否可以复用"的判断依据。
/// 哈希输入包含路径 + 模型 + 语言,不是只有路径——不然换个识别模型或手动指定源语言重新
/// 处理同一个文件时,会直接复用上一次(用旧模型/旧语言)跑出来的 tmp/ 产物,界面上却
/// 显示"完成",用户会以为新设置生效了,其实什么都没变(真实踩过的坑:换模型重跑无效)。
/// 把 model_key/language 也纳入哈希,换了任何一个都会算出不同的 key,自然触发重新
/// 提取/重新识别,不需要额外的"是否要重新计算"判断逻辑。
fn cache_key(video_path: &Path, model_key: &str, language: &str) -> String {
    let stem = video_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio");
    format!(
        "{stem}_{:x}",
        simple_hash(&format!("{}|{model_key}|{language}", video_path.to_string_lossy()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 断点续跑的"命中缓存"判断依赖这个 key,换模型或换语言必须算出不同的 key,否则
    /// 用户切换设置重新处理同一个文件时会静默复用旧结果——真实踩过的坑:换模型重跑无效,
    /// 界面显示"完成"但内容和上一次一模一样。
    #[test]
    fn cache_key_changes_with_model_or_language() {
        let path = Path::new(r"F:\videos\Episode.mkv");
        let base = cache_key(path, "large-v3", "auto");
        assert_ne!(
            base,
            cache_key(path, "medium", "auto"),
            "换模型应该产生不同的 key"
        );
        assert_ne!(
            base,
            cache_key(path, "large-v3", "ja"),
            "换语言应该产生不同的 key"
        );
        assert_eq!(
            base,
            cache_key(path, "large-v3", "auto"),
            "同样的路径/模型/语言应该稳定算出同一个 key(断点续跑靠这个复用产物)"
        );
    }

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

        let found = collapse_repetitions(&tmp).expect("折叠失败");
        assert!(found, "应该检测到复读并折叠");

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

    #[test]
    fn place_sidecar_srt_skips_when_target_exists_and_no_overwrite() {
        let dir = std::env::temp_dir().join("subgen_test_place_skip");
        std::fs::create_dir_all(&dir).unwrap();
        let video_path = dir.join("Episode.mkv");
        let target = dir.join("Episode.srt");
        std::fs::write(&target, "existing").unwrap();
        let generated = dir.join("generated.srt");
        std::fs::write(&generated, "new content").unwrap();

        let result = place_sidecar_srt(&video_path, &generated, false);
        assert!(matches!(result, Err(PlaceError::Skipped(_))));
        // 跳过时不应该改动已有的目标文件内容
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "existing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn place_sidecar_srt_overwrites_when_requested() {
        let dir = std::env::temp_dir().join("subgen_test_place_overwrite");
        std::fs::create_dir_all(&dir).unwrap();
        let video_path = dir.join("Episode.mkv");
        let target = dir.join("Episode.srt");
        std::fs::write(&target, "old").unwrap();
        let generated = dir.join("generated.srt");
        std::fs::write(&generated, "new content").unwrap();

        let result = place_sidecar_srt(&video_path, &generated, true);
        assert!(result.is_ok());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new content");

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
        let found = collapse_repetitions(&tmp).unwrap();
        assert!(!found, "正常内容不应该被判定为复读故障");
        let _ = std::fs::remove_file(&tmp);
    }
}
