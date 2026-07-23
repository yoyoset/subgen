//! 任务队列:GPU 一次只处理一个文件(并发反而抢显存更慢),后台线程串行处理,
//! 通过 channel 把每个文件的状态变化回传给界面线程。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc::Sender, Arc};

/// 流水线的三个阶段,有天然的先后顺序——界面上画步骤条时,"当前处于哪个阶段"本身就隐含了
/// "更早的阶段都已经完成或复用完了",不需要单独维护每个阶段各自的完成状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    Done,
    /// 处理成功,但有值得用户注意的情况(目前用于:检测到并自动折叠了"复读机"故障)
    DoneWithWarning(String),
    Skipped(String),
    /// 带上具体是哪个阶段失败的,界面步骤条上能直接标红对应的那一步,不用去猜。
    Failed(Stage, String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub video_path: PathBuf,
    pub status: JobStatus,
    /// 本文件开始处理(第一次收到 Running 状态)的时刻,用于界面显示"已运行 XXs",
    /// 不依赖 whisper.cpp 自己的进度回调频率——避免复杂/大文件长时间没有百分比更新时
    /// 界面看起来像卡死。
    pub started_at: Option<std::time::Instant>,
}

impl Job {
    pub fn new(video_path: PathBuf) -> Self {
        Self {
            video_path,
            status: JobStatus::Pending,
            started_at: None,
        }
    }
}

pub enum WorkerEvent {
    JobUpdate { index: usize, status: JobStatus },
    GpuMissing,
    BatchDone,
}

/// `batch` 是 (界面列表里的原始下标, 视频路径) 对,只包含这次真正要跑的任务——调用方
/// (main.rs::start_processing)已经把状态是 Done/DoneWithWarning 的任务过滤掉了,重新点
/// "开始处理" 不会把已完成的文件重新跑一遍。
#[allow(clippy::too_many_arguments)]
pub fn run_batch(
    batch: Vec<(usize, PathBuf)>,
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

        for (index, video_path) in batch.iter() {
            let index = *index;
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
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
                Err((_, e)) if e == "已取消" => {
                    let _ = tx.send(WorkerEvent::JobUpdate {
                        index,
                        status: JobStatus::Cancelled,
                    });
                    continue;
                }
                Err((stage, e)) => {
                    let _ = tx.send(WorkerEvent::JobUpdate {
                        index,
                        status: JobStatus::Failed(stage, e),
                    });
                    continue;
                }
            };

            // used_gpu 是 Option<bool>:None 表示这次复用了之前已经识别好的结果、没有真实
            // 观测(见 engine.rs 的断点续跑逻辑),这种情况不该被当成"没用上 GPU"报警告。
            if result.used_gpu == Some(false) {
                let _ = tx.send(WorkerEvent::GpuMissing);
            }

            let placed = match crate::engine::place_sidecar_srt(video_path, &result.srt_path, overwrite)
            {
                Ok(p) => p,
                Err(crate::engine::PlaceError::Skipped(e)) => {
                    let _ = tx.send(WorkerEvent::JobUpdate {
                        index,
                        status: JobStatus::Skipped(e),
                    });
                    continue;
                }
                Err(crate::engine::PlaceError::Failed(e)) => {
                    let _ = tx.send(WorkerEvent::JobUpdate {
                        index,
                        status: JobStatus::Failed(Stage::Transcribe, e),
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
            if result.repetition_collapsed {
                append_warning(
                    &mut warning,
                    "检测到疑似复读故障已自动折叠,建议抽查该片段".to_string(),
                );
            }

            if translate {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = tx.send(WorkerEvent::JobUpdate {
                        index,
                        status: JobStatus::Cancelled,
                    });
                    continue;
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
                    let _ = tx.send(WorkerEvent::JobUpdate {
                        index,
                        status: JobStatus::Running(
                            Stage::Translate,
                            "正在启动本地翻译引擎...".to_string(),
                            None,
                        ),
                    });
                    match crate::translate::LlamaServer::start(model_def, &cancel) {
                        Ok(s) => server = Some(s),
                        Err(e) if e == "已取消" => {
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
                            let _ = tx.send(WorkerEvent::JobUpdate {
                                index,
                                status: JobStatus::DoneWithWarning(warning.unwrap()),
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
                    overwrite,
                    &cancel,
                    move |text, pct| {
                        let _ = tx_progress.send(WorkerEvent::JobUpdate {
                            index,
                            status: JobStatus::Running(Stage::Translate, text, pct),
                        });
                    },
                );

                match translate_outcome {
                    Ok(crate::translate::TranslateResult::Written { failed_lines, .. }) => {
                        if failed_lines > 0 {
                            append_warning(
                                &mut warning,
                                format!("翻译中有 {failed_lines} 行未能对齐,已保留原文"),
                            );
                        }
                    }
                    Ok(crate::translate::TranslateResult::Skipped(e)) => {
                        append_warning(&mut warning, e);
                    }
                    Err(e) if e == "已取消" => {
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

            let status = match warning {
                Some(w) => JobStatus::DoneWithWarning(w),
                None => JobStatus::Done,
            };
            let _ = tx.send(WorkerEvent::JobUpdate { index, status });
        }

        if let Some(mut s) = server.take() {
            s.stop();
        }
        let _ = tx.send(WorkerEvent::BatchDone);
    });
}
