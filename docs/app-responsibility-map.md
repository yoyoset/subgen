# `app.rs` 职责边界

这份映射按当前源码中的真实方法边界记录,不是按行数平均切分。新增功能应先判断它回答
哪个问题;如果不属于现有边界,应放进独立模块。CI 中 `scripts/check-line-budgets.ps1`
把 `app.rs` 的总量限制为 1300 行,防止新功能再次把所有逻辑堆回这个文件。

**总原则**:`app.rs` 只做"应用状态编排 + egui 生命周期",不承载具体业务。所有能抽走的
核心逻辑都已经抽到独立模块——翻译复核的文件操作在 `report.rs`,首次下载在 `setup/`,
处理流水线在 `engine/`,翻译在 `translate/`,界面在各 `ui/*` 面板。新功能先判断该住进
哪个模块,只有"要访问 SubgenApp 状态、又没法归入现有模块"的部分才允许进 `app.rs`。

| 范围 | 当前职责 | 主要入口 |
|---|---|---|
| 11-202 | 应用状态结构、异步操作槽位、初始状态,以及后台线程通用 panic 兜底 | `SetupPhase`, `SubgenApp`, `new`, `spawn_guarded_thread` |
| 241-482 | 首次组件检查、下载、模型目录切换和搬迁轮询 | `begin_setup_check`, `start_downloading`, `begin_change_models_dir`, `commit_models_dir`, `poll_migrate` |
| 483-554 | 视频文件入队和批次处理启动 | `add_files`, `start_processing`, `VIDEO_EXTENSIONS` |
| 558-633 | 翻译复核报告的界面状态编排(文件读取/粗筛/格式化/写盘在 `report.rs`) | `generate_review_report`, `apply_review_corrections` |
| 634-755 | setup/worker 事件消费与批次标题更新 | `poll_setup`, `poll_worker` |
| 756-863 | egui 生命周期、关闭确认、拖放处理、轮询驱动和退出落盘(页面分发在 `ui/setup_screens::render_phase`) | `eframe::App for SubgenApp` |
| 864-926 | 本文件内测试 | `tests` |

## 模型管理操作(已拆出,不在 app.rs)

模型清单的定位/校验/绑定/内联安装/扫描/清理这些方法以 `impl SubgenApp` 的形式住在
`src/models/actions.rs`(和 `begin_locate_model` 等是同一个类型,调用方式不变)。拆出去
是因为它们是一个连贯的职责、且体积不小——app.rs 的行数预算(1300)是治理门槛,不是
"必须堆在 app.rs 里"的理由。当前 `src/app.rs` 约 926 行。

相邻模块职责速查:

- `report.rs`:翻译复核报告的读取/粗筛/格式化/写盘 + 粘贴修正的文件重写,是 `app.rs`
  对应方法的核心逻辑,独立成模块后可以脱离 UI 单测(见其中的 `apply_corrections`/`generate_review_report` 测试)。
- `models/actions.rs`:`SubgenApp` 的模型管理操作(定位/校验/绑定/内联安装/扫描/清理)。
- `ui/theme.rs`:唯一的语义色板和全局视觉风格;界面里要颜色一律从这里取,不硬编码 RGB。
- `ui/*.rs`:各面板渲染,只读 `SubgenApp` 状态、把用户意图回填,不实现业务。

## 这次不做

- 不继续拆 `app.rs`:现状约 926 行,预算充足;若未来功能把它重新顶到上限,优先观察它
  "新回答了什么问题",再决定拆去哪,而不是为了拆而拆。
- 不为每个方法建立抽象 trait:当前没有独立复用需求,会增加状态借用和测试成本。
