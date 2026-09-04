//! 轻量硬件探测,不依赖任何已下载的组件。
//!
//! 首次下载页(引擎/模型都还没下载)需要回答"这台机器有没有可用的 CUDA 加速",而真正可靠的
//! 探测(`translate::server::probe_gpu_available`)要等 llama-server.exe 下载完才能跑——在
//! 用户决定下不下那 677MB 的 CUDA 版引擎之前就给出判断,只能靠 OS 层面的廉价信号。
//!
//! 现状:whisper 引擎和翻译引擎的预编译包都是 **CUDA 12.4 构建**(见 setup/mod.rs 的
//! ENGINE_URL/TRANSLATOR_URL),纯 CPU/无 N 卡机器下载后照样能跑(CUDA 构建会静默回退 CPU),
//! 但用户会白下几 GB、实际速度远低于预期。所以这里在下载页给出"检测到 NVIDIA 驱动"的
//! 提示,让用户在下载前就知道自己这台机器预计走 GPU 还是 CPU。

use std::path::Path;

/// Windows 上 NVIDIA 驱动安装时会把 `nvcuda.dll` / `nvapi64.dll` 放进 System32,文件存在就
/// 说明装了 NVIDIA 显卡驱动(大概率有可用的 CUDA 设备)。纯文件存在性检查,毫秒级、零依赖,
/// 可在首次下载页调用。不探测 Intel/AMD 集显——subgen 的预编译包只有 CUDA 后端,集显不提供
/// 加速,探测它们没有意义(识别阶段会解析 whisper 日志里的 "using CUDA" 行来最终确认)。
#[cfg(windows)]
pub fn nvidia_driver_present(system32: &Path) -> bool {
    system32.join("nvcuda.dll").is_file() || system32.join("nvapi64.dll").is_file()
}

/// 在真实系统上探测:返回 `Some(true)` = 检测到 NVIDIA 驱动,`Some(false)` = 确定没有,
/// `None` = 无法判断(读不到 System32 路径等,尽力而为,不该阻断)。
#[cfg(windows)]
pub fn has_nvidia_driver() -> Option<bool> {
    let system_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(std::path::PathBuf::from)
        .or_else(|| std::path::PathBuf::from("C:\\Windows").into())?;
    Some(nvidia_driver_present(&system_root.join("System32")))
}

#[cfg(not(windows))]
pub fn has_nvidia_driver() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用临时目录模拟 System32,分别造出 nvcuda.dll / nvapi64.dll / 两个都没有三种情况,
    /// 确认"任意一个存在即判定有 NVIDIA 驱动"。
    #[cfg(windows)]
    #[test]
    fn nvidia_driver_present_recognizes_either_marker_dll() {
        let dir = std::env::temp_dir().join(format!(
            "subgen_test_nvdriver_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // 只有 nvcuda.dll
        std::fs::write(dir.join("nvcuda.dll"), b"x").unwrap();
        assert!(nvidia_driver_present(&dir));

        // 删掉 nvcuda.dll,换成 nvapi64.dll
        std::fs::remove_file(dir.join("nvcuda.dll")).unwrap();
        std::fs::write(dir.join("nvapi64.dll"), b"x").unwrap();
        assert!(nvidia_driver_present(&dir));

        // 两个都没有
        std::fs::remove_file(dir.join("nvapi64.dll")).unwrap();
        assert!(!nvidia_driver_present(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }
}