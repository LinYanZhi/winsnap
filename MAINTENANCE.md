# winsnap — 项目维护文档

> **本文件目录** — 项目快照式维护文档，供 AI 接手快速了解全貌。
> **最后更新**: 2026-10-01
> **更新者**: DSH session-61b7be30（workspace-project-audit 工作流）
> **过期阈值**: 30 天重检；超过 60 天 AI 不得以此为决策依据，必须 `workspace-health.mjs` 实测。
> **来源**: `README.md` + `git log` + `Cargo.toml` + 实测。

## 0. 速读（30 秒接手）

| 项 | 值 |
|---|---|
| 项目类型 | Windows 窗口管理工具 |
| 技术栈 | Rust 2024 edition + Win32 API（windows crate 0.58，DWM/UI/Threading 全栈特性）+ rdev 0.5（鼠标/键盘钩子）+ winreg 0.52 + 自有 `_shared` color 库；图标经 `build.rs` + winres 嵌入资源 |
| 主入口 | `src/main.rs` |
| 当前状态 | 🔥 近 7 天活跃（30d 内） |
| 最近 commit | 2026-09-22 |
| 接手难度 | 4/5 |
| AGENTS.md | ✗ |
| README.md | ✓ |

## 1. 这是什么

Windows 窗口管理工具。核心交互：

- **Alt + 鼠标拖拽**：移动 / 缩放窗口
- **Alt + 数字九宫格**：定位
- **贴边吸附**：DWM 可视矩形 + 一维区间裁剪
- **平滑 ease-out 动画**

架构分 11 个模块：`main` / `tray` / `snap` / `anim` / `config` / `keyboard` / `single` / `autostart` / `state` / `log` / `cmd`，三线程协作：

- 托盘线程
- 主轮询线程（~120Hz）
- 动画线程

**复杂度核心**：`tray.rs` 32KB + `main.rs` 26KB。最近两周密集提交 5 个 commit，重点完善 `--cmd` CLI 模式（snap / list / screen / restore），定位是**供 AI / 脚本接管窗口控制**。

## 3. 关键命令

```bash
cargo build                # 开发构建（带调试符号）
cargo run                  # 开发运行
cargo build --release      # 发布构建 → target/release/winsnap.exe（约 400KB，单文件无依赖）
cargo test                 # 单元测试（项目暂无测试用例）

# 运行参数：
winsnap.exe -c             # 启动显示控制台
winsnap.exe -l log.txt     # 日志落文件
winsnap.exe --cmd <snap|list|screen|restore>    # AI/脚本 CLI 模式
```

## 4. 最近 5 条 commit

- `e04f2d3` chore: 加 .gitignore 忽略 target/ 构建产物
- `2556b46` chore: 提交 Cargo.lock 锁定依赖版本
- `4f895dd` fix(cmd): proportional 改为按内容（visual）= 工作区 × scale、frame = 内容 + DWM 边框——与滚轮缩放（SCALE_STEP 0.1）完全一致
- `779750b` fix(cmd): snap 自动还原最小化窗口 + 新增 restore 子命令；`find_window` 按 pid 不滤尺寸（最小化窗口不丢）
- `d57dd9c` feat(cmd): 新增 `--cmd` CLI 模式（snap / list / screen），供 AI / 脚本调用窗口控制（复用 `config.rs` DPI / 可视矩形经验）

## 5. 关键说明 / 坑

- **CLI 模式专供 AI / 脚本**：snap / list / screen / restore 4 个子命令接入点。
- **proportional 计算口径**：visual = 工作区 × scale；frame = visual + DWM 边框；与滚轮缩放 `SCALE_STEP 0.1` 一致。
- **最小化窗口不丢**：`find_window` 按 pid 不滤尺寸。
- **单文件无依赖**：发布构建约 400KB，无 DLL 依赖。

## 6. 关联项目

- 无（独立工具）。

## 7. 状态摘要

| 指标 | 值 |
|---|---|
| 7d commits | 0 |
| 30d commits | 5 |
| dirty | 0 |
| ahead/behind | 0 / 0 |

---

🤖 *本文档由 workspace-project-audit 工作流生成（DSH workflow，2026-10-01）。如需重检，跑 `node my-skills/scripts/workspace-health.mjs active` 看实际状态。*