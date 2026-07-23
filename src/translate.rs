//! 本地字幕翻译:调用 llama-server(llama.cpp 预编译二进制)子进程,通过其 OpenAI 兼容
//! HTTP API 把原文字幕翻译成中文,生成 `Video.zh.srt`(双语或仅中文,见 Config::bilingual)。
//!
//! 模型选择策略(关键事实,写代码时核实过,不是猜测):
//! - SakuraLLM 系列模型**只训练了日→中方向**,是专门用海量日中轻小说平行语料微调的,译文风格、
//!   人名/术语处理明显强于同规模通用模型;但喂英文等其他语言效果很差(不在其训练分布内)。
//! - 所以采用"语言 → 模型"映射表:日语走 Sakura,其余语言走通用指令模型(Qwen3-4B-Instruct)。
//!   `langs` 里用 `"*"` 表示兜底模型,匹配不到具体语言时用它。
//! - 与 engine.rs 里 whisper-cli 的子进程调用方式(便携二进制、子进程、CREATE_NO_WINDOW)保持
//!   一致,不引入 FFI/CUDA 编译依赖。

use crate::engine;
use crate::paths;
use serde::Serialize;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// llama-server 监听端口。批次内只会有一个 LlamaServer 实例存活,固定端口即可,
/// 不需要动态探测空闲端口(v1 简化)。
const PORT: u16 = 8712;

/// 每批喂给模型的字幕行数。数值来自实测权衡:太大容易在长输出中丢行/错位编号,
/// 太小则 HTTP 往返开销主导总耗时、拖慢整体处理速度。
const BATCH_SIZE: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// SakuraLLM 官方提示词风格(轻小说翻译)
    Sakura,
    /// 通用指令模型的字幕翻译提示词
    General,
}

pub struct TransModel {
    /// 供 Config::translate_model / 界面下拉框使用的稳定标识符
    pub key: &'static str,
    pub label: &'static str,
    pub file_name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    /// 文件字节数,来自 HuggingFace LFS 指针文件核实过的真实值(不是估算),用于下载前的
    /// 磁盘空间提示(见 setup.rs 的 estimated_size_bytes)。
    pub size_bytes: u64,
    /// 该模型适用的源语言(ISO 639-1),`"*"` 表示兜底/通用
    pub langs: &'static [&'static str],
    pub prompt_kind: PromptKind,
}

pub const SAKURA_7B: TransModel = TransModel {
    key: "sakura-7b",
    label: "Sakura 7B(日→中,轻小说风格)",
    file_name: "sakura-7b-qwen2.5-v1.0-iq4xs.gguf",
    url: "https://huggingface.co/SakuraLLM/Sakura-7B-Qwen2.5-v1.0-GGUF/resolve/main/sakura-7b-qwen2.5-v1.0-iq4xs.gguf",
    sha256: "30e82ca13bc1e321f4f7b77459d533d483d46200522dc5c31eef9d2f2a6335d6",
    size_bytes: 4_250_298_208,
    langs: &["ja"],
    prompt_kind: PromptKind::Sakura,
};

pub const QWEN3_4B: TransModel = TransModel {
    key: "qwen3-4b",
    label: "Qwen3 4B(通用,多语言→中)",
    file_name: "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
    url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
    sha256: "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597",
    size_bytes: 2_497_281_120,
    langs: &["*"],
    prompt_kind: PromptKind::General,
};

pub const MODELS: &[TransModel] = &[SAKURA_7B, QWEN3_4B];

pub fn model_by_key(key: &str) -> Option<&'static TransModel> {
    MODELS.iter().find(|m| m.key == key)
}

/// 挑选翻译模型:`selected` 不是 "auto" 时尊重用户手动指定;否则按 `source_lang`
/// 精确匹配某个模型的 `langs`,匹配不到就落到 `langs` 含 `"*"` 的兜底模型。
pub fn pick_model(source_lang: &str, selected: &str) -> &'static TransModel {
    if selected != "auto" {
        if let Some(m) = model_by_key(selected) {
            return m;
        }
    }
    let lang = source_lang.to_ascii_lowercase();
    MODELS
        .iter()
        .find(|m| m.langs.contains(&lang.as_str()))
        .unwrap_or_else(|| {
            MODELS
                .iter()
                .find(|m| m.langs.contains(&"*"))
                .expect("MODELS 中必须有一个兜底(langs 含 \"*\")模型")
        })
}

/// llama-server 子进程句柄。批次内同一个模型只启动一次、复用处理多个文件;
/// 切换模型或批次结束时显式 stop()。Drop 里兜底 kill,避免异常路径下留下孤儿进程。
pub struct LlamaServer {
    child: std::process::Child,
    port: u16,
    pub model_key: &'static str,
    /// 是否探测到可用的 GPU 设备(见 `probe_gpu_available`)。识别阶段(engine.rs)有 GPU
    /// 检测,翻译阶段之前完全没有——如果翻译意外掉到纯 CPU,用户只会感觉"很慢",没有任何
    /// 界面提示指向真正原因。
    used_gpu: bool,
}

impl LlamaServer {
    pub fn start(model: &'static TransModel, cancel: &Arc<AtomicBool>) -> Result<Self, String> {
        let exe = paths::llama_server_exe().ok_or("找不到 llama-server.exe,请先完成组件下载")?;
        let model_path = paths::trans_model_path(model.file_name);
        if !model_path.is_file() {
            return Err(format!("翻译模型文件不存在: {}", model_path.display()));
        }

        // llama-server 需要 cudart64_12.dll / cublas64_12.dll / cublasLt64_12.dll 才能用 GPU。
        // 不单独下载一份约 400MB 的 CUDA 运行库包 —— whisper 引擎(同为 CUDA 12.4 构建)已经带了
        // 这些 DLL,把它所在目录加进子进程的 PATH 前部,Windows 动态链接器就能找到,免掉重复下载。
        let dll_dir = paths::whisper_cli_exe().and_then(|p| p.parent().map(|p| p.to_path_buf()));

        // 先用 --list-devices 单独探测一次 GPU 是否可用,再正式启动服务进程——见
        // probe_gpu_available 的文档注释,这是踩过真实的坑之后换掉的方案。
        let used_gpu = probe_gpu_available(&exe, dll_dir.as_deref());

        let mut cmd = Command::new(&exe);
        cmd.arg("-m").arg(&model_path);
        cmd.arg("--host").arg("127.0.0.1");
        cmd.arg("--port").arg(PORT.to_string());
        cmd.arg("-ngl").arg("99");
        cmd.arg("-c").arg("8192");
        cmd.arg("--jinja");
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        if let Some(dir) = &dll_dir {
            let existing_path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{};{existing_path}", dir.display()));
        }

        #[cfg(windows)]
        cmd.creation_flags(engine::CREATE_NO_WINDOW);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("无法启动本地翻译引擎: {e}"))?;
        crate::job_guard::bind_child_to_job(&child);

        // 排空 stdout/stderr(不解析,只为防止管道缓冲区打满导致子进程阻塞写入而卡死)。
        // GPU 检测已经在上面用 --list-devices 单独做完,不再依赖这里的日志内容。
        if let Some(s) = child.stdout.take() {
            std::thread::spawn(move || {
                for _ in BufReader::new(s).lines().map_while(Result::ok) {}
            });
        }
        if let Some(s) = child.stderr.take() {
            std::thread::spawn(move || {
                for _ in BufReader::new(s).lines().map_while(Result::ok) {}
            });
        }

        let health_client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| format!("创建健康检查客户端失败: {e}"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("已取消".to_string());
            }
            if let Ok(Some(status)) = child.try_wait() {
                // 缺 DLL 时 Windows 常见退出码是 0xC0000135(STATUS_DLL_NOT_FOUND)
                return Err(format!(
                    "本地翻译引擎启动后提前退出(退出码 {:?}),可能是显存不足、端口 {PORT} 被占用,\
                     或缺少 CUDA 运行库(需要 whisper 引擎目录里的 cudart64_12.dll 等文件存在)",
                    status.code()
                ));
            }
            if let Ok(resp) = health_client
                .get(format!("http://127.0.0.1:{PORT}/health"))
                .send()
            {
                if resp.status().is_success() {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("本地翻译引擎启动超时(180秒)".to_string());
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }

        Ok(Self {
            child,
            port: PORT,
            model_key: model.key,
            used_gpu,
        })
    }

    pub fn used_gpu(&self) -> bool {
        self.used_gpu
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub enum TranslateResult {
    /// `failed_indices` 是翻译失败、原样保留原文的字幕块下标(0-based,对应 `parse_srt_blocks`
    /// 返回的顺序),不只是一个数量——界面据此能告诉用户具体是第几条,而不是让用户对着几百
    /// 条字幕从头找起。
    Written { failed_indices: Vec<usize> },
    Skipped(String),
}

/// 读取 `original_srt`,逐批翻译,按 `bilingual` 写出 `Video.zh.srt`(中文在上、原文在下,
/// 或仅中文)。`overwrite=false` 且目标已存在时返回 Skipped,不覆盖(与 engine::place_sidecar_srt
/// 的跳过语义保持一致)。
#[allow(clippy::too_many_arguments)]
pub fn translate_srt(
    server: &LlamaServer,
    model: &'static TransModel,
    original_srt: &Path,
    video_path: &Path,
    bilingual: bool,
    overwrite: bool,
    cancel: &Arc<AtomicBool>,
    mut progress_cb: impl FnMut(String, Option<f32>),
) -> Result<TranslateResult, String> {
    let target = paths::sidecar_srt_lang_path(video_path, "zh");
    if target.exists() && !overwrite {
        // 只显示文件名,不显示完整路径,和 engine.rs::place_sidecar_srt 的跳过消息保持
        // 同样的简洁度——裸露一整条绝对路径又长又没必要。
        let filename = target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        return Ok(TranslateResult::Skipped(format!(
            "已有同名中文字幕,未覆盖({filename})"
        )));
    }

    let content =
        std::fs::read_to_string(original_srt).map_err(|e| format!("读取原文字幕失败: {e}"))?;
    let blocks = engine::parse_srt_blocks(&content);
    if blocks.is_empty() {
        // 没有对白(空字幕)是合理结果,直接写一份同样为空的中文字幕,不算失败
        std::fs::write(&target, "").map_err(|e| format!("写入字幕文件失败: {e}"))?;
        return Ok(TranslateResult::Written {
            failed_indices: Vec::new(),
        });
    }

    let total = blocks.len();
    let mut translated: Vec<String> = Vec::with_capacity(total);
    let mut failed_indices: Vec<usize> = Vec::new();
    let mut done = 0usize;
    // 上一批最后几行(原文, 译文)——现在每批是独立翻译的,人称/称呼/术语在批边界处经常
    // 漂移(比如上一批刚确定用"她",这一批又变回泛指)。把上一批的结尾喂给下一批当参考
    // 上下文(不要求模型翻译或复述它),是这类问题里最便宜也最有效的缓解办法,不需要改动
    // 批量大小或对齐解析逻辑。
    let mut prev_tail: Vec<(String, String)> = Vec::new();

    for chunk in blocks.chunks(BATCH_SIZE) {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".to_string());
        }
        let texts: Vec<String> = chunk.iter().map(|(_, _, t)| t.clone()).collect();
        let (results, failed) =
            translate_batch_with_retry(server, model, &texts, cancel, &prev_tail)?;

        prev_tail = compute_context_tail(&texts, &results, &failed);

        // failed 是这一批内部(0-based)的下标,加上 done(这一批开始前已经处理过的行数)
        // 才是相对整份字幕的全局下标。
        failed_indices.extend(failed.iter().map(|&i| done + i));
        translated.extend(results);
        done += chunk.len();
        progress_cb(
            "正在翻译字幕...".to_string(),
            Some((done as f32 / total as f32) * 100.0),
        );
    }

    let out_blocks = build_output_blocks(&blocks, &translated, &failed_indices, bilingual);

    std::fs::write(&target, engine::write_srt_blocks(&out_blocks))
        .map_err(|e| format!("写入字幕文件失败: {e}"))?;

    Ok(TranslateResult::Written { failed_indices })
}

/// 从这一批的 (原文, 译文) 和失败下标里,取最后(最多)2 行"成功翻译"的 pair,供下一批
/// 当上下文参考(见 build_prompt 里 context 参数的说明)。跳过失败行——失败行的"译文"
/// 就是原文的拷贝(见 translate_batch_with_retry 的单行兜底),拿去当"已确定的译文"
/// 参考只会把这种错误带进下一批。抽成独立函数是因为这段"跳过失败下标"的逻辑本身就是
/// 容易出 off-by-one 错误的地方,不该依赖真实的 llama-server 网络调用才能测到。
fn compute_context_tail(
    texts: &[String],
    results: &[String],
    failed_indices: &[usize],
) -> Vec<(String, String)> {
    let failed_set: std::collections::HashSet<usize> = failed_indices.iter().copied().collect();
    let successful_pairs: Vec<(String, String)> = texts
        .iter()
        .cloned()
        .zip(results.iter().cloned())
        .enumerate()
        .filter(|(i, _)| !failed_set.contains(i))
        .map(|(_, pair)| pair)
        .collect();
    let tail_start = successful_pairs.len().saturating_sub(2);
    successful_pairs[tail_start..].to_vec()
}

/// 把原文块和翻译结果拼成最终要写出的字幕块。抽成独立函数是因为这段逻辑不该依赖真实的
/// llama-server 网络调用就能被测到——翻译失败行的 `translated` 本身就是原文的拷贝(见
/// `translate_batch_with_retry` 的单行兜底),双语模式下不能再按正常行的逻辑拼一遍原文,
/// 否则同一句话会重复输出两遍(`{orig}\n{orig}`)——失败行不管双语与否都只输出一遍原文。
fn build_output_blocks(
    blocks: &[(String, String, String)],
    translated: &[String],
    failed_indices: &[usize],
    bilingual: bool,
) -> Vec<(String, String, String)> {
    let failed_set: std::collections::HashSet<usize> = failed_indices.iter().copied().collect();
    blocks
        .iter()
        .zip(translated.iter())
        .enumerate()
        .map(|(i, ((start, end, orig), zh))| {
            let text = if failed_set.contains(&i) {
                orig.clone()
            } else if bilingual {
                format!("{zh}\n{orig}")
            } else {
                zh.clone()
            };
            (start.clone(), end.clone(), text)
        })
        .collect()
}

/// 翻译一批字幕行,失败/行数错位时递归对半重试,缩小出错范围;单行仍失败则原样保留该行
/// (计入失败下标,不让整个任务失败——用户仍然拿到完整的字幕,只是个别行没翻译)。
///
/// 返回值第二项是失败行在 `texts` 里的下标(0-based),不是单纯的计数——上层
/// `translate_srt` 需要知道具体是哪几行,才能在双语模式下避免对失败行重复拼接原文,
/// 也能在界面警告里告诉用户具体第几条需要人工核对。
/// `context` 是给模型看的"上一批结尾"参考(见 translate_srt),对半重试时原样传给两个
/// 子调用——重试是格式错位这种偶发情况的兜底路径,没必要为它精细计算"这一半真正紧邻的
/// 上文",用同一个批级上下文对两半都总比完全没有上下文好。
fn translate_batch_with_retry(
    server: &LlamaServer,
    model: &'static TransModel,
    texts: &[String],
    cancel: &Arc<AtomicBool>,
    context: &[(String, String)],
) -> Result<(Vec<String>, Vec<usize>), String> {
    if texts.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".to_string());
    }
    match translate_batch_once(server, model, texts, context) {
        Ok(results) if results.len() == texts.len() => Ok((results, Vec::new())),
        _ if texts.len() == 1 => Ok((vec![texts[0].clone()], vec![0])),
        _ => {
            let mid = texts.len() / 2;
            let (mut out, mut failed) =
                translate_batch_with_retry(server, model, &texts[..mid], cancel, context)?;
            let (b, fb) =
                translate_batch_with_retry(server, model, &texts[mid..], cancel, context)?;
            out.extend(b);
            // fb 是后半段(texts[mid..])内部的下标,加上 mid 才是相对整个 texts 的下标。
            failed.extend(fb.iter().map(|&i| i + mid));
            Ok((out, failed))
        }
    }
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Serialize)]
struct ChatRequest {
    model: &'static str,
    messages: Vec<ChatMessage>,
    temperature: f32,
    max_tokens: u32,
    /// llama-server 支持的 KV 缓存复用开关。system prompt 在同一批次内的每次请求里都一样
    /// (见 build_prompt),开启后 llama-server 能复用之前请求已经算过的那部分 KV 缓存,
    /// 不用每次都从头重新计算,是白捡的速度,不影响翻译结果。
    cache_prompt: bool,
}

fn translate_batch_once(
    server: &LlamaServer,
    model: &'static TransModel,
    texts: &[String],
    context: &[(String, String)],
) -> Result<Vec<String>, String> {
    let numbered: String = texts
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}. {}", i + 1, t.replace('\n', " ")))
        .collect::<Vec<_>>()
        .join("\n");

    let (system, user) = build_prompt(model, &numbered, context);
    let body = ChatRequest {
        model: model.key,
        messages: vec![
            ChatMessage {
                role: "system",
                content: system,
            },
            ChatMessage {
                role: "user",
                content: user,
            },
        ],
        temperature: 0.2,
        max_tokens: (texts.len() as u32 * 100 + 200).min(4096),
        cache_prompt: true,
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("创建翻译请求客户端失败: {e}"))?;
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/chat/completions", server.port))
        .json(&body)
        .send()
        .map_err(|e| format!("翻译请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("翻译请求失败: HTTP {}", resp.status()));
    }
    let json: serde_json::Value = resp
        .json()
        .map_err(|e| format!("解析翻译响应失败: {e}"))?;
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("翻译响应格式异常(缺少 choices[0].message.content 字段)")?;

    parse_numbered_response(content, texts.len())
}

/// 提示词按模型的 PromptKind 区分:
/// - Sakura 官方推荐提示词是单段文本(不带编号),这里为了能批量翻译多行字幕、又要保证
///   行与行对齐,在官方提示词基础上追加了"保留编号"的指令 —— 与官方单行格式略有出入,
///   是吞吐量(不然一集字幕要发几百次请求)和对齐可靠性之间的权衡,v1 先这样,
///   如果实测发现 Sakura 在编号格式下翻译质量明显下降,再改成小批量(如 3~5 行一批)。
/// - General(Qwen)本来就是通用指令模型,直接要求保留编号格式即可。
///
/// `context` 是上一批最后几行的(原文, 译文),不空时拼进 user 消息最前面,给模型一个
/// "紧接在前的上文"参考——现在每批是独立翻译的,人称代词、称呼、专有名词在批边界处
/// 经常漂移(比如上一批刚确定某角色用"她",下一批又变回中性称呼),喂一点上文能让模型
/// 在批与批之间保持一致,不需要为此改动批量大小或编号对齐解析逻辑(上下文只是参考信息,
/// 不算进正文编号,parse_numbered_response 完全不受影响)。明确要求模型不要把这段上下文
/// 也翻译/复述出来,避免它把参考内容当成本批正文的一部分。
fn build_prompt(model: &TransModel, numbered: &str, context: &[(String, String)]) -> (String, String) {
    let context_block = if context.is_empty() {
        String::new()
    } else {
        let lines: String = context
            .iter()
            .map(|(orig, zh)| format!("{orig} → {zh}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "(以下是紧接在前的上文原文及其译文,仅供参考、用来保持人称和称呼前后一致,\
             不要翻译它、也不要在你的回复里重复输出它:\n{lines}\n参考上文到此结束,\
             正式要翻译的正文从下一行开始:)\n"
        )
    };

    match model.prompt_kind {
        PromptKind::Sakura => (
            "你是一个轻小说翻译模型，可以流畅通顺地以日本轻小说的风格将日文翻译成简体中文，\
             并联系上下文正确使用人称代词，不擅自添加原文中没有的代词。\n\
             输入是带编号的多行日文字幕，请严格逐行翻译，保留原有的“编号.”格式，每行只输出一条\
             译文，不要合并、拆分、增删行，不要输出编号之外的任何解释。"
                .to_string(),
            format!("{context_block}将下面的日文字幕文本翻译成中文：\n{numbered}"),
        ),
        PromptKind::General => (
            "你是专业的字幕翻译员。将用户给出的带编号字幕文本翻译成简体中文。要求:逐行对应翻译,\
             严格保留原有的“编号.”格式,每行只输出一条译文,不要合并或拆分行,不要添加编号之外的\
             任何解释、注释或原文。"
                .to_string(),
            format!("{context_block}{numbered}"),
        ),
    }
}

/// 解析模型输出里的编号行(如 "3. 译文" / "3、译文" / "3) 译文"),返回 编号→译文 映射。
/// 行数或编号连续性对不上时返回 Err,交给上层 translate_batch_with_retry 拆小重试。
fn parse_numbered_response(content: &str, expected: usize) -> Result<Vec<String>, String> {
    let mut map: std::collections::BTreeMap<usize, String> = std::collections::BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((num, rest)) = split_leading_number(line) {
            map.insert(num, rest.to_string());
        }
    }
    if map.len() != expected {
        return Err(format!(
            "翻译输出行数不匹配(期望 {expected},实际 {})",
            map.len()
        ));
    }
    let mut out = Vec::with_capacity(expected);
    for i in 1..=expected {
        match map.get(&i) {
            Some(t) => out.push(t.clone()),
            None => return Err(format!("翻译输出缺少第 {i} 行")),
        }
    }
    Ok(out)
}

fn split_leading_number(line: &str) -> Option<(usize, &str)> {
    let digits_len = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_len == 0 {
        return None;
    }
    let (num_str, after_digits) = line.split_at(digits_len);
    let num: usize = num_str.parse().ok()?;
    let after_sep = after_digits
        .strip_prefix('.')
        .or_else(|| after_digits.strip_prefix('、'))
        .or_else(|| after_digits.strip_prefix(')'))
        .or_else(|| after_digits.strip_prefix('）'))?;
    Some((num, after_sep.trim_start()))
}

/// 探测这台机器上 llama-server 能不能找到可用的 GPU 设备。用 `--list-devices` 单独跑
/// 一次(不加载模型,进程打印设备列表就退出,很快)取代早先"解析主进程 stderr 里的层
/// 卸载日志行"的方案——见下面的踩坑记录。`-ngl 99` 在这个项目里恒定拉满,所以"探测到
/// 设备可用"约等于"这次启动会用上 GPU"。
///
/// 真实踩过的坑:原方案解析形如 "load_tensors: offloaded 37/37 layers to GPU" 的日志行,
/// 格式号称来自 llama.cpp b10079 源码(`load_tensors()` 里的 `LLAMA_LOG_INFO`)实测确认。
/// 但用真机(RTX 3060)跑一次完整流程后用 `nvidia-smi` 独立监控显存,启动前基线约
/// 2260 MiB、加载 2.4GB 的 Qwen3 模型后约 5988 MiB,增量约 3.7GB,证明 GPU 确实被用上了;
/// 与此同时把这次启动的 stdout(0 行)和 stderr(11 行,全文读完)都检查了一遍,完全没有
/// 任何包含 offload/CUDA/cuda/device 的行——这个项目实际绑定的 llama-server.exe
/// (build b10079-40b740ad0)在这次真实运行里根本不打印那一行日志。旧方案在这个真实
/// 场景下会稳定得到假阴性:GPU 明明工作正常,却告诉用户"未能使用显卡加速",比不做检测
/// 还糟糕。
///
/// `--list-devices` 这条路径已经用同一台机器实测确认过两种情况:PATH 里有 CUDA DLL 时
/// 输出 "Available devices:\n  CUDA0: NVIDIA GeForce RTX 3060 (12287 MiB, 11255 MiB free)";
/// PATH 里没有时只输出光秃秃的 "Available devices:" 一行,没有设备行。这个探测依赖的是
/// "进程能不能枚举到设备"这个更底层的事实,不锚定某一条具体日志字符串的措辞,不会随
/// llama-server 版本更新就失效。
fn probe_gpu_available(exe: &Path, dll_dir: Option<&Path>) -> bool {
    let mut cmd = Command::new(exe);
    cmd.arg("--list-devices");
    if let Some(dir) = dll_dir {
        let existing_path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{};{existing_path}", dir.display()));
    }
    #[cfg(windows)]
    cmd.creation_flags(engine::CREATE_NO_WINDOW);
    let Ok(output) = cmd.output() else {
        return false;
    };
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_device_list(&combined)
}

/// 判断 `--list-devices` 的输出里是否列出了至少一个设备。设备行形如
/// "  CUDA0: NVIDIA GeForce RTX 3060 (12287 MiB, 11255 MiB free)"——去掉首尾空白后,
/// 冒号前是一段不含空白的设备名;没有设备时只有标题行 "Available devices:"。不锚定
/// "CUDA" 这个具体后端名字,是因为同一套预编译包理论上也可能报告 Vulkan/SYCL 等其它
/// 后端的设备,解析逻辑不该绑定某一个后端。
fn parse_device_list(output: &str) -> bool {
    output.lines().any(|line| {
        let trimmed = line.trim();
        match trimmed.split_once(':') {
            Some((name, _)) => !name.is_empty() && !name.contains(char::is_whitespace),
            None => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_context_tail_takes_last_two_successful_pairs() {
        let texts = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let results = vec!["甲".to_string(), "乙".to_string(), "丙".to_string()];
        let tail = compute_context_tail(&texts, &results, &[]);
        assert_eq!(
            tail,
            vec![("B".to_string(), "乙".to_string()), ("C".to_string(), "丙".to_string())]
        );
    }

    #[test]
    fn compute_context_tail_skips_failed_lines() {
        // 第 2 行(下标 1)失败,results[1] 是原文的拷贝——不该被当成"已确定的译文"带进
        // 下一批上下文,真正该取的是跳过它之后剩下的最后两行成功翻译。
        let texts = vec!["A".to_string(), "B".to_string(), "C".to_string(), "D".to_string()];
        let results = vec![
            "甲".to_string(),
            "B".to_string(), // 失败,原样保留原文
            "丙".to_string(),
            "丁".to_string(),
        ];
        let tail = compute_context_tail(&texts, &results, &[1]);
        assert_eq!(
            tail,
            vec![("C".to_string(), "丙".to_string()), ("D".to_string(), "丁".to_string())],
            "应该跳过失败的第 2 行,只取成功翻译里最后两行: {tail:?}"
        );
    }

    #[test]
    fn compute_context_tail_returns_fewer_than_two_when_not_enough_successful_lines() {
        let texts = vec!["A".to_string()];
        let results = vec!["甲".to_string()];
        let tail = compute_context_tail(&texts, &results, &[]);
        assert_eq!(tail, vec![("A".to_string(), "甲".to_string())]);

        let texts_all_failed = vec!["A".to_string(), "B".to_string()];
        let results_all_failed = vec!["A".to_string(), "B".to_string()];
        let tail_empty = compute_context_tail(&texts_all_failed, &results_all_failed, &[0, 1]);
        assert!(tail_empty.is_empty(), "全部失败时不该有任何上下文可用: {tail_empty:?}");
    }

    #[test]
    fn build_prompt_without_context_has_no_reference_block() {
        let (_, user) = build_prompt(&QWEN3_4B, "1. Hello", &[]);
        assert_eq!(user, "1. Hello", "没有上下文时正文不该被额外包装");
        assert!(!user.contains("紧接在前的上文"));
    }

    #[test]
    fn build_prompt_with_context_prepends_reference_block_before_body() {
        let context = vec![("Hello there.".to_string(), "你好。".to_string())];
        let (_, user) = build_prompt(&QWEN3_4B, "1. World", &context);
        assert!(user.contains("紧接在前的上文"), "应该包含上下文说明: {user}");
        assert!(user.contains("Hello there. → 你好。"), "应该包含具体的上文内容: {user}");
        let context_pos = user.find("Hello there.").unwrap();
        let body_pos = user.find("1. World").unwrap();
        assert!(context_pos < body_pos, "上下文应该在正文之前: {user}");
    }

    #[test]
    fn build_prompt_sakura_also_gets_context_block() {
        // Sakura 和 General 是两套不同的提示词模板,上下文注入要对两边都生效,不能只改
        // 其中一个分支就当作做完了。
        let context = vec![("こんにちは".to_string(), "你好".to_string())];
        let (system, user) = build_prompt(&SAKURA_7B, "1. さようなら", &context);
        assert!(user.contains("こんにちは → 你好"));
        assert!(user.contains("将下面的日文字幕文本翻译成中文"));
        assert!(system.contains("轻小说"));
    }

    /// 输出格式来自真机(RTX 3060)实测的 `llama-server.exe --list-devices`(见
    /// probe_gpu_available 文档注释),不是猜测的。
    #[test]
    fn parse_device_list_detects_cuda_device() {
        let output = "Available devices:\n  CUDA0: NVIDIA GeForce RTX 3060 (12287 MiB, 11255 MiB free)\n";
        assert!(parse_device_list(output));
    }

    #[test]
    fn parse_device_list_no_devices_returns_false() {
        // 真机实测:PATH 里没有 CUDA DLL 时,输出只有光秃秃的标题行,没有设备行。
        let output = "Available devices:\n";
        assert!(!parse_device_list(output));
    }

    #[test]
    fn parse_device_list_empty_output_returns_false() {
        assert!(!parse_device_list(""));
    }

    #[test]
    fn parse_device_list_does_not_match_title_line_itself() {
        // "Available devices:" 本身也含冒号,但冒号前是"Available devices"(含空格),
        // 不该被误判成一个设备名。
        assert!(!parse_device_list("Available devices:"));
    }

    /// 真实踩过的坑:失败行的 `translated` 值就是原文本身(见 translate_batch_with_retry
    /// 的单行兜底),按正常行的逻辑拼接会变成"原文\n原文",双语字幕里同一句话重复两遍。
    #[test]
    fn build_output_blocks_does_not_duplicate_original_text_for_failed_lines() {
        let blocks = vec![
            ("00:00:01,000".to_string(), "00:00:02,000".to_string(), "Hello".to_string()),
            ("00:00:02,000".to_string(), "00:00:03,000".to_string(), "World".to_string()),
        ];
        // 第 0 行翻译成功;第 1 行失败,translated[1] 是原文的拷贝(和真实失败路径一致)
        let translated = vec!["你好".to_string(), "World".to_string()];
        let out = build_output_blocks(&blocks, &translated, &[1], true);
        assert_eq!(out[0].2, "你好\nHello");
        assert_eq!(out[1].2, "World", "失败行双语模式下不应该重复输出原文");
    }

    #[test]
    fn build_output_blocks_success_lines_respect_bilingual_flag() {
        let blocks = vec![("00:00:01,000".to_string(), "00:00:02,000".to_string(), "Hello".to_string())];
        let translated = vec!["你好".to_string()];

        let bilingual = build_output_blocks(&blocks, &translated, &[], true);
        assert_eq!(bilingual[0].2, "你好\nHello");

        let zh_only = build_output_blocks(&blocks, &translated, &[], false);
        assert_eq!(zh_only[0].2, "你好");
    }

    #[test]
    fn parses_well_formed_numbered_response() {
        let content = "1. 你好\n2. 世界\n3. 再见";
        let out = parse_numbered_response(content, 3).unwrap();
        assert_eq!(out, vec!["你好", "世界", "再见"]);
    }

    #[test]
    fn accepts_fullwidth_and_alternate_separators() {
        let content = "1、第一行\n2）第二行\n3. 第三行";
        let out = parse_numbered_response(content, 3).unwrap();
        assert_eq!(out, vec!["第一行", "第二行", "第三行"]);
    }

    #[test]
    fn rejects_mismatched_line_count() {
        let content = "1. 只有一行";
        assert!(parse_numbered_response(content, 3).is_err());
    }

    /// 真实数据回归测试:这段内容是开发时用真实 Sakura-7B-Qwen2.5-v1.0(iq4xs 量化)模型跑
    /// llama-server 实测出来的原始响应(15 行,批大小和生产环境的 BATCH_SIZE 一致),不是
    /// 手写的理想化样例——用来确认真实模型输出里的全角标点(。！？，)、混合中英文标点
    /// 都能被正确解析、编号连续 1~15 不缺行。同一次实测也确认了两件事(不在本测试里体现,
    /// 记录在此供后续参照):llama-server 通过复用 whisper 引擎目录里的 CUDA 12.4 DLL 成功用
    /// 上了 GPU(生成速度约 69 token/s,3060 上纯 CPU 跑 7B 模型不可能有这个速度),且批量编号
    /// 提示词格式确实按预期让 Sakura 严格保留了编号、没有合并或跳过任何一行。
    #[test]
    fn parses_real_sakura_batch_response() {
        let content = "1. 早安。\n2. 今天天气真好。\n3. 一起去上学吧。\n4. 肚子饿了。\n\
                        5. 去吃点东西吧。\n6. 我觉得这是个好主意。\n7. 那间拉面店怎么样？\n\
                        8. 好像很好吃！\n9. 我也想去。\n10. 那就决定咯。\n\
                        11. 先问过老师之后再去吧。\n12. 好啊，毕竟迟到就糟了。\n\
                        13. 放学后再集合吧。\n14. 我很期待。\n15. 嗯，待会儿见。";
        let out = parse_numbered_response(content, 15).unwrap();
        assert_eq!(out.len(), 15);
        assert_eq!(out[0], "早安。");
        assert_eq!(out[6], "那间拉面店怎么样？");
        assert_eq!(out[11], "好啊，毕竟迟到就糟了。");
        assert_eq!(out[14], "嗯，待会儿见。");
    }

    #[test]
    fn rejects_missing_middle_line() {
        // 有 2 条内容但编号是 1 和 3,中间缺 2 —— 长度凑巧对上但不能被接受
        let content = "1. 第一行\n3. 第三行";
        assert!(parse_numbered_response(content, 2).is_err());
    }

    #[test]
    fn pick_model_matches_japanese_to_sakura() {
        let m = pick_model("ja", "auto");
        assert_eq!(m.key, "sakura-7b");
    }

    #[test]
    fn pick_model_falls_back_to_catchall_for_other_languages() {
        let m = pick_model("en", "auto");
        assert_eq!(m.key, "qwen3-4b");
    }

    #[test]
    fn pick_model_respects_manual_selection() {
        let m = pick_model("ja", "qwen3-4b");
        assert_eq!(m.key, "qwen3-4b");
    }
}
