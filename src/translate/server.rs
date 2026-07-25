use super::TransModel;
use crate::engine;
use crate::paths;
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

/// llama-server 子进程句柄。批次内同一个模型只启动一次、复用处理多个文件;
/// 切换模型或批次结束时显式 stop()。Drop 里兜底 kill,避免异常路径下留下孤儿进程。
pub struct LlamaServer {
    child: std::process::Child,
    pub(crate) port: u16,
    pub model_key: &'static str,
    /// 是否探测到可用的 GPU 设备(见 `probe_gpu_available`)。识别阶段(engine.rs)有 GPU
    /// 检测,翻译阶段之前完全没有——如果翻译意外掉到纯 CPU,用户只会感觉"很慢",没有任何
    /// 界面提示指向真正原因。
    used_gpu: bool,
}

impl LlamaServer {
    pub fn start(model: &'static TransModel, cancel: &Arc<AtomicBool>) -> Result<Self, String> {
        let exe = paths::llama_server_exe().ok_or("找不到 llama-server.exe,请先完成组件下载")?;
        let model_path = paths::trans_model_path(model.key, model.file_name);
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
                && resp.status().is_success() {
                    break;
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
}
