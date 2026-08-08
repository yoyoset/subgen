# `app.rs` 职责边界

这份映射按当前源码中的真实方法边界记录,不是按行数平均切分。新增功能应先判断它回答
哪个问题;如果不属于现有边界,应放进独立模块。CI 中 `scripts/check-line-budgets.ps1`
把 `app.rs` 的总量限制为 1300 行,防止新功能再次把所有逻辑堆回这个文件。

| 范围 | 当前职责 | 主要入口 |
|---|---|---|
| 11-208 | 应用状态结构、异步操作槽位和初始状态 | `SetupPhase`, `SubgenApp`, `new` |
| 210-402 | 首次组件检查、下载、模型目录切换和搬迁轮询 | `begin_setup_check`, `start_downloading`, `begin_change_models_dir`, `poll_migrate` |
| 404-496 | 视频文件入队和批次处理启动 | `add_files`, `start_processing` |
| 498-656 | 翻译复核报告生成及用户修正回写 | `generate_review_report`, `apply_review_corrections` |
| 658-1008 | 模型清单的定位、校验、扫描、安装和重复文件清理 | `begin_locate_model`, `begin_verify_candidate`, `begin_inline_install`, `begin_scan_for_models`, `begin_cleanup_duplicates` 及对应轮询 |
| 1010-1129 | setup、模型校验、扫描、清理和 worker 事件消费 | `poll_setup`, `poll_verify`, `poll_scan`, `poll_cleanup`, `poll_worker` |
| 1132-1286 | egui 生命周期、关闭确认、拖放处理、页面分发和退出落盘 | `eframe::App for SubgenApp` |
| 1288-1295 | 模型文件魔数读取的局部辅助函数 | `read_magic` |

## 这次不做

- 不再次拆分 `app.rs`:本次目标是先建立防回长门槛,不是重复上一次的结构搬迁。
- 不为每个方法建立抽象 trait:当前没有独立复用需求,会增加状态借用和测试成本。
- 不改变 UI 行为或异步生命周期:治理机制只在 CI 阶段阻止超预算提交。
