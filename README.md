# subgen —— 字幕生成器

给 Windows 上的视频文件生成字幕(.srt),本地跑,不联网上传你的视频/字幕内容。绿色软件,不装到系统里。

语音识别用 whisper.cpp;**可选**再用本地大模型把字幕翻译成中文(也是本地跑,不联网上传字幕内容)——日语走 [Sakura](https://github.com/SakuraLLM/SakuraLLM)(专门用轻小说语料微调的日→中模型,效果明显好于通用模型),其他语言走 Qwen3 通用模型,自动按识别出的源语言选择,也可以手动指定。

---

## 用法

1. 双击 `subgen.exe`(或 `cargo run` 跑起来)
2. **首次运行**会自动下载必要组件(识别引擎、FFmpeg、语音模型;如果勾选了翻译还会下载翻译引擎和翻译模型)。`engine/`、`ffmpeg/`、`translator/`(体积小,约 1GB 以内)固定存在 exe 同目录;`models/`(体积大,语音模型+翻译模型加起来能到 10GB+)**可以改存放位置**——首次下载界面和主界面顶栏都有"模型位置: ... [更改...]"这一行,点"更改..."选个别的盘,不想让 C 盘被吃掉一大截的话就用这个。已经下载过的模型文件会自动搬过去,不用重新下载
3. 把视频文件拖进窗口,或点"选择视频文件..."按钮(支持多选)
4. 需要中文翻译的话,勾选"生成中文翻译(本地模型)",可以选翻译模型(自动按语言 / 固定用 Sakura / 固定用 Qwen3)和输出格式(双语字幕 / 仅中文)
5. 点"开始处理",逐个跑完。每个文件的处理过程分"提取音轨 → 识别 → 翻译"三步(没开翻译就两步),界面上有个小步骤条实时显示卡在哪一步、哪一步已经完成。原文字幕生成在**视频同目录、同名**的位置(如 `EpisodeName.mkv` → `EpisodeName.srt`);勾选了翻译的话额外生成 `EpisodeName.zh.srt`(双语时中文在上、原文在下)。大多数播放器(包括 Jellyfin)会自动识别加载这些外挂字幕

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

如果自动下载失败(网络问题),可以按上表链接手动下载,分别解压/放到 exe 同目录的 `engine/`、`ffmpeg/`、`translator/` 文件夹里(zip 解压进去就行,程序会自动递归找里面的可执行文件,不要求固定的子目录层级),模型文件(语音模型/VAD/翻译模型)直接放进 `models/` 文件夹,不用解压。

如果你本地已经用其他工具(比如 LM Studio、text-generation-webui)通过 HuggingFace 下载过 Sakura 或 Qwen3 的 gguf 文件,首次下载时程序会自动扫描 `HF_HOME`/`HF_HUB_CACHE` 环境变量和 `~/.cache/huggingface` 默认缓存目录,哈希校验通过就直接复制过来,不会重复下载。

**更进一步:模型目录可以直接指向 HuggingFace 缓存本身**(比如 `F:\hf_cache`),不用复制。程序在这个目录下查找模型文件时是递归查找(和查找 whisper.cpp/ffmpeg/llama.cpp 可执行文件用的是同一套逻辑),不要求平铺结构,能直接在 HF 缓存自己的 `hub/models--xxx/snapshots/<commit>/文件名` 这种嵌套目录里找到已有文件——把"模型存放位置"(见下文)改指到 HF 缓存目录,就是和其它工具共用一个模型池,不用各自留一份。

### 模型存放位置

默认在 exe 同目录的 `models/` 文件夹,但可以改到任何盘、任何目录(比如 exe 图方便放在 C 盘桌面,但模型想放到空间更大的 D/E/F 盘)。改法:首次下载界面或主界面顶栏点"更改...",选个文件夹即可——

- 选中的目录不存在会自动创建;创建/写入失败(比如选了只读的网络共享)会当场报错,不会拖到真正下载时才发现。
- 如果旧目录下已经有下载好的文件,会自动搬到新目录(同盘瞬间完成,跨盘走复制,搬迁过程中断电/被强关也不会丢文件——没搬完的部分原样留在旧目录,不会出现两边都没有的情况),不用因为换了目录就重新下载十几 GB。
- 下载前会检查目标盘剩余空间够不够,不够会有醒目提示(不会硬性阻断,因为部分组件大小是估算值)。
- 处理任务进行中不能改(避免正在跑的批次里,后面文件要用的模型路径突然变了)。

## 硬件要求

- NVIDIA 显卡(实测 RTX 3060 12G:large-v3 语音识别约 8 倍实时速度,一集 45 分钟的剧大概 5-7 分钟出字幕;翻译模型 7B/4B 量级在 12GB 显存上和识别模型一起跑也没问题——同一批次里翻译引擎常驻显存、识别引擎按文件启停,两者短暂重叠时显存也够)
- 没有 N 卡也能跑,但会退化成纯 CPU,慢很多。程序会在处理时检测是否真的用上了 GPU,没用上会在界面上醒目提示(目前这个检测只覆盖语音识别阶段,翻译阶段的 GPU 检测还没做,见下文 TODO)

## 卸载

删掉整个文件夹就行。不写注册表、不装到系统目录。

---

## 给开发者

### 构建

```
cargo build --release
```

产物在 `target/release/subgen.exe`,单文件,把它拷到任何地方运行都行(首次运行还是会自动下载组件到它所在目录)。

### 技术栈

- **界面**:[egui](https://github.com/emilk/egui) / eframe(纯 Rust,无 Node.js/webview 依赖)+ [rfd](https://github.com/PolyMeilex/rfd)(原生文件选择框)
- **识别引擎**:[whisper.cpp](https://github.com/ggml-org/whisper.cpp) 官方预编译二进制,子进程调用(不是 FFI 链接),好处是不用在编译本项目时碰 CUDA 工具链
- **音频提取**:ffmpeg(便携版,子进程调用,把视频转成 16kHz 单声道 wav —— **whisper-cli 不能直接读视频文件**,只支持 flac/mp3/ogg/wav,这是实测确认过的,不是文档假设)
- **翻译引擎**:[llama.cpp](https://github.com/ggml-org/llama.cpp) 官方预编译二进制里的 `llama-server`,子进程调用,通过它的 OpenAI 兼容 HTTP API(`/v1/chat/completions`)喂字幕文本、拿翻译结果——和识别引擎同一套"不链接、不碰 CUDA 工具链"的哲学

### 项目结构

```
src/
  main.rs      egui 应用入口 + 界面(注意:eframe 0.35 的 App trait 用 fn ui(&mut self, ui: &mut egui::Ui, ...),
               不是旧版本熟悉的 fn update(&mut self, ctx: &egui::Context, ...);面板 .show() 也相应地接
               &mut Ui 而不是 &Context —— 这版本 API 和网上大多数旧教程不一样,踩过坑,如果以后升级
               eframe 版本要注意再核实一遍)
  setup.rs     首次组件检测 + 下载 + 解压 + 哈希校验(含翻译引擎/翻译模型,支持从本地 HuggingFace
               缓存复用已下载过的模型文件)
  engine.rs    调用 ffmpeg + whisper-cli 的核心处理逻辑,GPU 检测、进度解析、源语言检测、
               断点续跑(复用 tmp/ 里已有的中间产物)
  translate.rs 调用 llama-server 做字幕翻译:模型注册表("语言→模型"映射)、子进程生命周期管理、
               分批翻译 + 编号对齐校验 + 失败重试
  job.rs       任务队列(含 Stage 枚举:提取音轨/识别/翻译),后台线程串行处理,通过 channel
               回传阶段化状态给界面
  job_guard.rs 用 Windows Job Object 绑定所有子进程的生命周期,主程序不管怎么退出,ffmpeg/
               whisper-cli/llama-server 都会被系统一起终止,不留孤儿进程
  paths.rs     所有路径默认相对于 exe 所在目录(绿色软件要求),组件路径用递归搜索文件名而不是硬编码
               内部目录结构(whisper.cpp/ffmpeg/llama.cpp 官方压缩包内部路径可能随版本变化,
               models_dir() 同样递归查找,支持直接指向 HuggingFace 缓存这类嵌套结构);
               models_dir() 额外支持进程级覆盖(见"模型存放位置"),用户可以让它不跟着 exe 走
  config.rs    便携配置(config.toml,存在 exe 同目录)
```

### 已知的技术事实(写代码时实测验证过,不是猜的)

- whisper.cpp 官方 Windows CUDA 预编译包(v1.9.1)解压后,可执行文件在内部的 `Release/` 子目录,但代码用递归搜索找 `whisper-cli.exe`,不依赖固定路径
- whisper-cli 用 miniaudio 解码音频,**不支持 mp4/mkv 等视频容器**,必须先 ffmpeg 转 wav
- GPU 是否真正用上,看 whisper-cli 输出里有没有 `using CUDA0 backend` 这一行;显式禁用 GPU(`-ng`)时会输出 `no GPU found`。**已知 whisper.cpp 在部分 Windows 环境下会静默回退 CPU 而不报错**,所以必须解析这行日志来判断,不能假设"没报错就是用了 GPU"
- 进度行格式:`whisper_print_progress_callback: progress = NN%`,NN 可能超过 100(whisper.cpp 自身的估算误差),界面展示时要 clamp;**大文件/复杂内容第一次进度更新可能要等一两分钟才出现**,期间界面靠"已运行 XXs"计时器提示未卡死,而不是干等一个不动的 0%
- **`--vad` 默认关闭,不是开启**(踩过坑,方向和最初设计相反)。实测拿同一段真实纪录片音频对比:开 `--vad` 会导致输出全部丢失大小写和标点(比如 "Jeremy Wade, explorer" 变成 "jeremy wade explorer"),不开 VAD 则完全正常、且没有观察到幻听/回退重试(fallbacks=0)。VAD 的防幻听收益只在纯音调/长静音场景验证过("(phone ringing)" 这种无中生有的内容),对持续对白类内容(纪录片、剧集)VAD 弊大于利,所以 v1 默认不开
- **GUI 主程序用 `windows_subsystem = "windows"` 关掉了自身的控制台窗口**(release 构建才生效,debug 构建保留控制台方便看日志),但这引出一个连带问题:**子进程(ffmpeg / whisper-cli)如果不显式处理,Windows 会单独给它们弹一个新控制台窗口**(实测确认过,主程序一关控制台,点"开始处理"就弹出一个 ffmpeg/whisper-cli 的黑窗口)。用户如果手滑关掉那个弹出的控制台窗口,等于给子进程发了 SIGTERM,会导致"音频提取失败"之类的误导性报错(看着像文件损坏,其实只是窗口被关了)。修复:两个 `Command`(ffmpeg、whisper-cli)都加 `creation_flags(CREATE_NO_WINDOW)`(`0x0800_0000`),子进程不再弹窗,stdout/stderr 依然通过管道正常捕获,不影响进度解析
- **whisper.cpp 长文件"复读机"故障 —— 根因已定位并修复**:实测在一个约 44 分钟的真实文件上遇到过解码卡死循环,同一句字幕原样重复了 983 次,后面的内容全部丢失。第一次只加了后处理安全网(`collapse_repetitions`,检测连续 ≥4 条完全相同的字幕并折叠成 1 条),用户重新处理同一文件后**在完全相同的位置(00:08:06)再次触发**——说明问题是确定性的,不是随机的,安全网只是把垃圾行折叠好看了,但丢失的 32 分钟内容并没有找回来,必须治根因。根因:whisper 默认把前一个 30 秒窗口识别出的文本作为下一个窗口解码的上下文("condition on previous text"),一旦某句话重复进入上下文,就会自我强化、越来越确信"接下来还是这句",从而卡死到文件结束——这也解释了为什么单独截出来的 4 分钟/15 分钟短片段(上下文是干净的)怎么都无法复现。修复:加 `-mc 0`(`--max-context 0`)禁止跨窗口携带文本上下文。已在同一份真实故障音频上验证:加参数后重复计数从 983 降到 0,fallbacks 从 5 降到 0,全片 44 分钟内容完整识别。代价很小,只在全片末尾约 68 秒观察到大小写丢失,规模远小于此前的复读故障,暂未处理。`collapse_repetitions` 安全网继续保留,作为纵深防御
- **SakuraLLM 只训练了日→中方向**,拿它翻英文等其他语言效果很差(不在训练分布内),所以 `translate.rs` 里用"语言→模型"注册表(`translate::MODELS`)按 whisper 检测出的源语言选模型:日语走 Sakura,其余语言走通用的 Qwen3-4B-Instruct,而不是无脑一个模型翻所有语言
- **翻译走批量 + 编号对齐,不是逐行请求**:整集字幕逐行发 HTTP 请求太慢(一集可能几百条),所以每批 15 行一起发给模型,提示词里显式给每行编号、要求模型保留编号原样输出,回复按编号解析对齐;行数对不上(模型漏译/多译/合并了某几行)就对半拆开重试,缩小出错范围,最终还是对不齐的单行原样保留原文、不阻断整个任务(`translate::translate_batch_with_retry`)。这也是为什么 Sakura 用的提示词和官方文档给的单段落格式不完全一样——官方格式没有编号,是拿吞吐量换来的妥协,如果实测发现批量编号格式明显拉低 Sakura 翻译质量,后续可以把 `BATCH_SIZE` 调小
- **llama-server 复用 whisper 引擎目录里的 CUDA 12.4 运行库 DLL**,不单独下载一份约 400MB 的 cudart 运行库包:两者都是 CUDA 12.4 构建,`translate::LlamaServer::start` 把 whisper 引擎所在目录塞进子进程 PATH 前部,Windows 动态链接器就能找到 `cudart64_12.dll`/`cublas64_12.dll`/`cublasLt64_12.dll`,免掉重复下载
- **批次内 llama-server 常驻、按需重启**:一个视频转写完立刻用同一个 `llama-server` 翻译,启动一次要把几 GB 模型加载进显存,不可能每个文件都重启;只有当这批文件里下一个视频需要的翻译模型和当前跑着的不一样时(比如"自动"策略下前一个是日语用 Sakura、下一个是英语要切到 Qwen3)才 kill 掉重启。批次处理完 / 被取消时显式 stop,`Drop for LlamaServer` 兜底 kill,避免异常退出路径下留下孤儿进程占着显存
- **模型目录覆盖是进程级全局状态,不是把 Config 一路传参传下去**:`setup.rs`/`engine.rs`/`translate.rs` 深层调用 `paths::model_path()`/`vad_model_path()`/`trans_model_path()` 的地方都拿不到 `Config`,真要一路传参改动面太大,所以用 `paths.rs` 里一个 `Mutex<Option<PathBuf>>` 静态变量(`set_models_dir_override`/`models_dir()`),main.rs 启动时用配置文件里的值设一次,用户在界面里改存放位置时重新设一次,下游所有路径函数自动生效不用改调用方
- **换模型目录时,旧目录里已有文件的搬迁是"尽力而为、不因单个文件出错中断整体"**:`setup::migrate_models_dir` 遇到某个文件搬不动(比如目标已有同名但大小不同的文件、复制失败)只记一条问题继续搬下一个,不会因为一个文件卡住就让后面几个 GB 的文件全跳过不搬;不管过程顺不顺利、是否被取消,调用方都会把生效目录切到新位置——已经搬过去的文件才是准确的当前状态,没搬完的缺口交给现成的 `missing_components()` 首次下载流程去补,不需要为这个中间状态写特殊分支。跨盘搬迁是"复制到临时名 → 核对大小 → 原子改名 → 删源文件",任何一步中断源文件都还在原地,不会两边都丢
- **子进程用 Windows Job Object 绑定生命周期,而不是靠自己的清理代码**:真实遇到过一次主程序意外退出(大概率是 wgpu 渲染层的"设备丢失直接 panic"——它默认行为就是这样,和 whisper-cli/llama-server 抢同一块物理 GPU 时有一定概率触发),whisper-cli 子进程当场变孤儿、自己又跑了半个多小时,GPU 占着、临时文件也没人清理。`std::fs::remove_file` 这类"退出前清理"代码天然靠不住,因为进程崩溃的那一刻恰恰是这些代码来不及跑的时候。修复用 `win32job` crate 创建一个带 `KILL_ON_JOB_CLOSE` 限制的 Job Object,ffmpeg/whisper-cli/llama-server 一 spawn 出来就绑进去——这是操作系统内核层面的保证,主程序不管怎么死,子进程都会被一起干掉,不依赖任何应用层代码执行。用真实拉起的子进程验证过这个机制确实生效(`job_guard::tests::dropping_the_job_kills_bound_child_process`)
- **断点续跑靠文件是否存在,不是额外的状态记录**:`tmp/` 里的临时文件名本来就是"{文件名}_{路径哈希}.wav/.srt"和视频一一对应,`engine::process_video` 重新处理某个文件前会先检查这两个中间产物是不是已经存在且完整(srt 用 `srt_looks_complete`:内容为空或者能解析出至少一块都算完整),完整就直接复用、跳过重新跑那一步。复用识别结果时没有真实跑 whisper-cli,`used_gpu` 用 `Option<bool>` 表示"这次没有观测"(不是 false),避免误报 GPU 警告;`detected_language` 会尝试从同一个哈希对应的旧日志文件里恢复,找不到就退化成 "auto"

### 哈希校验的可信度分级

- `engine`(whisper.cpp)、`ffmpeg`、`vad` 模型、`model`(large-v3)、`translator`(llama.cpp)、两个翻译模型(Sakura/Qwen3):哈希都是通过 GitHub Release API 的资产 digest 字段或 HuggingFace LFS 指针文件核实过的,large-v3、ffmpeg 额外做过一次完整下载后本地 `sha256sum` 复核,两者一致;校验不过**阻断安装**
- `ffmpeg` 早期用的是 gyan.dev 滚动更新的"最新构建"链接,没有可钉死的版本号/哈希,只做存在性检查;已改用 [GyanD/codexffmpeg](https://github.com/GyanD/codexffmpeg/releases) 在 GitHub 上带版本号的正式发布(内容与 gyan.dev 官网一致),可以像其他组件一样钉版本号 + 强哈希校验,不再依赖一个随时可能变化的滚动链接

如果以后 whisper.cpp/ffmpeg/llama.cpp 出新版本、想升级绑定的版本号,去 `src/setup.rs` 顶部改对应的 `_URL`/`_SHA256` 常量,记得重新下载一遍算真实哈希,不要瞎编。

### TODO / 已知小问题

- [x] ~~"剩余 N 项"计数不准~~ 已修复,改成"第 N/总数 项"
- [x] ~~大文件处理时进度长时间不动,看着像卡死~~ 已加"已运行 XXs"计时器 + 首次更新慢的提示文案(见 `job.rs` 的 `started_at`、`main.rs` 的 `format_elapsed`)
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
- [ ] 翻译阶段没有 GPU 使用检测(语音识别阶段有,`used_gpu` 靠解析 whisper-cli 日志里的 `using CUDA0 backend`;llama-server 的日志目前只是排空、没解析,所以翻译如果意外掉到纯 CPU 不会有界面提示,只会感觉很慢)
- [ ] Sakura 的批量编号翻译提示词和官方单段落格式有出入(见上文",翻译走批量 + 编号对齐"一条),没有和逐行调用做过量化的翻译质量对比,只保证了格式对齐可靠、不阻断任务
- [ ] 暂无打包成单文件安装包/自动更新机制,`cargo build --release` 出的就是最终产物
- [ ] 全片末尾约 68 秒偶尔观察到大小写/标点丢失(`-mc 0` 的副作用,规模很小),暂未处理
- [x] ~~暂无自动化测试~~ `engine.rs`/`translate.rs`/`setup.rs`/`paths.rs`/`job_guard.rs` 里有 `cargo test` 覆盖复读折叠逻辑(含合成的 983 次重复场景)、语言检测解析、翻译编号对齐解析(含真实 Sakura 模型跑出来的响应样本)、模型选择逻辑、模型目录搬迁(跳过/覆盖/取消/目录不存在等边界情况)、断点续跑判定(含真实崩溃场景遗留的字幕文件)、Job Object 子进程终止机制(真实拉起子进程验证)。翻译子系统和模型目录搬迁都已经用真实模型/真实跨盘环境做过端到端验证(RTX 3060 实测:llama-server 复用 whisper 引擎 CUDA 运行库成功用上 GPU、15 行批量翻译编号完全对齐、whisper 转写与 llama-server 常驻并存时两者都能正常拿到 GPU、真实 F 盘→C 盘跨盘搬迁内容逐字节校验一致),不是只过了单元测试

---

## 第三方组件许可

本项目源代码为 MIT 协议,但首次运行下载的 whisper.cpp、FFmpeg 是各自项目按各自许可证分发的独立二进制,不随本仓库源码分发,请自行查阅对应项目的许可证。
