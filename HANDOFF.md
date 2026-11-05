# HANDOFF：写给接手 subgen 的下一个 AI

你好。这封交接信写于 2026-11-05（Claude Opus 5.5），汇总了 2026-07 以来各轮会话的记忆、提交记录和 `docs/` 里的评审与报告。信里讲五件事：项目是什么、现在到了哪一步、怎么构建、测试和发布、为什么这样设计、踩过哪些坑。
写给使用者 AI 的安装清单是 [SETUP_FOR_AI.md](SETUP_FOR_AI.md)（外部资产的 URL、大小和 sha256），面向用户的说明在 [README.md](README.md)。README「给开发者」一节里的「已知的技术事实」和「TODO」写得很扎实，改代码前要读。

> 用户的全局工作原则，请照做：
> - 先实测，再下结论；实测和早先的假设矛盾时，如实写"以为是 A，实测是 B"。
> - 绝不能出现"后台失败，用户却以为成功"。
> - 只按路径 `git add`，每个编号项或独立改动单独提交，commit message 写真实依据（数字、复现步骤），并写明"这次不做什么"和理由。
> - 构建和测试全绿才提交。
> - 本机是 Windows，主 shell 是 PowerShell，git bash 在 `D:\PortableGit\bin\bash.exe`。

---

## 1. 项目是什么，现在什么状态

subgen 是一个 Rust（egui）写的 Windows 桌面工具：拖进视频，生成字幕，可以选择翻成中文、输出双语字幕。整个流程全部在本机跑：
- ffmpeg 抽音频
- whisper.cpp 识别（CUDA 12.4）
- llama.cpp `llama-server` 翻译：日译中用 Sakura-14B-Qwen3-v1.5，其他语言用 Qwen3-14B 兜底
- 排版（折行、拆块）
- 写出 `.srt` / `.zh.srt`

程序只有一个 exe。引擎、ffmpeg 和模型在首次运行时下载，支持哈希校验、断点续传和镜像回退。

- 仓库 `yoyoset/subgen`：public。有 GitHub Actions CI（`.github/workflows/ci.yml`），跑的检查和 `scripts/verify.ps1` 一样。
- **v0.1.0 已发 Release（2026-11-05）**：https://github.com/yoyoset/subgen/releases/tag/v0.1.0 ，`subgen-0.1.0-win-x64.zip` 6,122,165 B（只有 `subgen.exe` + README + LICENSE），sha256 `5d4687c49e36ecfacbd2153797803c4660e129fdf08c1bd8d9133fdf47f8f769`；下回来核过哈希。解压到全新目录启动直接进首次下载界面（PrintWindow 截图确认），**完整首次下载流程这次没重跑**（几 GB）。下载页上架由 SUBLOG 的一次性定时任务 11-06 10:00 执行。
- 最近一次实质改动在 11-02：日译中模型从 Sakura-7B 换成了 14B，补了模型许可表，新增了 SETUP_FOR_AI.md 和赞助区。
- 11-04 有一个修 clippy unused variable 的提交（`678822e`）。CI 连续 4 次因为它变红，这个提交 11-05 已经推上去了。
- 工作区里有两份 08-09 的未跟踪文档（`docs/handover-2026-08-09-…`、`docs/review-2026-08-09-…`），不清楚是不是有意不入库。我没动，你也先别动，先问用户。

## 2. 命令

```powershell
powershell -File scripts/verify.ps1     # release 构建零警告 + cargo test + clippy -D warnings + 行数预算；全过才算能交付
cargo build --release                   # → target\release\subgen.exe（单文件）
cargo test
```

- 测试约 207 个（按 `#[test]` 计）。
- `dist\subgen.exe` 是**手动维护的本机部署副本**，用户平时用的就是它。`cargo build` 不会更新它，改了影响运行行为的代码以后，要手动执行 `cp target\release\subgen.exe dist\subgen.exe`。subgen 正在运行时覆盖会失败。
- 真实测试素材在 **E 盘**（例如 `E:\River Monsters S08\`、`E:\season 11`），不在项目目录里。找素材用 PowerShell 的 `Get-ChildItem -Path E:\ -Filter "*关键词*" -Recurse`，git bash 的 `find` 跨盘很慢。

**发布步骤（v0.1.0 就是这么做的）：**
1. `verify.ps1` 全绿。
2. 包里**只放 `target\release\subgen.exe`、`README.md`、`LICENSE`**，放在 `subgen/` 目录里，用 `%SystemRoot%\System32\tar.exe -a -c -f subgen-<ver>-win-x64.zip subgen` 打包（bsdtar，别用 Git 的 GNU tar）。引擎和模型首次运行时下载。
3. **`dist\` 里的东西一律不放进包**：`logs/`、`tmp/`、`config.toml`（里面记着用户上次打开的目录）、`subgen.exe.bak-*`、`subgen.lock`、`models/`、`engine/`、`ffmpeg/`、`translator/`。从 `target\release` 取全新的 exe。
4. 先解压到一个空目录跑一遍首次下载流程，再上传：`& "C:\Program Files\GitHub CLI\gh.exe" release create ...`（gh 要写全路径，账号 yoyoset 已登录）。上传后把文件下回来核对 sha256。
5. 发布打包没有脚本。可以参考 comicgen 的 `scripts/build_release.py`：发现用户数据就拒绝出包。

## 3. 关键决策和为什么

1. **翻译模型换成 Sakura-14B**（11-02）。用 16 句日语台词做过一次小样本对比：7B 把单数的「おい」译成了"你们"，还只回了 14 行；14B 16/16 全部对齐。样本很小，只能说明方向。显存实测峰值 11776/12288 MiB，**12GB 显卡余量很小**。Sakura-14B 的许可是 **CC BY-NC-SA 4.0，不能商用**。
2. **兜底模型从 Qwen3-4B 换成了 14B**（08-22，和 comicgen 同步改的）。14B 没有纯指令版，所以 system prompt 末尾加了 `/no_think`，另外用手写的 `strip_think_block` 兜底，没有为这一处引入 regex 依赖。
3. **模型不复制，改成写绑定表**（07-25，B0–B11）。用户的 `HF_HOME=F:\hf_cache` 是多个 AI 工具共用的模型池，以前命中池里已有的文件会复制一份，Sakura 7B 因此存了两份（3.96 GB）。现在 `Config.model_files` 只记录文件在哪；"清理重复"要用户手动点，而且是软删除，文件挪进 `_trash`。
4. **子进程统一挂在 Windows Job Object 上**（`job_guard.rs`）。主进程退出时 whisper、llama、ffmpeg 会一起退出，不留孤儿进程。
5. **翻译质量的兜底是复核报告**（`src/report.rs`）。错位问题在算法层面还没解决（见 §5），所以提供"生成复核报告 → 贴给大模型 → 应用修正"这条路。
6. **UI 重构 Phase C/D 被用户明确暂停了**（07-25，用户的原话是"先处理译文算法问题"）。**用户没有明确说继续之前，不要主动恢复。**

## 4. 踩过的坑（现象 → 原因 → 解决）

1. **GPU 检测完全失效**：代码注释写着"日志格式来自 llama.cpp 源码，实测确认"，其实只是读过源码，真二进制根本不输出那一行。改成单独跑一次 `--list-devices` 来探测，并且用 nvidia-smi 独立核对显存（2260 → 5988 MiB）。**读源码不等于验证过运行时行为。**
2. **`PathBuf::with_extension` 吃掉多点文件名**：剧集文件名里全是点，比如 `Deadliest.Catch.S22E05...`，`with_extension("srt")` 会把文件名后半截整段替换掉。结果 whisper 明明成功了，界面却报"未生成字幕"。这个 bug 从项目第一个提交就有，修之前先写了一段最小的 rustc 代码实测，确认了行为再改。
3. **拆文件重构时凭记忆重写代码**，写错了字段类型，还把中文弯引号写成了直引号。拆分必须用 `sed` / `git show` 机械提取原文。拆完用 `cargo test -- --list` 逐个比对测试名单，不能只比数量。一个文件拆成 3 个以上时，`git status` 不显示 rename，要用 `git diff --stat -M10` 复核。
4. **临时分析脚本用错了读法**：`.zh.srt` 是双语的，`parse_srt_blocks` 会把一块里的多行拼成一行，于是每条"译文"都带着英文原文，报出 492/505 这种离谱的数字，差点当成"整批字幕都毁了"报给用户。正确读法是 `parse_srt_blocks_lines` 取第一行，真实数字是 55/505。**数字大到不合常理时，先打印一条原始数据看看。**
5. **SRT 文件开头的 UTF-8 BOM**（用记事本另存为 UTF-8 时会写入）会让第一块字幕被跳过。已修。
6. **PowerShell 5.1 下 `$ErrorActionPreference='Stop'` 和原生命令的 stderr 冲突**，会把 cargo 的进度输出当成错误。`verify.ps1` 改成显式检查 `$LASTEXITCODE`。
7. **CI 变红了 4 次没人看**：本地 clippy 用的命令和 CI 不一样，unused variable 漏到了 CI。以后推送前跑 `verify.ps1`，它和 CI 跑的是同一套检查。

## 5. 已知但还没处理的问题（详见 `docs/known-issues-2026-08-09.md` 和 README 的 TODO）

- **翻译内容错位，算法层面没修。** 根因已经定位：whisper 把一句话切成几条，模型合并翻译后仍然凑满编号，于是编号对、内容错，而且会一直错下去，直到某处碰巧重新对齐。复核报告只能抓到错位的接缝，**抓不到中段**，所以报告里的数字是低估的，跟用户沟通时不要暗示"标了 N 处就是 N 处问题"。
  已经否定的方向：few-shot 示例、加重提示词措辞。**不要再加回去。**
  还没验证的方向：翻译前用标点和大小写规则把被切碎的句子合并，翻完再按字符比例分回各条。这个用户还没表态。
- **不支持的语言**（比如巴斯克语）在识别阶段就会幻觉复读，复核报告对此无能为力。
- 全片末尾约 68 秒偶尔丢大小写和标点；跨批携带上下文没有用真实模型验证过；模型清单窗口每帧都递归扫描目录（性能隐患）。
- 11-02 的模型切换没有用图形界面完整跑过"拖入视频 → 双语字幕"；启动时的迁移横幅只靠单元测试覆盖。
- FFmpeg 构建的具体许可没有核实。
- `app.rs`（637 行）和 `engine/mod.rs`（556 行）仍超过 500 行，这是有意暂缓的。
- Sakura 7B 的历史重复文件还在用户磁盘上。正确的清理顺序是：先绑定到 `hub\` 里那一份，再点"清理重复"。顺序反了会把正确的那份删掉。

## 6. 不要做的事

- 不要把 `dist\` 目录打包或上传。
- 不要 `git add -A` / `git add .`。
- 没有用户的明确指示，不要恢复 UI 重构 Phase C/D。
- 不要重新加 few-shot 或加重提示词措辞来治错位。
- 不要自动删除用户模型池（`F:\hf_cache`）里的文件，那是好几个工具共用的。
- 换模型或换二进制时，URL、大小、sha256 一律下载后实测填写，不凭记忆。
