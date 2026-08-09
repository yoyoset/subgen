# subgen —— 字幕生成器

给 Windows 上的视频文件生成字幕(.srt),本地跑,不联网上传你的视频/字幕内容。绿色软件,不装到系统里。

语音识别用 whisper.cpp;**可选**再用本地大模型把字幕翻译成中文(也是本地跑,不联网上传字幕内容)——日语走 [Sakura](https://github.com/SakuraLLM/SakuraLLM)(专门用轻小说语料微调的日→中模型,效果明显好于通用模型),其他语言走 Qwen3 通用模型,自动按识别出的源语言选择,也可以手动指定。

---

## 用法

1. 双击 `subgen.exe`(或 `cargo run` 跑起来)
2. **首次运行**会自动下载必要组件(识别引擎、FFmpeg、语音模型;如果勾选了翻译还会下载翻译引擎和翻译模型)。`engine/`、`ffmpeg/`、`translator/`(体积小,约 1GB 以内)固定存在 exe 同目录;`models/`(体积大,语音模型+翻译模型加起来能到 10GB+)**可以改存放位置**——首次下载界面,以及主界面右上角"组件与存放位置"窗口里,都有"模型位置: ... [更改...]"这一行,点"更改..."选个别的盘,不想让 C 盘被吃掉一大截的话就用这个。已经下载过的模型文件会自动搬过去,不用重新下载。"组件与存放位置"窗口会列出全部模型**和**识别引擎/FFmpeg/翻译引擎二进制的装没装状态,任何一项缺失都能在窗口里直接点"下载"补齐(二进制走 zip 解压,模型走单文件下载),不用翻 README 手动装
3. 把视频文件拖进窗口,或点"选择视频文件..."按钮(支持多选)
4. 需要中文翻译的话,勾选"翻译成中文(在本机运行,不上传内容)",可以选翻译模型(自动按语言 / 固定用 Sakura / 固定用 Qwen3)和输出格式(双语字幕 / 仅中文)
5. 点"开始处理",逐个跑完。每个文件的处理过程分"提取音轨 → 识别 → 翻译"三步(没开翻译就两步),界面上有个小步骤条实时显示卡在哪一步、哪一步已经完成,处理中窗口标题也会显示"第 N/M 个"、批次跑完会闪一下任务栏。原文字幕生成在**视频同目录、同名**的位置(如 `EpisodeName.mkv` → `EpisodeName.srt`);勾选了翻译的话额外生成 `EpisodeName.zh.srt`(双语时中文在上、原文在下)。大多数播放器(包括 Jellyfin)会自动识别加载这些外挂字幕
6. 每个任务行下面有一排小按钮(处理中不可用):**移除**(从列表拿掉,不影响已生成的文件)、**重新处理**(哪怕已经是"完成"状态,也强制重新识别/翻译一遍,不复用旧结果)、**打开文件夹**(在资源管理器里定位到生成的字幕,还没出结果就定位到视频本身)、**预览**(完成态可展开,直接在界面里看前 10 条译文,不用离开软件去找文件核对翻译质量)

**处理中途崩溃/被强制关闭也不怕**:所有子进程(ffmpeg/whisper-cli/翻译引擎)都绑定在一个 Windows Job Object 上,主程序不管是正常退出、崩溃还是被任务管理器强杀,这些子进程都会被系统自动一起终止,不会变成孤儿进程继续占着显存、留一堆没人管的临时文件。重新处理同一个文件时,如果 `tmp/` 目录里已经有提取好的音轨或识别好的字幕(比如上次真的没跑完、或者是很早以前跑到一半被打断的,或者是自己点了"取消"),会自动复用、跳过重新跑那一步——不用因为中断一次就把已经做完的工作全部重来。**点"取消"和意外中断的效果是一样的**:已经完整提取出来的音轨不会被删掉,下次重新处理同一个文件(用同一个模型和语言)会直接续上,不会平白多花一遍提取音轨的时间。

## 首次下载的组件

| 组件 | 来源 | 大小 | 何时需要 |
|---|---|---|---|
| whisper.cpp(识别引擎)| [ggml-org/whisper.cpp](https://github.com/ggml-org/whisper.cpp) 官方 Release,Windows CUDA 预编译版 | 约 650MB | 总是 |
| FFmpeg(音频提取) | [GyanD/codexffmpeg](https://github.com/GyanD/codexffmpeg/releases) essentials 构建(gyan.dev 官方在 GitHub 上的发布仓库,带版本号) | 约 100MB | 总是 |
| large-v3 语音模型 | [Hugging Face](https://huggingface.co/ggerganov/whisper.cpp) | 约 2.9GB | 总是 |
| VAD 模型 | [Hugging Face](https://huggingface.co/ggml-org/whisper-vad) | 约 1MB(体积很小,预先下好备用;当前默认识别流程不使用,见下文 VAD 的坑) | 总是 |
| llama.cpp(本地翻译引擎)| [ggml-org/llama.cpp](https://github.com/ggml-org/llama.cpp) 官方 Release,Windows CUDA 预编译版 | 约 240MB | 勾选翻译后 |
| Sakura 7B(日→中翻译模型)| [SakuraLLM/Sakura-7B-Qwen2.5-v1.0-GGUF](https://huggingface.co/SakuraLLM/Sakura-7B-Qwen2.5-v1.0-GGUF) | 约 4.25GB | 勾选翻译且选了 Sakura 或"自动" |
| Qwen3 4B(通用翻译模型)| [unsloth/Qwen3-4B-Instruct-2507-GGUF](https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF) | 约 2.4GB | 勾选翻译且选了 Qwen3 或"自动" |

**下载支持断点续传**:大文件(几个 GB)在家用网络上中途断开是首次安装最常见的失败原因,不用担心从零重来——网络中断或程序被关掉后重新下载同一个组件,会自动从上次的进度接着下,不会浪费已经下载的部分(内部原理:边下载边把已收到的内容写进 `.part` 临时文件,重启后先读取这部分内容补算哈希,再用 HTTP Range 请求续传;目标服务器不支持续传时会自动退化成从头下载)。单次连接中途断开也会在原地自动重试几次,不会一遇到网络抖动就整个失败。

**官方源连不上时自动切镜像**:HuggingFace 和 GitHub 在部分网络环境(比如国内)下经常连不上或很慢。官方地址多次重试仍失败后,程序会自动切换到镜像地址重试——HuggingFace 换成 [hf-mirror.com](https://hf-mirror.com)(国内 ML 社区公认的标准镜像,只换域名、路径不变),GitHub Release 资产换成 [gh-proxy.com](https://gh-proxy.com) 反代前缀。镜像内容一样要过哈希校验,不会因为走了镜像就放松安全检查;镜像服务本身不保证长期稳定,如果失效了去 `src/setup/download.rs` 改 `GITHUB_PROXY_PREFIX`/`url_candidates` 里的 hf-mirror 域名即可。

如果自动下载失败(网络问题),可以按上表链接手动下载,分别解压/放到 exe 同目录的 `engine/`、`ffmpeg/`、`translator/` 文件夹里(zip 解压进去就行,程序会自动递归找里面的可执行文件,不要求固定的子目录层级),模型文件(语音模型/VAD/翻译模型)直接放进 `models/` 文件夹,不用解压。

如果你本地已经用其他工具(比如 LM Studio、text-generation-webui)通过 HuggingFace 下载过 Sakura 或 Qwen3 的 gguf 文件,首次下载时程序会自动扫描 `HF_HOME`/`HF_HUB_CACHE` 环境变量和 `~/.cache/huggingface` 默认缓存目录,哈希校验通过就直接复制过来,不会重复下载。

**更进一步:模型目录可以直接指向 HuggingFace 缓存本身**(比如 `F:\hf_cache`),不用复制。程序在这个目录下查找模型文件时是递归查找(和查找 whisper.cpp/ffmpeg/llama.cpp 可执行文件用的是同一套逻辑),不要求平铺结构,能直接在 HF 缓存自己的 `hub/models--xxx/snapshots/<commit>/文件名` 这种嵌套目录里找到已有文件——把"模型存放位置"(见下文)改指到 HF 缓存目录,就是和其它工具共用一个模型池,不用各自留一份。

### 模型存放位置

默认在 exe 同目录的 `models/` 文件夹,但可以改到任何盘、任何目录(比如 exe 图方便放在 C 盘桌面,但模型想放到空间更大的 D/E/F 盘)。改法:首次下载界面或主界面"组件与存放位置"窗口里点"更改...",选个文件夹即可——

- 选中的目录不存在会自动创建;创建/写入失败(比如选了只读的网络共享)会当场报错,不会拖到真正下载时才发现。
- 如果旧目录下已经有下载好的文件,会自动搬到新目录(同盘瞬间完成,跨盘走复制,搬迁过程中断电/被强关也不会丢文件——没搬完的部分原样留在旧目录,不会出现两边都没有的情况),不用因为换了目录就重新下载十几 GB。
- 下载前会检查目标盘剩余空间够不够,不够会有醒目提示(不会硬性阻断,因为部分组件大小是估算值)。
- 处理任务进行中不能改(避免正在跑的批次里,后面文件要用的模型路径突然变了)。

## 硬件要求

- NVIDIA 显卡(实测 RTX 3060 12G:large-v3 语音识别约 8 倍实时速度,一集 45 分钟的剧大概 5-7 分钟出字幕;翻译模型 7B/4B 量级在 12GB 显存上和识别模型一起跑也没问题——同一批次里翻译引擎常驻显存、识别引擎按文件启停,两者短暂重叠时显存也够)
- 没有 N 卡也能跑:whisper-cli 找不到 CUDA 会回退到 CPU 继续识别,翻译引擎(llama-server)也有 CPU 可用路径。但**纯 CPU 路径只在开发机上推断过、尚未在无独显的真实机器上实测**,速度会慢很多,有 N 卡时这条可以忽略。程序会在处理时检测识别和翻译两个阶段是否真的用上了 GPU(`--list-devices` 探测 + whisper 日志解析),没真正用上会在界面上醒目提示。

## 卸载

删掉整个文件夹就行。不写注册表、不装到系统目录。

---

## 给开发者

### 构建

```
cargo build --release
```

产物在 `target/release/subgen.exe`,单文件,把它拷到任何地方运行都行(首次运行还是会自动下载组件到它所在目录)。

**交付门槛**:改完代码用一条命令验证所有门槛——`powershell -File scripts/verify.ps1`(release 构建零警告 + `cargo test` 全绿 + `cargo clippy --all-targets -- -D warnings` 零警告 + `app.rs` 行数预算),全过才算可以交付/提交。CI(.github/workflows/ci.yml)跑的是同一套检查。

### 技术栈

- **界面**:[egui](https://github.com/emilk/egui) / eframe(纯 Rust,无 Node.js/webview 依赖)+ [rfd](https://github.com/PolyMeilex/rfd)(原生文件选择框)
- **识别引擎**:[whisper.cpp](https://github.com/ggml-org/whisper.cpp) 官方预编译二进制,子进程调用(不是 FFI 链接),好处是不用在编译本项目时碰 CUDA 工具链
- **音频提取**:ffmpeg(便携版,子进程调用,把视频转成 16kHz 单声道 wav —— **whisper-cli 不能直接读视频文件**,只支持 flac/mp3/ogg/wav,这是实测确认过的,不是文档假设)
- **翻译引擎**:[llama.cpp](https://github.com/ggml-org/llama.cpp) 官方预编译二进制里的 `llama-server`,子进程调用,通过它的 OpenAI 兼容 HTTP API(`/v1/chat/completions`)喂字幕文本、拿翻译结果——和识别引擎同一套"不链接、不碰 CUDA 工具链"的哲学

> **eframe 0.35 API 提醒**(踩过坑):这个版本的 `eframe::App` trait 用的是
> `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame)`,不是旧教程里熟悉的
> `fn update(&mut self, ctx: &egui::Context, ...)`;面板 `.show()` 也相应地接收
> `&mut Ui` 而不是 `&Context`。升级 eframe 版本时先核实这一点,别照旧教程改。
> 另外 `egui::Theme`/`ThemePreference`/`CornerRadius`/`style_mut_of` 都是 0.35 的命名,
> 更老的 `Rounding` 已被移除(见 `ui/theme.rs`)。

### 项目结构

```
src/
  main.rs      egui 应用入口 + 全局初始化:单实例锁、panic 日志、模型目录覆盖、
               启动时清理过期的 tmp/ 中间产物
  app.rs       应用状态(任务队列/异步操作槽位)与事件轮询,UI 只做状态分发
               (约束 ≤1300 行,超了先拆新职责,见 docs/app-responsibility-map.md)
  config.rs    便携配置(config.toml,存在 exe 同目录):原子写入(.tmp+改名,写入中途断电
               不坏文件),解析失败自动备份 .bak 恢复默认,并在界面显示可关闭的说明横幅
  paths.rs     所有路径默认相对于 exe 所在目录(绿色软件要求);模型目录覆盖值/
               绑定表是进程级全局状态,界面改了立刻生效
  format.rs    人话格式化 + 状态文案/配色 + 应用日志落盘与轮转(超 2MB 转 .log.1)
  report.rs    翻译复核报告:粗筛可疑行、报告文本生成、粘贴修正回写(含 .bak 备份)
  job.rs       任务队列(Stage 枚举),后台线程串行处理,channel 回传状态给界面
  job_guard.rs 用 Windows Job Object 绑定所有子进程生命周期,主程序怎么退出子进程
               都会被系统一起终止
  engine/      调用 ffmpeg 提取音轨 + whisper-cli 识别:断点续跑、复读折叠、
               GPU/语言检测、临时产物清理
    mod.rs / srt.rs / whisper_log.rs
  translate/   调用 llama-server 做字幕翻译:模型注册表("语言→模型"映射)、
               编号对齐校验、句子碎片合并分组、失败重试
    mod.rs / prompt.rs / segment.rs / server.rs
  models/      whisper 语音模型注册表 + 模型状态判定(就绪/缺失/绑定失效/大小异常)
    mod.rs / whisper.rs / actions.rs(模型管理操作:定位/校验/绑定/内联安装/扫描/清理)
  setup/       首次组件检测 + 下载 + 解压 + 哈希校验 + 模型目录搬迁
    mod.rs / download.rs / migrate.rs
  ui/          egui 界面(主题 + 各面板),和业务逻辑严格分开
    theme.rs 单一语义色板 + 全局视觉风格(改配色只动这一个文件)
    batch_bar.rs / queue.rs / detail.rs / main_view.rs / settings.rs
    components.rs / model_manifest.rs / setup_screens.rs(首次引导页)
```

### 已知的技术事实(写代码时实测验证过,不是猜的)

- whisper.cpp 官方 Windows CUDA 预编译包(v1.9.1)解压后,可执行文件在内部的 `Release/` 子目录,但代码用递归搜索找 `whisper-cli.exe`,不依赖固定路径
- whisper-cli 用 miniaudio 解码音频,**不支持 mp4/mkv 等视频容器**,必须先 ffmpeg 转 wav
- GPU 是否真正用上,看 whisper-cli 输出里有没有 `using CUDA0 backend` 这一行;显式禁用 GPU(`-ng`)时会输出 `no GPU found`。**已知 whisper.cpp 在部分 Windows 环境下会静默回退 CPU 而不报错**,所以必须解析这行日志来判断,不能假设"没报错就是用了 GPU"
- 进度行格式:`whisper_print_progress_callback: progress = NN%`,NN 可能超过 100(whisper.cpp 自身的估算误差),界面展示时要 clamp;**大文件/复杂内容第一次进度更新可能要等一两分钟才出现**,期间界面靠"已运行 XXs"计时器提示未卡死,而不是干等一个不动的 0%
- **`--vad` 默认关闭,不是开启**(踩过坑,方向和最初设计相反)。实测拿同一段真实纪录片音频对比:开 `--vad` 会导致输出全部丢失大小写和标点(比如 "Jeremy Wade, explorer" 变成 "jeremy wade explorer"),不开 VAD 则完全正常、且没有观察到幻听/回退重试(fallbacks=0)。VAD 的防幻听收益只在纯音调/长静音场景验证过("(phone ringing)" 这种无中生有的内容),对持续对白类内容(纪录片、剧集)VAD 弊大于利,所以 v1 默认不开
- **GUI 主程序用 `windows_subsystem = "windows"` 关掉了自身的控制台窗口**(release 构建才生效,debug 构建保留控制台方便看日志),但这引出一个连带问题:**子进程(ffmpeg / whisper-cli)如果不显式处理,Windows 会单独给它们弹一个新控制台窗口**(实测确认过,主程序一关控制台,点"开始处理"就弹出一个 ffmpeg/whisper-cli 的黑窗口)。用户如果手滑关掉那个弹出的控制台窗口,等于给子进程发了 SIGTERM,会导致"音频提取失败"之类的误导性报错(看着像文件损坏,其实只是窗口被关了)。修复:两个 `Command`(ffmpeg、whisper-cli)都加 `creation_flags(CREATE_NO_WINDOW)`(`0x0800_0000`),子进程不再弹窗,stdout/stderr 依然通过管道正常捕获,不影响进度解析
- **whisper.cpp 长文件"复读机"故障 —— 根因已定位并修复**:实测在一个约 44 分钟的真实文件上遇到过解码卡死循环,同一句字幕原样重复了 983 次,后面的内容全部丢失。第一次只加了后处理安全网(`collapse_repetitions`,检测连续 ≥4 条完全相同的字幕并折叠成 1 条),用户重新处理同一文件后**在完全相同的位置(00:08:06)再次触发**——说明问题是确定性的,不是随机的,安全网只是把垃圾行折叠好看了,但丢失的 32 分钟内容并没有找回来,必须治根因。根因:whisper 默认把前一个 30 秒窗口识别出的文本作为下一个窗口解码的上下文("condition on previous text"),一旦某句话重复进入上下文,就会自我强化、越来越确信"接下来还是这句",从而卡死到文件结束——这也解释了为什么单独截出来的 4 分钟/15 分钟短片段(上下文是干净的)怎么都无法复现。修复:加 `-mc 0`(`--max-context 0`)禁止跨窗口携带文本上下文。已在同一份真实故障音频上验证:加参数后重复计数从 983 降到 0,fallbacks 从 5 降到 0,全片 44 分钟内容完整识别。代价很小,只在全片末尾约 68 秒观察到大小写丢失,规模远小于此前的复读故障,暂未处理。`collapse_repetitions` 安全网继续保留,作为纵深防御
- **SakuraLLM 只训练了日→中方向**,拿它翻英文等其他语言效果很差(不在训练分布内),所以 `src/translate/mod.rs` 里用"语言→模型"注册表(`translate::MODELS`)按 whisper 检测出的源语言选模型:日语走 Sakura,其余语言走通用的 Qwen3-4B-Instruct,而不是无脑一个模型翻所有语言
- **翻译走批量 + 编号对齐,不是逐行请求**:整集字幕逐行发 HTTP 请求太慢(一集可能几百条),所以每批 15 行一起发给模型,提示词里显式给每行编号、要求模型保留编号原样输出,回复按编号解析对齐;行数对不上(模型漏译/多译/合并了某几行)就对半拆开重试,缩小出错范围,最终还是对不齐的单行原样保留原文、不阻断整个任务(`translate::translate_batch_with_retry`)。这也是为什么 Sakura 用的提示词和官方文档给的单段落格式不完全一样——官方格式没有编号,是拿吞吐量换来的妥协,如果实测发现批量编号格式明显拉低 Sakura 翻译质量,后续可以把 `BATCH_SIZE` 调小
- **llama-server 复用 whisper 引擎目录里的 CUDA 12.4 运行库 DLL**,不单独下载一份约 400MB 的 cudart 运行库包:两者都是 CUDA 12.4 构建,`translate::LlamaServer::start` 把 whisper 引擎所在目录塞进子进程 PATH 前部,Windows 动态链接器就能找到 `cudart64_12.dll`/`cublas64_12.dll`/`cublasLt64_12.dll`,免掉重复下载
- **批次内 llama-server 常驻、按需重启**:一个视频转写完立刻用同一个 `llama-server` 翻译,启动一次要把几 GB 模型加载进显存,不可能每个文件都重启;只有当这批文件里下一个视频需要的翻译模型和当前跑着的不一样时(比如"自动"策略下前一个是日语用 Sakura、下一个是英语要切到 Qwen3)才 kill 掉重启。批次处理完 / 被取消时显式 stop,`Drop for LlamaServer` 兜底 kill,避免异常退出路径下留下孤儿进程占着显存
- **模型目录覆盖是进程级全局状态,不是把 Config 一路传参传下去**:`src/setup/`、`src/engine/`、`src/translate/` 深层调用 `paths::model_path()`/`vad_model_path()`/`trans_model_path()` 的地方都拿不到 `Config`,真要一路传参改动面太大,所以用 `paths.rs` 里一个 `Mutex<Option<PathBuf>>` 静态变量(`set_models_dir_override`/`models_dir()`),main.rs 启动时用配置文件里的值设一次,用户在界面里改存放位置时重新设一次,下游所有路径函数自动生效不用改调用方
- **换模型目录时,旧目录里已有文件的搬迁是"尽力而为、不因单个文件出错中断整体"**:`setup::migrate_models_dir` 遇到某个文件搬不动(比如目标已有同名但大小不同的文件、复制失败)只记一条问题继续搬下一个,不会因为一个文件卡住就让后面几个 GB 的文件全跳过不搬;不管过程顺不顺利、是否被取消,调用方都会把生效目录切到新位置——已经搬过去的文件才是准确的当前状态,没搬完的缺口交给现成的 `missing_components()` 首次下载流程去补,不需要为这个中间状态写特殊分支。跨盘搬迁是"复制到临时名 → 核对大小 → 原子改名 → 删源文件",任何一步中断源文件都还在原地,不会两边都丢
- **子进程用 Windows Job Object 绑定生命周期,而不是靠自己的清理代码**:真实遇到过一次主程序意外退出(大概率是 wgpu 渲染层的"设备丢失直接 panic"——它默认行为就是这样,和 whisper-cli/llama-server 抢同一块物理 GPU 时有一定概率触发),whisper-cli 子进程当场变孤儿、自己又跑了半个多小时,GPU 占着、临时文件也没人清理。`std::fs::remove_file` 这类"退出前清理"代码天然靠不住,因为进程崩溃的那一刻恰恰是这些代码来不及跑的时候。修复用 `win32job` crate 创建一个带 `KILL_ON_JOB_CLOSE` 限制的 Job Object,ffmpeg/whisper-cli/llama-server 一 spawn 出来就绑进去——这是操作系统内核层面的保证,主程序不管怎么死,子进程都会被一起干掉,不依赖任何应用层代码执行。用真实拉起的子进程验证过这个机制确实生效(`job_guard::tests::dropping_the_job_kills_bound_child_process`)
- **断点续跑靠文件是否存在,不是额外的状态记录**:`tmp/` 里的临时文件名本来就是"{文件名}_{路径哈希}.wav/.srt"和视频一一对应,`engine::process_video` 重新处理某个文件前会先检查这两个中间产物是不是已经存在且完整(srt 用 `srt_looks_complete`:内容为空或者能解析出至少一块都算完整),完整就直接复用、跳过重新跑那一步。复用识别结果时没有真实跑 whisper-cli,`used_gpu` 用 `Option<bool>` 表示"这次没有观测"(不是 false),避免误报 GPU 警告;`detected_language` 会尝试从同一个哈希对应的旧日志文件里恢复,找不到就退化成 "auto"

### 哈希校验的可信度分级

- `engine`(whisper.cpp)、`ffmpeg`、`vad` 模型、`model`(large-v3)、`translator`(llama.cpp)、两个翻译模型(Sakura/Qwen3):哈希都是通过 GitHub Release API 的资产 digest 字段或 HuggingFace LFS 指针文件核实过的,large-v3、ffmpeg 额外做过一次完整下载后本地 `sha256sum` 复核,两者一致;校验不过**阻断安装**
- `ffmpeg` 早期用的是 gyan.dev 滚动更新的"最新构建"链接,没有可钉死的版本号/哈希,只做存在性检查;已改用 [GyanD/codexffmpeg](https://github.com/GyanD/codexffmpeg/releases) 在 GitHub 上带版本号的正式发布(内容与 gyan.dev 官网一致),可以像其他组件一样钉版本号 + 强哈希校验,不再依赖一个随时可能变化的滚动链接

如果以后 whisper.cpp/ffmpeg/llama.cpp 出新版本、想升级绑定的版本号,去 `src/setup/mod.rs` 顶部改对应的 `_URL`/`_SHA256` 常量,记得重新下载一遍算真实哈希,不要瞎编。

### TODO / 已知小问题

- [x] ~~"剩余 N 项"计数不准~~ 已修复,改成"第 N/总数 项"
- [x] ~~大文件处理时进度长时间不动,看着像卡死~~ 已加"已运行 XXs"计时器 + 首次更新慢的提示文案(见 `job.rs` 的 `started_at`、`format.rs` 的 `format_elapsed`)
- [x] ~~开 VAD 导致输出丢失大小写/标点~~ 已通过真实音频对比测试确认是 `--vad` 的问题,v1 默认关闭
- [x] ~~点击"开始处理"弹出 ffmpeg/whisper-cli 的控制台窗口,手滑关掉会导致误导性的"提取失败"报错~~ 已加 `CREATE_NO_WINDOW` 创建标志,子进程不再弹窗
- [x] ~~长文件偶发"复读机"故障(同一句字幕重复几百次,后面内容全丢)~~ 根因已定位(跨窗口文本上下文自我强化)并用 `-mc 0` 修复,`collapse_repetitions` 安全网保留作纵深防御,详见上文
- [x] ~~重新点"开始处理"会把已经出过字幕的文件重新跑一遍~~ 已修复,只重跑状态不是 Done/DoneWithWarning 的任务
- [x] ~~窗口大小调整后不会记住,下次启动又是默认尺寸~~ 已修复,`on_exit` 时落盘当前窗口尺寸
- [x] ~~只做语音识别,不做翻译~~ 已接入本地翻译(llama-server + Sakura/Qwen3),详见上文
- [x] ~~模型默认下载到 exe 所在盘,放 C 盘会把系统盘吃掉一大截~~ 已支持自定义模型目录 + 已有文件自动搬迁,详见上文"模型存放位置"
- [x] ~~主程序意外退出后,whisper-cli/ffmpeg/llama-server 子进程变成孤儿继续跑~~ 已用 Windows Job Object(`KILL_ON_JOB_CLOSE`)绑定子进程生命周期,主程序不管怎么退出子进程都会被系统一起终止,详见上文
- [x] ~~处理中断后重新跑会把已经提取的音轨/识别的字幕从头再来一遍~~ 已支持断点续跑,`tmp/` 里已有的完整中间产物会自动复用,详见上文
- [x] ~~任务列表长文件名和状态文案会互相覆盖显示~~ 界面重做:文件名/状态各占一行,文件名超长会省略号截断(hover 可看完整),流水线阶段用步骤条单独展示
- [x] ~~大文件下载中断后只能从头重新下载~~ 已支持断点续传(HTTP Range),已用真机对 HuggingFace 和 GitHub Release 两个真实 CDN 实测验证(`curl -L -H "Range: bytes=1000-"` 跟随 302 重定向后仍能拿到 206 Partial Content),`.part` 临时文件在下载中断后不再被删除,详见上文"下载支持断点续传"
- [x] ~~HuggingFace/GitHub 在国内网络环境下经常连不上,没有备用线路~~ 已支持官方源多次失败后自动切换镜像(hf-mirror.com / gh-proxy.com),已用真机验证 gh-proxy.com 对 GitHub Release 资产的 Range 请求同样能正确返回 206,详见上文"官方源连不上时自动切镜像"
- [x] ~~翻译阶段没有 GPU 使用检测(语音识别阶段有,`used_gpu` 靠解析 whisper-cli 日志里的 `using CUDA0 backend`;llama-server 的日志之前只是排空、没解析)~~ 已支持,但检测方式中途因为真机测试推翻重做过一次,记录一下踩的坑:最初实现是解析 llama-server stderr 里的 `offloaded N/M layers to GPU`(格式字符串号称来自 llama.cpp b10079 源码 `load_tensors()` 里的 `LLAMA_LOG_INFO` 实测确认)。等到真的在装了 RTX 3060 的机器上跑通整条翻译流程后用 `nvidia-smi` 独立核对显存(基线约 2260 MiB → 加载 2.4GB 的 Qwen3 模型后约 5988 MiB,增量约 3.7GB,GPU 确实被用上了),却发现这次启动的 stdout(0 行)和 stderr(11 行,全文读完)里根本没有任何包含 offload/CUDA/cuda/device 的行——项目实际绑定的这个 llama-server.exe(build `b10079-40b740ad0`)在真实运行里不打印那一行日志,"源码实测确认"这个说法本身就是错的(读的是源码,不是这个二进制真实跑出来的日志)。旧实现在这个场景下会稳定得到假阴性:GPU 明明用上了,却告诉用户"未能使用显卡加速",比不检测还误导人。现改成 `translate::probe_gpu_available`:启动真正的服务进程之前,先用同样的 PATH(共享 whisper 引擎目录下的 CUDA DLL)单独跑一次 `llama-server.exe --list-devices`(不加载模型,立刻打印设备列表退出),有设备行(如 `CUDA0: NVIDIA GeForce RTX 3060 (12287 MiB, 11255 MiB free)`)就判定为能用上 GPU,只有光秃秃的 `Available devices:` 标题行就判定用不上——这两种输出都用真机实测过(分别是 PATH 里有/没有 CUDA DLL 的情况)。界面警告文案会区分具体是"识别"还是"翻译"阶段没用上;顺带给翻译请求加了 `cache_prompt: true`,同批次内每次请求的 system prompt 相同,能复用 KV 缓存
- [x] ~~没有自动化 CI~~ 已新增 GitHub Actions Windows CI,在 `windows-latest` 上执行 release build、tests 和 `cargo clippy --all-targets -- -D warnings`
- [x] ~~`app.rs` 功能持续堆回单文件~~ 已记录真实职责边界,并在 CI 中用 `scripts/check-line-budgets.ps1` 将 `app.rs` 限制为 1300 行;超过预算必须先拆出新职责,详见 `docs/app-responsibility-map.md`
- [x] ~~界面配色/文案/布局各自硬编码,风格不统一~~ 已收拢:单一语义色板 + 全局视觉风格在 `src/ui/theme.rs`(成功/警告/错误/进行中/中性五类语义色,改配色只动这一个文件);首次下载引导页、空状态、批次条主按钮都按统一结构重做
- [x] ~~`logs/app.log` 和 `crash.log` 只增不删,长期使用会无限膨胀~~ 已加体积上限轮转:超过 2MB 自动改名成 `.log.1`(丢弃更早的),见 `format::rotate_log_file`
- [x] ~~`tmp/` 中间产物只增不删~~ 启动时清理超过 7 天没被复用的 `.wav/.srt`(断点续跑缓存只在"中断后很快重跑"时有价值,过期的不再需要),见 `engine::cleanup_stale_tmp`
- [x] ~~"清空列表"无确认,一次误点丢整个队列~~ 已加确认弹窗(已生成的字幕文件不受影响,只是从列表拿掉)
- [x] ~~复核报告/修正回写时文件写盘失败被静默吞掉(界面有报告、视频旁没有文件)~~ 已改为写入失败时报错提示,见 `report::generate_review_report`/`apply_corrections`
- [x] ~~ffmpeg 提取音轨期间点"取消"可能失效~~ 旧实现用 `wait_with_output()` 阻塞等待子进程,期间完全不检查取消标志——ffmpeg 万一卡住(损坏文件等)批次就永远停在"正在取消…"。已改为轮询 `try_wait` + 后台线程排空 stderr,全程响应取消信号;轮询逻辑抽成 `engine::wait_child_with_cancel` 并**用本机真实 ffmpeg + 真实视频实测过**:成功提取出有效 16kHz 单声道 wav、800ms 内取消能 kill 子进程返回"已取消"(临时测试跑完即删,不入库)
- [x] ~~配置写入可能留下半截损坏文件;配置损坏被静默重置时用户毫不知情~~ 已改原子写入(先写 `.tmp` 再改名覆盖);解析失败恢复默认时,界面顶部显示一条可关闭的说明横幅(之前只写进 app.log,用户不知道自己模型目录设置被重置了),见 `Config::save`/`load`
- [x] ~~任何后台线程(批次 worker / 下载 / 搬迁 / 内联安装 / 哈希校验 / 扫描 / 清理)内部 panic 时,界面永远卡在"处理中/下载中/扫描中"~~ 已统一加 panic 兜底:批次 worker 用 `catch_unwind` 保护,panic 时把剩余任务标记为失败并照常发 BatchDone;其余后台线程走 `spawn_guarded_thread`(app.rs),panic 时补发失败终态消息,都不会卡死
- [x] ~~加入队列后视频文件被移动/删除时,报的是误导性的"音频提取失败(可能损坏或无音轨)"~~ 已在跑 ffmpeg 前先检查 `video_path.is_file()`,明确报"找不到视频文件",见 `engine::process_video`
- [x] ~~拖入/选中的文件里没有可识别的视频时,界面毫无反应~~ 现在会提示"没有添加任务:没有可识别的视频,或都在列表中了"(部分成功时保持安静,不制造噪音),见 `app::add_files` 返回的计数
- [x] ~~批次进度只有文字,整体位置感弱~~ 批次条新增进度条(与"第 N/M 个"文字互补);设置窗口新增版本号/帮助脚注;"重新处理"点击后会给"下次开始处理时生效"的反馈
- [x] ~~下载过程中点"取消"后,界面显示"组件下载失败: 用户取消"+"请检查网络后重试"这种误导文案~~ 用户主动取消现在回到"还缺什么"的清单界面(已下完的组件会被重新核算),不再伪装成网络失败,见 `app::is_user_cancel`/`poll_setup`
- [x] ~~config.toml 里手改出非法窗口尺寸(NaN/负数/巨大数)会直接喂给 viewport~~ 已加"有限值 + 夹到合理区间"的防御,非法值退回默认,见 `main::sane_window_dimension`
- [x] ~~目录遍历在 Windows 符号链接/Junction 构成环时无限死循环,batch worker 卡死且无法取消~~ 已用规范化路径去重(`paths::find_all_matching_files`),并用真实 junction 环实测终止;另修复:字幕写入改为原子覆盖(report/engine)、配置备份失败如实告知、内联安装取消不再报"失败"、ffmpeg 失败文案不再把原因钉死在"文件损坏"、翻译服务健康检查客户端提前构建避免泄漏子进程、删除下载哈希校验里不可达的"警告放行"分支
- [x] ~~全部任务已完成时点"开始处理"毫无反应~~ 现在会提示"所有任务都已处理完成,想重跑某一集请在其任务行点'重新处理'",见 `app::start_processing`
- [x] ~~"选择视频文件"每次都从系统默认目录开始翻~~ 已记住上次添加视频的目录(`config.last_dir`),下次打开文件选择框默认跳到那,批量处理一整个季不用反复翻目录
- [x] ~~已有同名字幕且不覆盖时,还会白白跑完 ffmpeg 提取 + whisper 识别才告诉你跳过了~~ 跳过判定提前到开工前,见 `job::existing_srt_skip_reason`/`run_batch`
- [x] ~~ffmpeg 取消/失败后 tmp/ 残留半截 wav,断点续跑会把它误判成完整音轨复用~~ 取消/失败时删除半截 wav(实测确认取消后确实残留部分文件),见 `engine::process_video`
- [x] ~~配置损坏横幅只在主界面 batch_bar 出现,损坏配置 + 缺组件时首次下载/设置页看不到"模型位置已恢复默认"的说明~~ 横幅现在在首次下载/设置页也渲染,并统一在顶部面板渲染一次,不再被重复画
- [x] ~~首次下载页只能下 large-v3,配置一般的电脑被迫白下 2.9GB 才能去设置里换 medium/turbo~~ 首次下载页可直接选语音识别模型档位
- [x] ~~下载/搬迁页点"取消"没有即时反馈(下载线程可能卡在重试 sleep 或解压中),用户会连点或关窗~~ 点取消立即变灰显示"正在取消…"
- [x] ~~清理重复后再扫描,把 _trash 软删除区里已清走的副本又当成新重复,越清越多~~ 目录遍历跳过 `_trash` 子目录
- [x] ~~空状态引导卡选视频全被忽略时毫无反应~~ 与左侧队列面板一致弹"没有添加任务"反馈
- [x] ~~批次跑完退出确认框还挂着,文案却说过时了~~ 批次结束自动关掉确认框
- [x] ~~处理中还能点"扫描本机已有模型",遍历几十 GB 缓存跟批次抢磁盘 IO~~ 处理中禁用扫描按钮
- [x] ~~批次 ETA 被跳过项污染(跳过项开工前即判定、几乎不耗时,算进分母把 ETA 拉到近似 0;又把已跳过的项算成剩余任务高估时间)~~ 剩余时间只按真跑过流水线的任务外推,剩余数量用未到终态数
- [x] ~~下载进度 2.95GB 显示成 2950.0 MB~~ ≥1GB 时显示 GB
- [x] ~~翻译模型加载文案"首次加载"在同批次切模型时误导~~ 去掉"首次"二字
- [x] ~~双语 .zh.srt 预览把中文和原文糊成一行~~ 预览只显示中文行
- [x] ~~清空列表后旧复核报告仍挂着(下标失效或被新文件占用,显示错误文件的报告)~~ 清空列表同时清掉复核报告状态;重新处理也清掉该文件的旧报告
- [x] ~~大批次长时间运行时界面内存日志无限膨胀~~ 内存日志截断到最近 2000 行,完整记录在磁盘 app.log
- [x] ~~处理中只读摘要显示 large-v3 这种内部 key~~ 显示友好档位名,与可编辑表单/首次下载页一致
- [x] ~~"组件与存放位置"窗口只列模型,引擎/ffmpeg/翻译引擎二进制缺失时界面提示去装却装不了~~ 窗口同时列出二进制组件,可直接下载补齐
- [x] ~~首次下载(几 GB)时 alt-tab 到别的窗口看不到下载进度~~ 任务栏标题带"第 N/M 项 · 百分比";总大小未知时不带空百分比尾巴
- [x] ~~Windows 记事本另存为 UTF-8 的 srt 会写 BOM,第一个字幕块被跳过~~ srt 解析跳过文件头 UTF-8 BOM(实测确认)
- [ ] **已有原文字幕 + 开翻译 + 不覆盖时,整个任务直接跳过、翻译也不做**:用户意图是"已有字幕只想翻译成中文",却得到"跳过: 已有同名字幕"。修法是把 `run_batch` 改成可分支——跳过原文时直接用已存在的 srt 当输入跑翻译,需新增"结果来源"状态且要真实模型验证,暂不处理,详见 `docs/known-issues-2026-08-09.md`
- [ ] **模型清单窗口每帧递归扫描模型存放目录(性能隐患,非正确性 bug)**:窗口开着时每帧最多做 6 次全目录递归遍历,存放目录指向 HF 缓存(几千个文件)时拖慢界面。修法需给模型状态加缓存+失效条件,暂不处理,详见 `docs/known-issues-2026-08-09.md`
- [ ] Sakura 的批量编号翻译提示词和官方单段落格式有出入(见上文",翻译走批量 + 编号对齐"一条),没有和逐行调用做过量化的翻译质量对比,只保证了格式对齐可靠、不阻断任务
- [x] ~~翻译阶段取消的延迟:取消只在批边界生效,正在请求中的单次 HTTP 请求(自带 120 秒超时)要等它结束才停~~ 已修复:阻塞请求放后台线程、主线程每 100ms 轮询取消,点"取消"后当前请求会在 ~100ms 内被打断(返回"已取消"),不再干等超时;已用本地"接受连接但不响应"的假服务单测验证(`send_cancelable_honors_cancel_while_request_in_flight`)
- [ ] **翻译跨批上下文携带(`translate::build_prompt` 的 `context` 参数)没有用真实模型验证过实际效果**:每批开头会附上一批最后 2 行(原文, 译文)当参考,明确要求模型"不要翻译它、不要复述它",目的是缓解人称/称呼在批边界处漂移。这段提示词的措辞是写代码时设计的,没有在真机上拿 Sakura/Qwen3 跑一遍肉眼确认过"模型是否真的遵守了不复述的指令、编号对齐会不会因为多了这段参考文本而变得不可靠"——如果实测发现 Sakura 把参考内容当成正文的一部分复述/翻译了出来,先尝试把 context 缩到 1 行;仍不行就只在 General(Qwen)分支启用 context、Sakura 分支传空切片禁用这个功能,不要在没有实测支撑的情况下假设它对两个模型都有效
- [ ] 暂无打包成单文件安装包/自动更新机制,`cargo build --release` 出的就是最终产物
- [ ] 全片末尾约 68 秒偶尔观察到大小写/标点丢失(`-mc 0` 的副作用,规模很小),暂未处理
- [x] ~~暂无自动化测试~~ `src/engine/`、`src/translate/`、`src/setup/`、`paths.rs`、`job_guard.rs` 里有 `cargo test` 覆盖复读折叠逻辑(含合成的 983 次重复场景)、语言检测解析、翻译编号对齐解析(含真实 Sakura 模型跑出来的响应样本)、模型选择逻辑、模型目录搬迁(跳过/覆盖/取消/目录不存在等边界情况)、断点续跑判定(含真实崩溃场景遗留的字幕文件)、Job Object 子进程终止机制(真实拉起子进程验证),以及本轮新增的后台线程 panic 兜底、配置原子写入/损坏恢复、临时文件过期清理、复核报告生成与修正回写。翻译子系统和模型目录搬迁都已经用真实模型/真实跨盘环境做过端到端验证(RTX 3060 实测:llama-server 复用 whisper 引擎 CUDA 运行库成功用上 GPU、15 行批量翻译编号完全对齐、whisper 转写与 llama-server 常驻并存时两者都能正常拿到 GPU、真实 F 盘→C 盘跨盘搬迁内容逐字节校验一致),不是只过了单元测试

---

## 第三方组件许可

本项目源代码为 MIT 协议,但首次运行下载的 whisper.cpp、FFmpeg 是各自项目按各自许可证分发的独立二进制,不随本仓库源码分发,请自行查阅对应项目的许可证。
