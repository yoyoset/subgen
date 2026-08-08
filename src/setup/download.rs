use crate::paths;
use crate::setup::SetupEvent;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

// GitHub Release 资产的反向代理前缀,面向 GitHub 直连不稳定的网络环境(比如国内)。实测
// 验证过(2026-07-23):对 GyanD/codexffmpeg 的 ffmpeg 资产发 Range 请求,能正确返回
// 206 Partial Content 和准确的 Content-Range,和官方源行为一致,支持断点续传。这类反代
// 服务本身不算长期稳定,失效时把这一个常量换掉就行,不用改调用逻辑。
const GITHUB_PROXY_PREFIX: &str = "https://gh-proxy.com/";

pub(super) fn sha256_file(path: &Path) -> Result<[u8; 32], String> {
    let never_cancel = Arc::new(AtomicBool::new(false));
    sha256_file_cancelable(path, &never_cancel)
}

/// 和 `sha256_file` 一样逐块算哈希,但每块之间检查一次取消标志——"定位…"手动绑定一个
/// 几 GB 的文件时,哈希要算几十秒,用户选错文件想反悔不该被迫等完。`sha256_file` 内部
/// 复用这个函数(传一个永远不会被置位的标志),避免同一段循环写两遍。
pub(crate) fn sha256_file_cancelable(path: &Path, cancel: &Arc<AtomicBool>) -> Result<[u8; 32], String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".to_string());
        }
        let n = file.read(&mut buf).map_err(|e| format!("读取文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}


pub(super) fn download_file(
    label: &str,
    url: &str,
    sha256_hex: Option<&str>,
    target: &Path,
    tx: &Sender<SetupEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let tmp_path = target.with_extension("part");
    let digest = download_to_file(label, url, &tmp_path, tx, cancel)?;
    verify_hash_or_warn(label, &digest, sha256_hex, &tmp_path)?;

    std::fs::rename(&tmp_path, target).map_err(|e| format!("安装 {label} 失败: {e}"))?;
    let _ = tx.send(SetupEvent::ComponentReady(label.to_string()));
    Ok(())
}

pub(super) fn download_and_extract_zip(
    label: &str,
    url: &str,
    sha256_hex: Option<&str>,
    extract_dir: &Path,
    tx: &Sender<SetupEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    std::fs::create_dir_all(extract_dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let tmp_zip = paths::app_dir().join(format!("_dl_{}.zip.part", sanitize(label)));
    let digest = download_to_file(label, url, &tmp_zip, tx, cancel)?;
    verify_hash_or_warn(label, &digest, sha256_hex, &tmp_zip)?;

    let _ = tx.send(SetupEvent::Log(format!("正在解压 {label} ...")));
    let file = std::fs::File::open(&tmp_zip).map_err(|e| format!("打开压缩包失败: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("{label} 不是有效的压缩包: {e}"))?;
    archive
        .extract(extract_dir)
        .map_err(|e| format!("解压 {label} 失败: {e}"))?;
    let _ = std::fs::remove_file(&tmp_zip);
    Ok(())
}

/// 校验下载文件的哈希,不匹配就删除文件并返回错误(阻断安装)。
/// 所有下载都必须通过强校验,没有"哈希对不上也继续用"的降级路径——那是拿完整性换
/// 便利,一旦命中就是拿到一个错的文件。
fn verify_hash_or_warn(
    label: &str,
    digest: &[u8; 32],
    expected_hex: Option<&str>,
    file_to_remove_on_fail: &Path,
) -> Result<(), String> {
    if let Some(expected) = expected_hex {
        let actual = to_hex(digest);
        if !actual.eq_ignore_ascii_case(expected) {
            let msg = format!("{label} 文件校验哈希不一致(期望 {expected}, 实际 {actual})");
            let _ = std::fs::remove_file(file_to_remove_on_fail);
            return Err(msg);
        }
    }
    Ok(())
}

/// 补算 `.part` 里已有内容的哈希(hasher 是流式的,续传不能跳过中间直接从续传点开始算,
/// 不然最终哈希和完整文件对不上)。返回补算过的 hasher 和已有字节数(也就是续传 Range
/// 请求的起点)。文件不存在或读取中途出错时返回全新的 hasher 和 0——出错时当作没有可用的
/// 续传内容更安全,退化成从头下载,不会因为半吊子的部分文件产生错误的哈希结果。
fn hash_existing(path: &Path) -> (Sha256, u64) {
    let mut hasher = Sha256::new();
    let Ok(mut file) = std::fs::File::open(path) else {
        return (Sha256::new(), 0);
    };
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                total += n as u64;
            }
            Err(_) => return (Sha256::new(), 0),
        }
    }
    (hasher, total)
}

/// 返回按优先级排列的候选下载地址:官方地址永远排第一(镜像稳定性不如官方,能直连就
/// 不该绕路),官方多次失败后才尝试镜像。只覆盖 huggingface.co 和 github.com 这两个在部分
/// 网络环境(比如国内)下经常连不上的域名:
/// - HuggingFace:换成 hf-mirror.com,只替换域名、路径不变——这是国内 ML 社区公认的标准
///   HuggingFace 镜像方案。
/// - GitHub:整个 URL 前面加 GITHUB_PROXY_PREFIX 反代前缀。
///
/// 其他域名(目前只有 ffmpeg 用的 GitHub,已经在上面覆盖)原样返回,不额外处理。
fn url_candidates(url: &str) -> Vec<String> {
    let mut out = vec![url.to_string()];
    if let Some(rest) = url.strip_prefix("https://huggingface.co") {
        out.push(format!("https://hf-mirror.com{rest}"));
    } else if url.starts_with("https://github.com") {
        out.push(format!("{GITHUB_PROXY_PREFIX}{url}"));
    }
    out
}

/// 流式下载到临时文件,边下载边计算 sha256,定期上报进度,支持取消、断点续传、网络抖动
/// 重试、镜像回退。
///
/// 大文件(几个 GB)在家用网络上中途断开是首次安装失败率最大的来源;之前每次失败都从零
/// 重新下载,一个 4GB 文件网络稍微抖一下就前功尽弃。现在 `.part` 临时文件在下载中断(不是
/// 用户主动取消)后不会被删除,下次尝试会先读取已有内容补算哈希,再用 HTTP Range 请求从
/// 断点续传——即使切换到镜像地址也一样能续传(镜像和官方源理论上是同一份文件的字节级
/// 拷贝,最终都要通过哈希校验,续传安全)。每个候选地址内读流中途出错会原地重试几次;
/// 官方地址试完还失败,才切换到镜像地址重新走一遍同样的重试流程。
fn download_to_file(
    label: &str,
    url: &str,
    tmp_path: &Path,
    tx: &Sender<SetupEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<[u8; 32], String> {
    const ATTEMPTS_PER_URL: u32 = 2;

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(1800))
        .build()
        .map_err(|e| format!("创建下载客户端失败: {e}"))?;

    let candidates = url_candidates(url);
    let total_attempts = candidates.len() as u32 * ATTEMPTS_PER_URL;
    let mut attempt_no = 0u32;
    let mut last_err = String::new();

    for (url_index, candidate_url) in candidates.iter().enumerate() {
        if url_index > 0 {
            let _ = tx.send(SetupEvent::Log(format!(
                "{label} 官方源连接失败,已切换镜像源重试…"
            )));
        }
        for _ in 0..ATTEMPTS_PER_URL {
            attempt_no += 1;
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消下载".to_string());
            }
            match try_download_once(label, candidate_url, tmp_path, &client, tx, cancel) {
                Ok(digest) => return Ok(digest),
                Err(e) if e == "用户取消下载" => return Err(e),
                Err(e) => {
                    last_err = e;
                    if attempt_no < total_attempts {
                        let _ = tx.send(SetupEvent::Log(format!(
                            "{label} 下载中断,2 秒后重试(第 {attempt_no}/{total_attempts} 次尝试): {last_err}"
                        )));
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                }
            }
        }
    }
    Err(format!("下载 {label} 多次重试后仍失败: {last_err}"))
}

/// 单次下载尝试:可能是从头下载,也可能是从 `tmp_path` 已有内容续传。失败时返回的错误
/// 交给上层 `download_to_file` 判断要不要重试;`.part` 文件在这个函数内部任何失败路径都
/// 不会被删除(断点续传的价值就在于保留已下载的部分)。
fn try_download_once(
    label: &str,
    url: &str,
    tmp_path: &Path,
    client: &reqwest::blocking::Client,
    tx: &Sender<SetupEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<[u8; 32], String> {
    let (mut hasher, resume_from) = if tmp_path.is_file() {
        hash_existing(tmp_path)
    } else {
        (Sha256::new(), 0)
    };

    let mut request = client.get(url);
    if resume_from > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
    }
    let mut resp = request
        .send()
        .map_err(|e| format!("下载 {label} 失败(网络错误): {e}"))?;

    let (mut file, mut downloaded, total) = if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT
    {
        let _ = tx.send(SetupEvent::Log(format!(
            "{label} 从断点续传(已下载 {resume_from} 字节)..."
        )));
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp_path)
            .map_err(|e| format!("打开临时文件失败: {e}"))?;
        let total = resume_from + resp.content_length().unwrap_or(0);
        (file, resume_from, total)
    } else if resp.status().is_success() {
        // 服务器不支持 Range(直接返回 200 全量内容,不是 206)时退化成从头下载,不当错误处理。
        if resume_from > 0 {
            let _ = tx.send(SetupEvent::Log(format!(
                "{label} 服务器不支持断点续传,重新下载..."
            )));
        } else {
            let _ = tx.send(SetupEvent::Log(format!("开始下载 {label} ...")));
        }
        hasher = Sha256::new();
        let file =
            std::fs::File::create(tmp_path).map_err(|e| format!("创建临时文件失败: {e}"))?;
        let total = resp.content_length().unwrap_or(0);
        (file, 0u64, total)
    } else {
        return Err(format!("下载 {label} 失败: HTTP {}", resp.status()));
    };

    let mut buf = [0u8; 64 * 1024];
    let mut last_report = std::time::Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消下载".to_string());
        }
        let n = resp
            .read(&mut buf)
            .map_err(|e| format!("下载 {label} 中断: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入文件失败: {e}"))?;
        hasher.update(&buf[..n]);
        downloaded += n as u64;
        if last_report.elapsed().as_millis() > 150 {
            let _ = tx.send(SetupEvent::Progress {
                component: label.to_string(),
                downloaded,
                total,
            });
            last_report = std::time::Instant::now();
        }
    }
    let _ = tx.send(SetupEvent::Progress {
        component: label.to_string(),
        downloaded,
        total: total.max(downloaded),
    });

    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}


#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{VAD_SHA256, VAD_URL};
    use std::path::PathBuf;
    use std::sync::mpsc;

    /// 每个测试用独立的、带随机后缀的临时目录,避免并行跑的测试互相踩到同一个路径。
    fn temp_subdir(name: &str) -> PathBuf {
        let unique = format!(
            "{}_{:x}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join("subgen_setup_test").join(unique)
    }


    /// 断点续传正确性的核心保证:拼接续传后的文件哈希,必须和一次性写完整个文件算出的
    /// 哈希完全一致——不然下载"成功"了,校验却过不了,用户以为文件坏了实际是续传逻辑错了。
    #[test]
    fn hash_existing_matches_full_file_hash_after_resumed_write() {
        let dir = temp_subdir("hash_existing_resume");
        std::fs::create_dir_all(&dir).unwrap();
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 256) as u8).collect();
        let mid = content.len() / 2;

        let full = dir.join("full.bin");
        std::fs::write(&full, &content).unwrap();
        let (full_hasher, full_len) = hash_existing(&full);
        assert_eq!(full_len, content.len() as u64);

        // 模拟断点续传:先写前半段(第一次下载中断在这里),再用 append 补写后半段
        // (续传请求写回来的部分),中间调用 hash_existing 两次,分别验证长度和最终哈希。
        let resumed = dir.join("resumed.bin");
        std::fs::write(&resumed, &content[..mid]).unwrap();
        let (_, len_before_resume) = hash_existing(&resumed);
        assert_eq!(len_before_resume, mid as u64, "续传前应该报告已有的字节数,作为 Range 起点");

        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&resumed).unwrap();
            f.write_all(&content[mid..]).unwrap();
        }
        let (resumed_hasher, len_after_resume) = hash_existing(&resumed);
        assert_eq!(len_after_resume, content.len() as u64);
        assert_eq!(
            resumed_hasher.finalize().as_slice(),
            full_hasher.finalize().as_slice(),
            "分两段续传写完的文件,哈希必须和一次性写完的文件完全一致"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_existing_returns_zero_length_for_missing_file() {
        let missing = temp_subdir("hash_existing_missing").join("does_not_exist.bin");
        let (_, len) = hash_existing(&missing);
        assert_eq!(len, 0, "文件不存在应该当作没有可续传的内容,退化成从头下载");
    }

    #[test]
    fn url_candidates_adds_hf_mirror_for_huggingface_urls() {
        let candidates =
            url_candidates("https://huggingface.co/foo/bar/resolve/main/file.bin");
        assert_eq!(
            candidates,
            vec![
                "https://huggingface.co/foo/bar/resolve/main/file.bin".to_string(),
                "https://hf-mirror.com/foo/bar/resolve/main/file.bin".to_string(),
            ]
        );
    }

    #[test]
    fn url_candidates_adds_github_proxy_prefix_for_github_urls() {
        let original = "https://github.com/foo/bar/releases/download/v1/file.zip";
        let candidates = url_candidates(original);
        assert_eq!(
            candidates,
            vec![original.to_string(), format!("{GITHUB_PROXY_PREFIX}{original}")]
        );
    }

    #[test]
    fn url_candidates_leaves_other_domains_unchanged() {
        let candidates = url_candidates("https://www.gyan.dev/something.zip");
        assert_eq!(candidates, vec!["https://www.gyan.dev/something.zip".to_string()]);
    }

    /// 真实网络端到端验证断点续传:下载 VAD 模型(885KB,是这批组件里最小的,适合当实验
    /// 对象)前 200KB 后人为截断模拟"下载中断",再调用同一个 download_to_file 续传剩余
    /// 部分,确认最终哈希和 VAD_SHA256(GitHub/HuggingFace 官方校验值)完全一致——不只是
    /// 测 hash_existing 这个纯函数本身,而是测真实服务器(HuggingFace CDN,经过一次 302
    /// 重定向)在 Range 请求下的实际行为是否符合预期。标 #[ignore] 是因为它需要真实网络
    /// 且会产生真实流量,不该在日常 `cargo test`/CI 里跑,手动 `cargo test -- --ignored`
    /// 验证。
    #[test]
    #[ignore]
    fn download_to_file_resumes_after_simulated_interruption_against_real_server() {
        let dir = temp_subdir("resume_e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let tmp_path = dir.join("vad.part");

        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));

        // 第一次:正常下载到一半就人为掐断(不是真的网络故障,是主动截断已下载的文件来
        // 模拟"上次只下了一部分"这个断点续跑要处理的真实场景)。
        let digest1 = download_to_file("VAD(测试)", VAD_URL, &tmp_path, &tx, &cancel)
            .expect("第一次下载应该成功(用来产出一个完整文件,再截断模拟中断)");
        let full_len = std::fs::metadata(&tmp_path).unwrap().len();
        assert_eq!(to_hex(&digest1), VAD_SHA256, "完整下载一次的哈希应该本来就和官方值一致");

        let truncated_len = 200_000u64.min(full_len - 1);
        {
            let file = std::fs::OpenOptions::new().write(true).open(&tmp_path).unwrap();
            file.set_len(truncated_len).unwrap();
            drop(file);
        }
        assert_eq!(std::fs::metadata(&tmp_path).unwrap().len(), truncated_len);

        // 第二次:同一个 .part 路径,应该从 200KB 续传,而不是从头下载。
        let digest2 = download_to_file("VAD(测试)", VAD_URL, &tmp_path, &tx, &cancel)
            .expect("续传应该成功");
        assert_eq!(
            to_hex(&digest2),
            VAD_SHA256,
            "续传拼接后的文件哈希应该和官方校验值一致,证明 Range 续传没有产生数据错位/丢失"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
