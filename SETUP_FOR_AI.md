# 给 AI 的安装清单（subgen）

> 本仓库**只提供框架（源码）**：不含模型、不含二进制。需要的东西由使用者或它的 AI 按下面的清单去下载。
> 数字取自 2026-11-02 的实测与源码常量（`src/setup/mod.rs`、`src/models/whisper.rs`、`src/translate/mod.rs`）；URL 当天 HTTP 200。

## 0. 先决条件

- Windows 10/11、Rust 1.92+（edition 2024）、NVIDIA 显卡 + 支持 CUDA 12.4 的驱动
- 显存：Sakura-14B/Qwen3-14B 常驻 + whisper large-v3 同时跑，实测峰值约 11.8GB（含桌面约 2GB），**12GB 卡余量很小**
- 磁盘：组件约 1.1GB + 语音模型 3.1GB + 翻译模型 8.2–9.0GB

## 1. 构建与运行

```powershell
cargo build --release
.\target\release\subgen.exe
```

**最省事的做法：直接运行，首次启动会自动下载并校验下面全部组件**（带断点续传、官方源失败自动切镜像、哈希必须通过）。
AI 如需手动备齐，按 §2：`engine/`、`ffmpeg/`、`translator/` 三个目录放在 exe 同目录，zip 解压进去即可（程序递归查找可执行文件）；
`models/` 目录可放在任意位置（界面里「组件与存放位置」指过去）。

## 2. 需要下载的外部资产

| 组件 | 来源 | 大小 | sha256 |
| :--- | :--- | :--- | :--- |
| whisper.cpp 引擎（MIT） | https://github.com/ggml-org/whisper.cpp/releases/download/v1.9.1/whisper-cublas-12.4.0-bin-x64.zip | 677,887,125 B | `106a2030eff8998e4ef320fe72e263a78449e9040386ee27c41ea80b001b601b` |
| FFmpeg（gyan essentials 构建；许可以其发布页为准） | https://github.com/GyanD/codexffmpeg/releases/download/8.1.2/ffmpeg-8.1.2-essentials_build.zip | ~110MB | `db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec` |
| llama.cpp 翻译引擎（MIT） | https://github.com/ggml-org/llama.cpp/releases/download/b10079/llama-b10079-bin-win-cuda-12.4-x64.zip | 249,081,777 B | `3f94750a057e22e970e5fc6e4960dddd018baf4ed69829c84d1a2f09b5894bdd` |
| whisper large-v3（MIT） | https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin | 3,095,033,483 B | `64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2` |
| VAD（预下载备用，当前默认流程不使用） | https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin | ~1MB | `29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf` |
| Sakura-14B 日译中（**CC BY-NC-SA 4.0，非商用**） | https://huggingface.co/SakuraLLM/Sakura-14B-Qwen3-v1.5-GGUF/resolve/main/sakura-14b-qwen3-v1.5-iq4xs.gguf | 8,180,361,440 B | `cd1a129b19c711fb67f232e7feadc3cf0b2183e9408d47ac222f14123dae5436` |
| Qwen3-14B 通用翻译（Apache-2.0） | https://huggingface.co/unsloth/Qwen3-14B-GGUF/resolve/main/Qwen3-14B-Q4_K_M.gguf | 9,001,753,984 B | `5eaa0870bd81ed3b58a630a271234cfa604e43ffb3a19cd68e54a80dd9d52a66` |

可选语音模型档位（同一 HF 仓库，换文件名）：`ggml-large-v3-turbo.bin`（1,624,555,275 B）、`ggml-medium.bin`（1,533,763,059 B）；哈希见 `src/models/whisper.rs`。
源码里的 `size_bytes`/`sha256` 是权威值；上表若与源码不一致，以源码为准并更新本表。

## 3. 验证

```powershell
# 交付门禁：零警告 release 构建 + 单元测试(应为 206 passed, 1 ignored) + clippy -D warnings + 行数预算
powershell -ExecutionPolicy Bypass -File scripts\verify.ps1
```

真机冒烟：拖一段几十秒的日语视频进窗口，勾选翻译，应在视频同目录生成 `.srt` 与 `.zh.srt`。

## 4. 已知陷阱

- **llama-server 要用 whisper 引擎目录里的 CUDA DLL**：本项目启动它时把引擎目录加进子进程 PATH，所以**不要**另下 400MB 的 cudart 包。
  自己单独跑 `llama-server.exe` 做实验时若不设同样的 PATH，CUDA 后端加载失败会**静默退回 CPU**（慢 10 倍以上，且没有报错）。
- 翻译用的 llama-server 参数是 `-ngl 99 -c 8192 --jinja`。
- whisper.cpp 新版（v1.9.3 起）的正式 release 页**没有预编译包**，Windows 二进制挪到了 `b5xxx` 预发布 tag、文件名也变了；升级引擎要同时改 URL 与哈希。
- 配置里的旧翻译模型 key `sakura-7b` 启动时自动迁移为 `sakura-14b`。
- 许可见 README「第三方组件许可」。
