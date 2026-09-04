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

mod srt;
mod whisper_log;

pub(crate) use srt::{
    count_subtitle_blocks, parse_srt_blocks, parse_srt_blocks_lines, wrap_and_split_blocks,
    write_srt_blocks, DEFAULT_SUBTITLE_MAX_WIDTH,
};
use srt::{collapse_repetitions, srt_looks_complete};
use whisper_log::{describe_exit_status, parse_detected_language, parse_progress_percent, recover_detected_language_from_log};

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

/// 识别/提取失败时的错误信息:(哪个阶段失败, 给用户看的一行摘要, 完整技术细节所在的日志
/// 文件路径)。摘要故意不塞退出码/stderr 这类技术细节——那些留在日志文件里,界面只展示
/// 一行人话,想深挖的用户点"查看日志"按钮直接打开文件,不用对着一屏英文技术输出发呆。
pub type EngineError = (Stage, String, Option<PathBuf>);

/// 构造一个没有对应日志文件的错误——比如"找不到 whisper-cli.exe"这类还没真正跑起来就
/// 失败的情况,压根没有子进程日志可看。
fn no_log(stage: Stage, msg: impl Into<String>) -> EngineError {
    (stage, msg.into(), None)
}

/// Windows 进程创建标志:不给子进程分配控制台窗口。
/// 主程序自己用 `windows_subsystem = "windows"` 关掉了控制台后,子进程(ffmpeg/whisper-cli/
/// llama-server)反而会被 Windows 单独弹一个新控制台窗口(实测确认过,不加这个标志真的会弹窗)——
/// 这里显式禁止,子进程的 stdout/stderr 依然能通过管道正常捕获,不影响读取输出。
/// pub(crate) 因为 translate.rs 启动 llama-server 子进程时也要用同一个标志。
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug)]
pub struct EngineOutcome {
    pub srt_path: PathBuf,
    /// None 表示这次没有真实观测(复用了之前识别好的结果,见"断点续跑"),不代表"没用上
    /// GPU"——调用方只应该在 `Some(false)` 时提示 GPU 警告。
    pub used_gpu: Option<bool>,
    /// 检测到并自动折叠了"复读机"故障时,首个被折叠片段的起始时间戳(见 collapse_repetitions);
    /// 没检测到是 None。界面据此能告诉用户具体去哪个时间点抽查,而不是笼统一句"建议核对"。
    pub repetition_at: Option<String>,
    /// whisper 自动检测到的源语言(language="auto" 时来自日志解析;显式指定语言时就是那个值)。
    /// 用于翻译阶段挑选对应的翻译模型(如日语走 Sakura)。
    pub detected_language: Option<String>,
    /// 字幕是否复用了 `tmp/` 里已有的产物,而不是这次真实识别出来的——复用时完全没有
    /// 真实跑 whisper-cli,调用方(界面)应该让用户知道这次"完成"其实是"沿用了上次的结果",
    /// 而不是让用户误以为换了模型/语言重新识别过。
    pub reused_srt: bool,
    /// 字幕块数量(`parse_srt_blocks` 的结果长度,复用分支也会重新数一遍)。0 表示这段
    /// 视频没有识别出任何对白——界面据此把"空字幕"和"识别失败"区分开,不让用户误以为
    /// 软件坏了。
    pub subtitle_count: usize,
}

/// 处理单个视频文件,progress_cb 用于把当前所在阶段/状态文案/百分比回传给调用方(界面线程)。
/// cancel 在关键节点(音频提取前、识别子进程运行中)会被检查。
///
/// **支持断点续跑**:提取音轨、语音识别这两步,如果 `tmp/` 里已经有对应哈希的产物且看起来
/// 完整,会直接复用、跳过重新跑——这样即使上次处理到一半崩溃/被打断,这次重新处理同一个
/// 文件也不用把已经做完的工作再做一遍。哈希由路径 + `model_key` + `language` 一起算出
/// (见 `cache_key`),换模型或换语言会自然算出不同的哈希、不会误命中旧结果。完全自动判断,
/// 不需要开关;想强制重新跑,`force=true` 即可(界面上"重新处理"按钮用的就是这个),不用
/// 再手动去 `tmp/` 里删文件。
///
/// `force=true` 时两个复用检查都直接跳过,效果上等同于"当作 tmp/ 里从来没有这份产物"——
/// 不需要额外先物理删除旧文件:ffmpeg 命令本来就带 `-y`(覆盖已有输出),whisper-cli 写
/// 输出文件也是标准的截断写入,重新跑一遍会自然覆盖旧内容,不会残留旧数据的痕迹。
pub fn process_video(
    video_path: &Path,
    model_key: &str,
    language: &str,
    force: bool,
    cancel: &Arc<AtomicBool>,
    mut progress_cb: impl FnMut(Stage, String, Option<f32>),
) -> Result<EngineOutcome, EngineError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(no_log(Stage::ExtractAudio, "已取消"));
    }

    // 先确认视频文件真的还在——加入队列后文件可能被移动/删除,如果不查,ffmpeg 会报一个
    // 误导性的"音频提取失败(该文件可能损坏或无音轨)",用户以为文件坏了,实际只是找不到。
    // 不存在的路径也无法生成 cache_key 对应的中间产物,提前失败是对的。
    if !video_path.is_file() {
        return Err(no_log(
            Stage::ExtractAudio,
            format!("找不到视频文件: {}", video_path.display()),
        ));
    }

    let whisper_cli = paths::whisper_cli_exe()
        .ok_or_else(|| no_log(Stage::ExtractAudio, "找不到 whisper-cli.exe,请先完成组件下载"))?;
    let ffmpeg = paths::ffmpeg_exe()
        .ok_or_else(|| no_log(Stage::ExtractAudio, "找不到 ffmpeg.exe,请先完成组件下载"))?;
    let model_path = paths::model_path(model_key);
    if !model_path.is_file() {
        return Err(no_log(
            Stage::ExtractAudio,
            format!("模型文件不存在: {}", model_path.display()),
        ));
    }

    std::fs::create_dir_all(paths::tmp_dir())
        .map_err(|e| no_log(Stage::ExtractAudio, format!("创建临时目录失败: {e}")))?;
    std::fs::create_dir_all(paths::logs_dir())
        .map_err(|e| no_log(Stage::ExtractAudio, format!("创建日志目录失败: {e}")))?;

    let unique = cache_key(video_path, model_key, language);
    let wav_path = paths::tmp_dir().join(format!("{unique}.wav"));
    let srt_out_prefix = paths::tmp_dir().join(&unique); // whisper -of 不带扩展名,会自动加 .srt
    let generated_srt = generated_srt_path(&unique);
    let log_file = paths::logs_dir().join(format!("{unique}.log"));

    // ---- 1. 提取音轨(支持复用已有的临时 wav,force=true 时强制当作不存在) ----
    let wav_reused = !force
        && wav_path.is_file()
        && wav_path.metadata().map(|m| m.len() > 0).unwrap_or(false);
    if wav_reused {
        progress_cb(
            Stage::ExtractAudio,
            "检测到已提取的音轨,跳过重新提取".to_string(),
            None,
        );
    } else {
        // 括号里的预期时长是给用户一个"这一步正常应该多快"的参照——没有这个信息,
        // 30 秒没动静就容易被当成卡死,尤其这是整个流水线的第一步,第一印象很重要。
        progress_cb(
            Stage::ExtractAudio,
            "正在提取音轨(通常不到一分钟)...".to_string(),
            None,
        );
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
        let mut ff_child = ff_cmd
            .spawn()
            .map_err(|e| no_log(Stage::ExtractAudio, format!("无法启动 ffmpeg: {e}")))?;
        job_guard::bind_child_to_job(&ff_child);

        // 等待 ffmpeg 结束,期间持续响应取消并排空 stderr(见 wait_child_with_cancel)。
        // 真实踩过的坑:旧实现用 `wait_with_output()` 阻塞等待子进程结束,期间完全不检查
        // 取消标志——ffmpeg 万一卡住(极少见,比如某些损坏文件),用户点"取消"后批次永远
        // 停在这一步,"正在取消…"会一直转下去。
        let ff_stderr_handle = ff_child.stderr.take();
        let (ff_status, ff_stderr) = match wait_child_with_cancel(&mut ff_child, ff_stderr_handle, cancel) {
            Ok(v) => v,
            Err(e) => {
                // 取消时 ffmpeg 可能只写了半截 wav。删掉它——留着会被断点续跑误判成
                // "已提取好的完整音轨"(wav_reused 只看 is_file && len>0)直接复用,喂给
                // whisper 的是截断的音频,会产出一份静默丢失后半段的字幕。这和识别阶段
                // 取消时删半成品 srt 是同一个道理:半成品不该被当成完整产物复用。
                let _ = std::fs::remove_file(&wav_path);
                return Err(no_log(Stage::ExtractAudio, e));
            }
        };

        if !ff_status.success() {
            // ffmpeg 失败说明这次提取没成功,输出的 wav 不可靠(可能只写了一半)。删掉
            // 以免下次运行误复用,重新提取一次比产出静默丢失内容的字幕便宜得多。
            let _ = std::fs::remove_file(&wav_path);
            // 完整 stderr 写进日志文件,界面只展示一行人话摘要——ffmpeg 的报错动辄十几行,
            // 塞进状态栏会撑爆一行的高度,技术细节留给"查看日志"按钮的用户。失败原因不限于
            // 文件损坏(还有缺解码组件/权限等),所以文案不把原因钉死在"文件损坏"上。
            // 日志写盘失败时返回 None 而不是一个不存在的路径,避免界面出现点了打不开的
            // "查看日志"按钮。
            let ffmpeg_log = paths::logs_dir().join(format!("{unique}_ffmpeg.log"));
            let log_path = std::fs::write(&ffmpeg_log, ff_stderr.as_slice())
                .ok()
                .map(|_| ffmpeg_log);
            return Err((
                Stage::ExtractAudio,
                "音频提取失败,详见日志(可能是文件损坏/无音轨/缺解码组件)".to_string(),
                log_path,
            ));
        }
        if !wav_path.is_file() {
            return Err(no_log(Stage::ExtractAudio, "音频提取未产生输出文件"));
        }
    }

    if cancel.load(Ordering::Relaxed) {
        // 不删 wav——它是这一步的完整产物,不是半成品。取消不该比崩溃损失更大:崩溃后
        // 断点续跑能直接复用已提取的音轨,主动取消却把它删掉、下次还要重新提取一遍,
        // 是反直觉的(用户点"取消"通常是想暂停/改主意,不是想作废已经做完的部分)。
        return Err(no_log(Stage::ExtractAudio, "已取消"));
    }

    // ---- 2. 语音识别(支持复用已有的 srt,force=true 时强制当作不存在) ----
    if !force && generated_srt.is_file() && srt_looks_complete(&generated_srt) {
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
        let repetition_at = collapse_repetitions(&generated_srt).unwrap_or_else(|e| {
            let _ = std::fs::write(
                paths::logs_dir().join(format!("{unique}_collapse_error.log")),
                e,
            );
            None
        });
        return Ok(EngineOutcome {
            srt_path: generated_srt.clone(),
            used_gpu: None, // 这次没有真实跑 whisper-cli,不知道用没用上 GPU,不该瞎猜
            repetition_at,
            detected_language,
            reused_srt: true,
            subtitle_count: count_subtitle_blocks(&generated_srt),
        });
    }

    // 注意:这里传 None 而不是 Some(0.0) —— 真正的百分比要等 whisper-cli 输出第一条
    // progress 回调才有,大文件/复杂音频这条第一次更新可能要等一两分钟,期间界面靠
    // "已运行 XXs" 计时器 + 这句文案里的预期时长说明(而不是卡在 0% 不动)让用户知道
    // 它没有卡死。等真的有百分比可显示了(下面 handle_line 里),文案会换回不带这句
    // 说明的版本——进度条已经在动了,不再需要"可能要等一两分钟"这种预期管理。
    progress_cb(
        Stage::Transcribe,
        "正在识别语音(大文件可能一两分钟后才显示进度)...".to_string(),
        None,
    );

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
        .map_err(|e| no_log(Stage::Transcribe, format!("无法启动识别引擎: {e}")))?;
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
            let srt_partial = generated_srt_path(&unique);
            let _ = std::fs::remove_file(&srt_partial);
            return Err(no_log(Stage::Transcribe, "已取消"));
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
        .map_err(|e| no_log(Stage::Transcribe, format!("等待识别进程失败: {e}")))?;

    let _ = std::fs::remove_file(&wav_path); // 清理临时 wav,不留在磁盘上

    // 写日志文件,便于事后排查(界面只展示精简错误)。记录子进程真实退出码——遇到过
    // whisper-cli 输出显示完全正常(progress=100%、output_srt 已保存)但退出码非 0 的情况,
    // 当时完全没法知道具体是几号错误,只能干猜。
    let exit_desc = describe_exit_status(&status);
    let log_content = format!("{full_log}\n--- subgen 记录 ---\n{exit_desc}\n");
    // 日志写盘失败时 log_path 为 None,失败任务不展示一个点不开的"查看日志"按钮。
    let log_path = std::fs::write(&log_file, &log_content)
        .ok()
        .map(|_| log_file.clone());

    if !status.success() {
        // 完整输出(progress 行、CUDA 探测、原始报错)都在 log_file 里,界面只展示一行
        // 摘要——之前直接把最后 15 行原始日志拼进状态栏,一屏英文技术输出对用户毫无意义,
        // 想深挖的用户点"查看日志"按钮打开这个文件就够了。
        return Err((
            Stage::Transcribe,
            format!("识别引擎出错({exit_desc})"),
            log_path,
        ));
    }

    // 成功判据要严格:退出码为 0 还不够,必须确认目标 srt 文件确实生成(存在即可,允许内容为空 ——
    // 有些片段确实没有对白,空字幕是合理结果,不当作失败,但文件必须真实存在)
    if !generated_srt.is_file() {
        return Err((
            Stage::Transcribe,
            "识别流程结束但未生成字幕文件".to_string(),
            log_path,
        ));
    }

    let repetition_at = collapse_repetitions(&generated_srt).unwrap_or_else(|e| {
        // 折叠失败不应该让整个任务失败——顶多是没清理掉复读,原样交付
        let _ = std::fs::write(
            paths::logs_dir().join(format!("{unique}_collapse_error.log")),
            e,
        );
        None
    });

    // language != "auto" 时用户已经明确指定了源语言,不依赖日志解析(whisper 在这种情况下
    // 也不会打印 "auto-detected language" 那一行)。
    let detected_language = if language != "auto" {
        Some(language.to_string())
    } else {
        detected_language
    };

    let subtitle_count = count_subtitle_blocks(&generated_srt);

    Ok(EngineOutcome {
        srt_path: generated_srt,
        used_gpu: Some(used_gpu),
        repetition_at,
        detected_language,
        reused_srt: false,
        subtitle_count,
    })
}



/// 目标已存在且不允许覆盖("跳过",不算失败)与真正的 IO 错误("失败")需要在界面上
/// 用不同颜色/文案区分,所以用专门的错误类型而不是笼统的 String。
pub enum PlaceError {
    Skipped(String),
    Failed(String),
}

/// 把生成的 srt 从临时目录移动到视频同目录、同名(标准外挂字幕命名规则)。
/// overwrite=false 且目标已存在时返回 Skipped 而不是覆盖。
/// `max_subtitle_width` 传给折行(0 关闭);`generated_srt` 是 whisper 的原文产物,
/// 单语言模式不经过翻译直接落到这里,它的超宽行同样要被排版层处理,不能只处理翻译路径。
pub fn place_sidecar_srt(
    video_path: &Path,
    generated_srt: &Path,
    overwrite: bool,
    max_subtitle_width: usize,
) -> Result<PathBuf, PlaceError> {
    let target = paths::sidecar_srt_path(video_path);
    if let Some(reason) = crate::format::existing_srt_skip_reason(video_path, overwrite) {
        let _ = std::fs::remove_file(generated_srt);
        return Err(PlaceError::Skipped(reason));
    }
    if max_subtitle_width == 0 {
        // 关闭折行时退化成原来的纯复制行为,产物逐字节不变,不引入 parse→write 往返。
        let tmp_target = target.with_extension("srt.tmp");
        std::fs::copy(generated_srt, &tmp_target)
            .map_err(|e| PlaceError::Failed(format!("写入字幕文件失败: {e}")))?;
        std::fs::rename(&tmp_target, &target).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_target);
            PlaceError::Failed(format!("写入字幕文件失败: {e}"))
        })?;
        let _ = std::fs::remove_file(generated_srt);
        return Ok(target);
    }
    // 覆盖已有字幕时,先写同目录临时名再原子改名——直接覆盖的话,中途失败会把用户已有的
    // 旧字幕截断成半截。改名是同一目录内的原子操作,失败时旧字幕保持完好。
    // 用 parse_srt_blocks_lines 保留块内原有的多行结构(whisper 的 srt 块通常单行,
    // 万一多行也别把行边界丢掉),再逐块折行后写出,而不是直接复制临时文件。
    let content = std::fs::read_to_string(generated_srt)
        .map_err(|e| PlaceError::Failed(format!("读取字幕文件失败: {e}")))?;
    let blocks: Vec<(String, String, String)> = parse_srt_blocks_lines(&content)
        .into_iter()
        .map(|(start, end, lines)| (start, end, lines.join("\n")))
        .collect();
    let layout = wrap_and_split_blocks(blocks, max_subtitle_width);
    let tmp_target = target.with_extension("srt.tmp");
    std::fs::write(&tmp_target, write_srt_blocks(&layout))
        .map_err(|e| PlaceError::Failed(format!("写入字幕文件失败: {e}")))?;
    std::fs::rename(&tmp_target, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_target);
        PlaceError::Failed(format!("写入字幕文件失败: {e}"))
    })?;
    let _ = std::fs::remove_file(generated_srt);
    Ok(target)
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

/// whisper-cli 最终写出的识别结果路径,在 tmp/ 下,文件名是 `{unique}.srt`。
///
/// 真实踩过的坑:不能写成 `paths::tmp_dir().join(unique).with_extension("srt")`。网上下载
/// 的剧集文件名本身就一堆点(比如 "Deadliest.Catch.S22E05.720p.HEVC.x265-MeGusta[EZTVx.to]"),
/// `unique` 是拿这段文件名拼出来的(见 `cache_key` 里的 `stem`),`PathBuf::with_extension`
/// 只看"文件名里最后一个点之后的部分"当"扩展名"替换,会把文件名从那个点之后整段吃掉换成
/// "srt",算出一个和 whisper-cli 实际写入的文件完全不同的路径——真机用真实的剧集文件名跑过
/// 一次,`generated_srt.is_file()` 因此永远是 false,whisper-cli 明明成功了,界面却报错
/// "识别流程结束但未生成字幕文件"。whisper-cli 自己对 `-of` 参数只是纯字符串拼接 + ".srt"
/// (不经过 Path 的"扩展名"语义),这里必须用同样的拼接方式才能算出一致的路径。
fn generated_srt_path(unique: &str) -> PathBuf {
    paths::tmp_dir().join(format!("{unique}.srt"))
}

/// 等待子进程结束,期间持续响应取消信号并排空 stderr——不排空会撑满管道缓冲区把子进程
/// 卡死。取消时 kill 子进程并返回 `Err("已取消")`。返回 `(退出状态, 收集到的完整 stderr)`。
///
/// 抽出成独立函数:提取音轨(ffmpeg)用的是同一个"轮询 + 取消响应 + 排空管道"模式,而且
/// 这段逻辑可以脱离具体子进程单独验证(见测试里用真实 ffmpeg 跑真实视频的临时测试)。
fn wait_child_with_cancel(
    child: &mut std::process::Child,
    stderr_handle: Option<std::process::ChildStderr>,
    cancel: &Arc<AtomicBool>,
) -> Result<(std::process::ExitStatus, Vec<u8>), String> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    if let Some(handle) = stderr_handle {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = handle;
            let mut buf = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = tx.send(buf[..n].to_vec());
                    }
                    Err(_) => break,
                }
            }
        });
    }
    let mut stderr = Vec::new();
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("已取消".to_string());
        }
        if let Ok(chunk) = rx.try_recv() {
            stderr.extend_from_slice(&chunk);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(format!("等待子进程失败: {e}")),
        }
    };
    // 子进程已退出,把管道里剩余的 stderr 排空(reader 线程可能还有最后几块没送到)。
    while let Ok(chunk) = rx.try_recv() {
        stderr.extend_from_slice(&chunk);
    }
    Ok((status, stderr))
}

/// 启动时尽力而为地清理 `tmp/` 里长期无人复用的中间产物(.wav/.srt)。断点续跑依赖
/// tmp/ 里的中间文件,但那些文件只在"中断后很快重跑同一个文件"的场景才有价值——隔了
/// 很久(默认 7 天)还没被复用,基本可以确定用户不会再续,留着只会让 tmp/ 无限膨胀
/// (每个处理过的视频都留一份 wav + 一份 srt,一批 20 集能占几个 GB)。只删超过期限的,
/// 新鲜的绝不碰——单线程串行处理,启动时本来也没有正在跑的任务,不会误删进行中的产物。
/// 返回清理掉的文件数;任何一步失败都静默跳过,清理只是维护性动作,不该影响启动。
pub fn cleanup_stale_tmp(older_than: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(paths::tmp_dir()) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_artifact = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("wav") || e.eq_ignore_ascii_case("srt"))
            .unwrap_or(false);
        if is_artifact && is_stale(&path, older_than) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 启动时清理 `logs/` 里长期无人复用的每视频日志(`{unique}.log`、`{unique}_ffmpeg.log`、
/// `{unique}_collapse_error.log`,见本文件 `process_video` 的写日志处)。这些日志只在断点
/// 续跑时用来恢复 `detected_language`(`recover_detected_language_from_log`),一旦对应的
/// tmp 中间产物被 `cleanup_stale_tmp` 过期清理,它们同样失去价值——用同一个过期窗口联动
/// 清理,否则 logs/ 会随处理的视频数无限累积(每个视频一份,几十 KB,一批 20 集就是 1MB+,
/// 长期使用几百个视频能攒出几百 MB)。识别逻辑按扩展名 `.log` + 排除系统日志文件名
/// (`app.log`/`crash.log`),新加的每视频日志模式自动被覆盖,不用每种后缀各写一条规则。
/// 系统日志有自己的轮转机制(`format::rotate_log_file`),绝不在这里误删。返回清理掉的
/// 文件数;任何一步失败都静默跳过,清理只是维护性动作,不该影响启动。
pub fn cleanup_stale_logs(older_than: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(paths::logs_dir()) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let is_per_video_log = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("log"))
            .unwrap_or(false)
            && file_name != "app.log"
            && file_name != "crash.log";
        if is_per_video_log && is_stale(&path, older_than) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 判断文件最后修改时间是否已早于 `older_than` 之前。纯谓词,方便单测;元数据读不到时
/// 一律判"不陈旧",绝不因为读不到时间就删掉一个可能还在用的文件。
pub fn is_stale(path: &Path, older_than: std::time::Duration) -> bool {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .map(|age| age > older_than)
        .unwrap_or(false)
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

    /// 真实踩过的坑:剧集下载文件名(如 "Deadliest.Catch.S22E05.720p.HEVC.x265-MeGusta
    /// [EZTVx.to]")本身就有很多点,`cache_key` 拼出的 `unique` 会原样带着这些点。用
    /// `PathBuf::with_extension("srt")` 算最终路径会被这些内部的点误伤(只看"最后一个点
    /// 之后"当扩展名整段替换掉),`generated_srt_path` 改用纯字符串拼接就不会有这个问题——
    /// 这里直接验证这一点:结果文件名必须是 "{完整的 unique}.srt",一个字符都不能丢。
    #[test]
    fn generated_srt_path_preserves_dots_in_original_filename() {
        let path = Path::new(r"F:\videos\Deadliest.Catch.S22E05.720p.HEVC.x265-MeGusta[EZTVx.to].mkv");
        let unique = cache_key(path, "large-v3", "auto");
        let generated = generated_srt_path(&unique);
        assert_eq!(
            generated.file_name().and_then(|n| n.to_str()),
            Some(format!("{unique}.srt").as_str()),
            "文件名内部的点不该被当成扩展名分隔符吃掉: {generated:?}"
        );
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

        let result = place_sidecar_srt(&video_path, &generated, false, 42);
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
        // 超宽行在落盘前应被折行,而不是原样复制到视频同目录(不翻译的路径也要排版)。
        let long_line = "This is a fairly long English sentence that will wrap.";
        std::fs::write(
            &generated,
            format!("1\n00:00:01,000 --> 00:00:05,000\n{long_line}\n\n"),
        )
        .unwrap();

        let result = place_sidecar_srt(&video_path, &generated, true, 20);
        assert!(result.is_ok());
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(written.contains('\n'), "超宽行应该被折行: {written:?}");
        for line in written.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.contains("-->") && trimmed.parse::<u64>().is_err() {
                assert!(
                    crate::engine::srt::display_width(trimmed) <= 20,
                    "折行后仍有超宽行: {line:?} (written={written:?})"
                );
            }
        }
        // 内容不丢:把文本行(非空、非编号、非时间戳)抽出来,去掉空白后和原文一致。
        let text_lines: Vec<&str> = written
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.contains("-->") && t.parse::<u64>().is_err()
            })
            .collect();
        let norm = |s: &str| s.chars().filter(|c| *c != '\n' && *c != ' ').collect::<String>();
        assert_eq!(norm(&text_lines.join("\n")), norm(long_line));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// max_subtitle_width=0 时退化回纯复制,输出与临时文件逐字节一致——"关闭折行"不该
    /// 顺带把文件重新排版一遍。
    #[test]
    fn place_sidecar_srt_with_zero_width_copies_byte_identically() {
        let dir = std::env::temp_dir().join("subgen_test_place_zero_width");
        std::fs::create_dir_all(&dir).unwrap();
        let video_path = dir.join("Episode.mkv");
        let target = dir.join("Episode.srt");
        let generated = dir.join("generated.srt");
        let content = "1\n00:00:01,000 --> 00:00:02,000\n  A line with a leading space\n\n";
        std::fs::write(&generated, content).unwrap();

        let result = place_sidecar_srt(&video_path, &generated, true, 0);
        assert!(result.is_ok());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), content);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_stale_false_for_missing_file() {
        let missing = std::env::temp_dir().join("subgen_test_is_stale_missing.bin");
        assert!(
            !is_stale(&missing, std::time::Duration::ZERO),
            "读不到元数据的文件不该被判定为陈旧(避免误删)"
        );
    }

    /// 加入队列后视频文件被移动/删除时,应该明确报"找不到视频文件",而不是让 ffmpeg 报
    /// 一个误导性的"音频提取失败(该文件可能损坏或无音轨)"。
    #[test]
    fn process_video_reports_missing_video_file_clearly() {
        let missing = std::env::temp_dir().join("subgen_test_missing_video.mkv");
        let _ = std::fs::remove_file(&missing);
        let cancel = Arc::new(AtomicBool::new(false));
        let result = process_video(&missing, "large-v3", "auto", false, &cancel, |_, _, _| {});
        let (stage, msg, log) = result.unwrap_err();
        assert_eq!(stage, Stage::ExtractAudio);
        assert!(
            msg.contains("找不到视频文件"),
            "应该明确指出文件不存在: {msg}"
        );
        assert!(log.is_none(), "这一步还没有子进程日志可看");
    }

    /// cleanup_stale_tmp 只删超过期限的 .wav/.srt,新鲜的和其他扩展名一律不动。
    /// 用 set_modified 把文件修改时间拨回一小时前,模拟"很久以前留下的中间产物"。
    #[test]
    fn cleanup_stale_tmp_removes_only_expired_wav_and_srt() {
        let dir = paths::tmp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let unique = format!(
            "cleanup_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let stale_wav = dir.join(format!("{unique}_stale.wav"));
        let stale_srt = dir.join(format!("{unique}_stale.srt"));
        let fresh_wav = dir.join(format!("{unique}_fresh.wav"));
        let unrelated = dir.join(format!("{unique}_note.txt"));
        std::fs::write(&stale_wav, b"a").unwrap();
        std::fs::write(&stale_srt, b"b").unwrap();
        std::fs::write(&fresh_wav, b"c").unwrap();
        std::fs::write(&unrelated, b"d").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for p in [&stale_wav, &stale_srt] {
            // 用带写权限的句柄才能改修改时间(Windows 上只读句柄改 set_modified 会拒绝)。
            let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
            f.set_modified(old).unwrap();
        }

        let removed = cleanup_stale_tmp(std::time::Duration::from_secs(300));

        assert!(removed >= 2, "至少应该清掉自己造的两个过期产物: {removed}");
        assert!(!stale_wav.exists(), "过期的 wav 应该被清掉");
        assert!(!stale_srt.exists(), "过期的 srt 应该被清掉");
        assert!(fresh_wav.exists(), "新鲜的中间产物不该被清掉");
        assert!(unrelated.exists(), "不是 .wav/.srt 的文件不该被清掉");

        for p in [stale_wav, stale_srt, fresh_wav, unrelated] {
            let _ = std::fs::remove_file(&p);
        }
    }

    /// cleanup_stale_logs 只删 logs/ 里过期的每视频日志(`{unique}.log` 等),系统日志
    /// (`app.log`/`crash.log`)和新鲜的一律不动——系统日志有自己的轮转机制,绝不能在这里
    /// 被当成"每视频日志"误删。
    #[test]
    fn cleanup_stale_logs_removes_only_expired_per_video_logs() {
        let dir = paths::logs_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let unique = format!(
            "cleanup_logs_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let stale_transcribe = dir.join(format!("{unique}.log"));
        let stale_ffmpeg = dir.join(format!("{unique}_ffmpeg.log"));
        let stale_collapse = dir.join(format!("{unique}_collapse_error.log"));
        let fresh_transcribe = dir.join(format!("{unique}_fresh.log"));
        let system_log = dir.join("app.log");
        let system_crash = dir.join("crash.log");
        for p in [
            &stale_transcribe,
            &stale_ffmpeg,
            &stale_collapse,
            &fresh_transcribe,
            &system_log,
            &system_crash,
        ] {
            std::fs::write(p, b"log content").unwrap();
        }
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for p in [&stale_transcribe, &stale_ffmpeg, &stale_collapse, &system_log, &system_crash] {
            // 用带写权限的句柄才能改修改时间(Windows 上只读句柄改 set_modified 会拒绝)。
            let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
            f.set_modified(old).unwrap();
        }

        let removed = cleanup_stale_logs(std::time::Duration::from_secs(300));

        assert!(removed >= 3, "至少应该清掉自己造的三个过期每视频日志: {removed}");
        assert!(!stale_transcribe.exists(), "过期的 {unique}.log 应该被清掉");
        assert!(!stale_ffmpeg.exists(), "过期的 _ffmpeg.log 应该被清掉");
        assert!(!stale_collapse.exists(), "过期的 _collapse_error.log 应该被清掉");
        assert!(fresh_transcribe.exists(), "新鲜的每视频日志不该被清掉");
        assert!(system_log.exists(), "app.log 即使过期也不该被清掉(有自己的轮转)");
        assert!(system_crash.exists(), "crash.log 即使过期也不该被清掉(有自己的轮转)");

        for p in [
            stale_transcribe,
            stale_ffmpeg,
            stale_collapse,
            fresh_transcribe,
            system_log,
            system_crash,
        ] {
            let _ = std::fs::remove_file(&p);
        }
    }
}