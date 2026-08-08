//! 任务队列:GPU 一次只处理一个文件(并发反而抢显存更慢),后台线程串行处理,
//! 通过 channel 把每个文件的状态变化回传给界面线程。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc::Sender, Arc};

/// 流水线的三个阶段,有天然的先后顺序——界面上画步骤条时,"当前处于哪个阶段"本身就隐含了
/// "更早的阶段都已经完成或复用完了",不需要单独维护每个阶段各自的完成状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    ExtractAudio,
    Transcribe,
    Translate,
}

impl Stage {
    /// 0-based 顺序,界面画步骤条时用来判断"这一步在当前阶段之前/之后"。
    pub fn ordinal(self) -> u8 {
        match self {
            Stage::ExtractAudio => 0,
            Stage::Transcribe => 1,
            Stage::Translate => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Stage::ExtractAudio => "提取音轨",
            Stage::Transcribe => "识别",
            Stage::Translate => "翻译",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Pending,
    Running(Stage, String, Option<f32>),
    Done(JobReport),
    /// 处理成功,但有值得用户注意的情况(比如检测到并自动折叠了"复读机"故障、翻译部分行
    /// 失败、语言检测不确定)
    DoneWithWarning(String, JobReport),
    Skipped(String),
    /// 带上具体是哪个阶段失败的(界面步骤条上能直接标红对应的那一步,不用去猜)和完整
    /// 技术细节所在的日志文件路径(Some 时界面加"查看日志"按钮;None 表示这类失败压根
    /// 没有子进程日志可看,比如缺组件、取消)。
    Failed(Stage, String, Option<PathBuf>),
    Cancelled,
}

impl JobStatus {
    /// 这个任务是不是已经到了不会再变的终态(不会再收到新的 JobUpdate)。用于窗口标题的
    /// "第 N/M 个"进度统计——数一批里有多少个任务已经跑完(不管成功/跳过/失败/取消),
    /// 不是只数成功的。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobStatus::Done(_)
                | JobStatus::DoneWithWarning(..)
                | JobStatus::Skipped(_)
                | JobStatus::Failed(..)
                | JobStatus::Cancelled
        )
    }
}

/// 一次处理成功结束后,值得回显给用户看的信息——之前 `JobStatus::Done` 是个无字段变体,
/// 这些数据在到达终点前就被扔掉了,用户只能看到一个"完成",没有任何自查线索(用了哪个
/// 语言/哪个翻译模型?这次是不是复用了旧结果?字幕是不是空的?)。这个结构体不是给程序
/// 内部用的中间状态,是专门为了"诚实展示"而存在的。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobReport {
    /// whisper 检测/用户指定的源语言(ISO 639-1),None 表示没能确定。
    pub detected_language: Option<String>,
    /// 实际用来翻译的模型 label(如 "Sakura 7B(日→中,轻小说风格)");没开翻译或翻译没有
    /// 真正产出结果时是 None。
    pub translate_model_label: Option<&'static str>,
    /// 最终字幕的条数,0 表示没有识别出任何对白。
    pub subtitle_count: usize,
    /// 这次识别是否复用了 `tmp/` 里已有的产物,没有真的重新跑 whisper-cli。
    pub reused_transcript: bool,
    /// 检测到"复读机"故障并自动折叠时,首个折叠片段的起始时间戳;没检测到是 None。
    pub repetition_at: Option<String>,
}

impl JobReport {
    pub fn empty_subtitle(&self) -> bool {
        self.subtitle_count == 0
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub video_path: PathBuf,
    pub status: JobStatus,
    /// 本文件开始处理(第一次收到 Running 状态)的时刻,用于界面显示"已运行 XXs",
    /// 不依赖 whisper.cpp 自己的进度回调频率——避免复杂/大文件长时间没有百分比更新时
    /// 界面看起来像卡死。
    pub started_at: Option<std::time::Instant>,
    /// 用户在任务行点了"重新处理"——即使这个任务已经是 Done/DoneWithWarning 终态,下次
    /// "开始处理"也要把它纳入这一批,并且绕过 engine.rs 的断点续跑缓存真正重新跑一遍。
    /// 入批后由 main.rs::start_processing 复位,不是"一直重新处理"的开关。
    pub force_reprocess: bool,
    /// 完成态任务行是否展开了字幕预览区(见 main.rs 的预览按钮)。
    pub preview_open: bool,
}

impl Job {
    pub fn new(video_path: PathBuf) -> Self {
        Self {
            video_path,
            status: JobStatus::Pending,
            started_at: None,
            force_reprocess: false,
            preview_open: false,
        }
    }
}

#[derive(Debug)]
pub enum WorkerEvent {
    JobUpdate { index: usize, status: JobStatus },
    /// 带上是哪个阶段没用上 GPU(识别 or 翻译)——两个阶段各自独立检测,警告文案要能
    /// 说清具体是哪一步慢,不是笼统一句"没用上 GPU"。
    GpuMissing(Stage),
    BatchDone,
}

/// 把翻译失败的行下标(0-based,来自 `translate::TranslateResult::Written`)拼成用户能看懂
/// 的警告文案:"有 3 行翻译失败,已保留原文(第 45、112、113 条)"——不只是给个数量,让用户
/// 知道具体是哪几行,不用对着几百行字幕从头核对。最多列出前 5 个具体行号(1-based,和 srt
/// 里的编号对齐),超过 5 个只列前 5 个加"等",避免大量失败时警告文案本身长到没法看。
fn format_failed_lines_warning(failed_indices: &[usize]) -> String {
    let numbers: Vec<String> = failed_indices.iter().take(5).map(|i| (i + 1).to_string()).collect();
    let list = numbers.join("、");
    let suffix = if failed_indices.len() > 5 { "等" } else { "" };
    format!(
        "有 {} 行翻译失败,已保留原文(第 {list} 条{suffix})",
        failed_indices.len()
    )
}

/// worker 线程内部 panic 时,把这一批里还没走到终态的任务标记为失败——否则界面会永远
/// 卡在"处理中"(BatchDone 收不到),或者显示着"等待中/正在识别"这种和真实情况不符的
/// 状态,属于"后台实际失败但界面看起来成功"。`finished` 是已经收到过终态更新的任务
/// 下标集合。返回受影响的任务数。
fn fail_unfinished_batch_items(
    batch: &[(usize, PathBuf, bool)],
    finished: &std::collections::HashSet<usize>,
    tx: &Sender<WorkerEvent>,
) -> usize {
    let mut count = 0;
    for (index, _, _) in batch {
        if !finished.contains(index) {
            let _ = tx.send(WorkerEvent::JobUpdate {
                index: *index,
                status: JobStatus::Failed(
                    Stage::Transcribe,
                    "批次内部错误,已中止(详情见 logs/crash.log),请重新开始处理该文件"
                        .to_string(),
                    None,
                ),
            });
            count += 1;
        }
    }
    count
}

/// 从 `catch_unwind` 的 panic payload 里尽量提取一行人话消息(常见的是 `&'static str` 或
/// `String`);提取不到返回占位文案,不让日志里出现一个无法阅读的 `Any`。
pub(crate) fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(无消息)".to_string()
    }
}

/// `batch` 是 (界面列表里的原始下标, 视频路径, 是否强制重新处理) 三元组,只包含这次真正
/// 要跑的任务——调用方(main.rs::start_processing)已经把状态是 Done/DoneWithWarning 且
/// 没有被用户点了"重新处理"的任务过滤掉了,重新点"开始处理"不会把已完成的文件再跑一遍。
/// `force` 为 true 的任务(用户在任务行点了"重新处理")会绕过 engine.rs 的断点续跑缓存,
/// 也会让这个文件的输出覆盖策略等效于 overwrite=true——用户主动要求重新处理,就不该再被
/// "已存在同名字幕,跳过" 挡住。
#[allow(clippy::too_many_arguments)]
pub fn run_batch(
    batch: Vec<(usize, PathBuf, bool)>,
    model: String,
    language: String,
    overwrite: bool,
    translate: bool,
    translate_model: String,
    bilingual: bool,
    tx: Sender<WorkerEvent>,
    cancel: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        // 批次内同一个翻译模型只启动一次、复用给多个文件用(启动一次要加载几 GB 模型到显存,
        // 每个文件都重启代价太大);模型需要切换(比如这批里既有日语又有英语视频、选的是
        // "自动"策略)时才重启。批次结束或被取消时显式 stop,避免 llama-server 常驻。
        let mut server: Option<crate::translate::LlamaServer> = None;
        // 已收到过终态更新的任务下标——worker 内部万一 panic,用它在批次层面兜底:剩余
        // 任务标记为失败、BatchDone 照常发送。不能出现"worker 线程死了、界面永远卡在
        // 处理中"这种比任务本身失败更糟的状态。
        let mut finished: std::collections::HashSet<usize> = std::collections::HashSet::new();

        // 用 catch_unwind 包住整个批次循环:任何意外 panic(比如将来引入的新代码路径里的
        // unwrap)都被收住,而不是让 worker 线程静默死掉。AssertUnwindSafe 是显式接受
        // "panic 可能把 server/finished 留在中间状态"的风险——panic 时 server 会在下面的
        // 分支里显式 stop,finished 只被读取用于标记剩余失败项,都能兜住。
        let run_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for (index, video_path, force) in batch.iter() {
                let index = *index;
                let force = *force;
                // 用户点"重新处理"就是要覆盖旧结果,不该再被"已存在同名字幕,跳过"挡住。
                let effective_overwrite = overwrite || force;
                // 预检:目标 .srt 已存在且不覆盖时,整个任务直接跳过,不启动 ffmpeg/whisper。
                // 否则用户会白等几分钟(提取音轨+识别全跑完),最后才在 place_sidecar_srt
                // 那里被告知"已有同名字幕,跳过"——跳过本身不该花这么长时间。
                if let Some(reason) = crate::format::existing_srt_skip_reason(video_path, effective_overwrite) {
                    finished.insert(index);
                    let _ = tx.send(WorkerEvent::JobUpdate {
                        index,
                        status: JobStatus::Skipped(reason),
                    });
                    continue;
                }
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    finished.insert(index);
                    let _ = tx.send(WorkerEvent::JobUpdate {
                        index,
                        status: JobStatus::Cancelled,
                    });
                    continue;
                }

                let tx_progress = tx.clone();
                let outcome = crate::engine::process_video(
                    video_path,
                    &model,
                    &language,
                    force,
                    &cancel,
                    move |stage, text, pct| {
                        let _ = tx_progress.send(WorkerEvent::JobUpdate {
                            index,
                            status: JobStatus::Running(stage, text, pct),
                        });
                    },
                );

                let result = match outcome {
                    Ok(r) => r,
                    Err((_, e, _)) if e == "已取消" => {
                        finished.insert(index);
                        let _ = tx.send(WorkerEvent::JobUpdate {
                            index,
                            status: JobStatus::Cancelled,
                        });
                        continue;
                    }
                    Err((stage, e, log_path)) => {
                        finished.insert(index);
                        let _ = tx.send(WorkerEvent::JobUpdate {
                            index,
                            status: JobStatus::Failed(stage, e, log_path),
                        });
                        continue;
                    }
                };

                // used_gpu 是 Option<bool>:None 表示这次复用了之前已经识别好的结果、没有真实
                // 观测(见 engine.rs 的断点续跑逻辑),这种情况不该被当成"没用上 GPU"报警告。
                if result.used_gpu == Some(false) {
                    let _ = tx.send(WorkerEvent::GpuMissing(Stage::Transcribe));
                }

                let placed = match crate::engine::place_sidecar_srt(
                    video_path,
                    &result.srt_path,
                    effective_overwrite,
                ) {
                    Ok(p) => p,
                    Err(crate::engine::PlaceError::Skipped(e)) => {
                        finished.insert(index);
                        let _ = tx.send(WorkerEvent::JobUpdate {
                            index,
                            status: JobStatus::Skipped(e),
                        });
                        continue;
                    }
                    Err(crate::engine::PlaceError::Failed(e)) => {
                        // 文件系统操作(把生成的 srt 从 tmp/ 挪到视频同目录)失败,不涉及子进程,
                        // 没有对应的日志文件。
                        finished.insert(index);
                        let _ = tx.send(WorkerEvent::JobUpdate {
                            index,
                            status: JobStatus::Failed(Stage::Transcribe, e, None),
                        });
                        continue;
                    }
                };

                let mut warning: Option<String> = None;
                let append_warning = |current: &mut Option<String>, msg: String| {
                    *current = Some(match current.take() {
                        Some(w) => format!("{w}; {msg}"),
                        None => msg,
                    });
                };
                if let Some(ts) = &result.repetition_at {
                    append_warning(
                        &mut warning,
                        format!("在 {ts} 附近检测到异常重复内容,已自动修正,建议播放核对该处"),
                    );
                }

                // 终态回显信息:结束时不管走哪条路径(正常完成、翻译引擎启动失败等),都要带上
                // 这份 report,不然用户只看到"完成"两个字,没有任何自查线索(用了哪个语言/哪个
                // 翻译模型、这次是不是复用了旧结果)。捕获 &result 而不是拿走所有权,因为下面
                // 还要继续读 result 的其它字段。
                let build_report = |translate_model_label: Option<&'static str>| JobReport {
                    detected_language: result.detected_language.clone(),
                    translate_model_label,
                    subtitle_count: result.subtitle_count,
                    reused_transcript: result.reused_srt,
                    repetition_at: result.repetition_at.clone(),
                };
                let mut translate_model_label: Option<&'static str> = None;

                if translate {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        finished.insert(index);
                        let _ = tx.send(WorkerEvent::JobUpdate {
                            index,
                            status: JobStatus::Cancelled,
                        });
                        continue;
                    }

                    // 语言检测不确定(比如断点续跑复用了旧结果、但找不到对应的旧日志来恢复语言)
                    // 会导致下面 pick_model 落到通用兜底模型,而不是真正匹配源语言的专用模型
                    // (比如日语本该走 Sakura)。这个事实要让用户知道,不然翻译质量下降却完全
                    // 找不到原因。
                    if result.detected_language.is_none() {
                        append_warning(
                            &mut warning,
                            "未能确定源语言,已使用通用翻译模型;如果源语言是日语,建议手动指定语言后重新处理".to_string(),
                        );
                    }

                    let source_lang = result.detected_language.as_deref().unwrap_or("auto");
                    let model_def = crate::translate::pick_model(source_lang, &translate_model);

                    let need_restart = match &server {
                        Some(s) => s.model_key != model_def.key,
                        None => true,
                    };
                    if need_restart {
                        if let Some(mut s) = server.take() {
                            s.stop();
                        }
                        // 括号里的预期时长同 engine.rs 的提取音轨文案——启动翻译引擎要把几 GB
                        // 的模型文件加载进显存,这一步没有百分比可显示,不给个"大概多久"的
                        // 参照,长达一分钟的停顿会被当成卡死。
                        let _ = tx.send(WorkerEvent::JobUpdate {
                            index,
                            status: JobStatus::Running(
                                Stage::Translate,
                                "正在加载翻译模型(文件较大,首次约需一分钟)...".to_string(),
                                None,
                            ),
                        });
                        match crate::translate::LlamaServer::start(model_def, &cancel) {
                            Ok(s) => {
                                // 复用识别阶段已有的 GpuMissing 警告通路(main.rs 的
                                // gpu_warning_stages)——翻译阶段之前完全没有 GPU 检测,静默
                                // 掉到纯 CPU 时用户只会感觉"翻译很慢",没有任何提示指向真正
                                // 原因。见 translate.rs 的 used_gpu 字段和 parse_gpu_offload。
                                if !s.used_gpu() {
                                    let _ = tx.send(WorkerEvent::GpuMissing(Stage::Translate));
                                }
                                server = Some(s);
                            }
                            Err(e) if e == "已取消" => {
                                finished.insert(index);
                                let _ = tx.send(WorkerEvent::JobUpdate {
                                    index,
                                    status: JobStatus::Cancelled,
                                });
                                continue;
                            }
                            Err(e) => {
                                append_warning(
                                    &mut warning,
                                    format!("翻译引擎启动失败,已保留原文字幕: {e}"),
                                );
                                finished.insert(index);
                                let _ = tx.send(WorkerEvent::JobUpdate {
                                    index,
                                    status: JobStatus::DoneWithWarning(
                                        warning.unwrap(),
                                        build_report(None),
                                    ),
                                });
                                continue;
                            }
                        }
                    }

                    let tx_progress = tx.clone();
                    let translate_outcome = crate::translate::translate_srt(
                        server.as_ref().unwrap(),
                        model_def,
                        &placed,
                        video_path,
                        bilingual,
                        effective_overwrite,
                        &cancel,
                        move |text, pct| {
                            let _ = tx_progress.send(WorkerEvent::JobUpdate {
                                index,
                                status: JobStatus::Running(Stage::Translate, text, pct),
                            });
                        },
                    );

                    match translate_outcome {
                        Ok(crate::translate::TranslateResult::Written { failed_indices }) => {
                            // 只在翻译真的产出了结果时才回显用了哪个模型——启动失败/跳过/出错
                            // 这几条路径都提前 continue 或没有真正跑起来,不该让用户以为翻译
                            // 用的是这个模型。
                            translate_model_label = Some(model_def.label);
                            if !failed_indices.is_empty() {
                                append_warning(&mut warning, format_failed_lines_warning(&failed_indices));
                            }
                        }
                        Ok(crate::translate::TranslateResult::Skipped(e)) => {
                            append_warning(&mut warning, e);
                        }
                        Err(e) if e == "已取消" => {
                            finished.insert(index);
                            let _ = tx.send(WorkerEvent::JobUpdate {
                                index,
                                status: JobStatus::Cancelled,
                            });
                            continue;
                        }
                        Err(e) => {
                            append_warning(&mut warning, format!("翻译失败(原文字幕已保留): {e}"));
                        }
                    }
                }

                let report = build_report(translate_model_label);
                let status = match warning {
                    Some(w) => JobStatus::DoneWithWarning(w, report),
                    None => JobStatus::Done(report),
                };
                finished.insert(index);
                let _ = tx.send(WorkerEvent::JobUpdate { index, status });
            }

            if let Some(mut s) = server.take() {
                s.stop();
            }
        }));

        // worker 内部 panic 的兜底:哪怕某个文件处理代码里出了意外崩溃,也不能让界面永远
        // 卡在"处理中"。已完成的照常保留,没走到的标记为失败,BatchDone 照常发送。
        if let Err(panic) = run_result {
            if let Some(mut s) = server.take() {
                s.stop();
            }
            let msg = panic_payload_message(&panic);
            crate::format::push_log(
                &mut Vec::new(),
                format!("批次 worker 线程内部崩溃,已中止剩余任务: {msg}"),
            );
            let affected = fail_unfinished_batch_items(&batch, &finished, &tx);
            crate::format::push_log(
                &mut Vec::new(),
                format!("已把 {affected} 个未完成任务标记为失败"),
            );
        }
        let _ = tx.send(WorkerEvent::BatchDone);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_failed_lines_warning_lists_all_indices_when_five_or_fewer() {
        let msg = format_failed_lines_warning(&[0, 44, 111]);
        assert_eq!(msg, "有 3 行翻译失败,已保留原文(第 1、45、112 条)");
    }

    #[test]
    fn format_failed_lines_warning_truncates_with_suffix_when_more_than_five() {
        let msg = format_failed_lines_warning(&[0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(msg, "有 7 行翻译失败,已保留原文(第 1、2、3、4、5 条等)");
    }

    #[test]
    fn is_terminal_covers_every_ending_status_but_not_pending_or_running() {
        let report = JobReport::default();
        assert!(!JobStatus::Pending.is_terminal());
        assert!(!JobStatus::Running(Stage::Transcribe, "x".to_string(), None).is_terminal());
        assert!(JobStatus::Done(report.clone()).is_terminal());
        assert!(JobStatus::DoneWithWarning("w".to_string(), report).is_terminal());
        assert!(JobStatus::Skipped("s".to_string()).is_terminal());
        assert!(JobStatus::Failed(Stage::Transcribe, "f".to_string(), None).is_terminal());
        assert!(JobStatus::Cancelled.is_terminal());
    }

    /// worker 内部 panic 时,只有没走到终态的任务才该被标记失败;已完成的不该被覆盖。
    #[test]
    fn fail_unfinished_batch_items_marks_only_unfinished_tasks() {
        let batch = vec![
            (0usize, PathBuf::from("a.mkv"), false),
            (1usize, PathBuf::from("b.mkv"), false),
            (5usize, PathBuf::from("c.mkv"), false),
        ];
        let mut finished = std::collections::HashSet::new();
        finished.insert(1); // 下标 1 已经收到过终态更新

        let (tx, rx) = std::sync::mpsc::channel();
        let count = fail_unfinished_batch_items(&batch, &finished, &tx);
        assert_eq!(count, 2, "应该只标记还没完成的两个任务");

        let mut failed_indices: Vec<usize> = Vec::new();
        while let Ok(WorkerEvent::JobUpdate { index, status }) = rx.try_recv() {
            assert!(matches!(status, JobStatus::Failed(..)), "标记失败的应该是 Failed 状态");
            failed_indices.push(index);
        }
        failed_indices.sort();
        assert_eq!(failed_indices, vec![0, 5]);
    }

    /// panic payload 的人话提取:`&str` 和 `String` 两种常见形态都要能读出来。
    #[test]
    fn panic_payload_message_extracts_str_and_string() {
        let str_payload = std::panic::catch_unwind(|| panic!("boom")).unwrap_err();
        assert_eq!(panic_payload_message(&str_payload), "boom");

        let string_payload = std::panic::catch_unwind(|| panic!("编号 {}", 42)).unwrap_err();
        assert_eq!(panic_payload_message(&string_payload), "编号 42");
    }

    /// 关键不变量:无论批次内容是什么(这里用空批次),worker 都必须送出 BatchDone,
    /// 界面才不会永远卡在"处理中"。
    #[test]
    fn run_batch_always_sends_batch_done_even_for_empty_batch() {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        run_batch(
            Vec::new(),
            "large-v3".to_string(),
            "auto".to_string(),
            false,
            false,
            "auto".to_string(),
            true,
            tx,
            cancel,
        );
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(WorkerEvent::BatchDone) => {}
            other => panic!("空批次也应收到 BatchDone,实际: {other:?}"),
        }
    }
}
