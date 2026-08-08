# AGENTS.md

供 ZCode agent 在 `bili-hardcore` 仓库工作时参考的工作区指令。

## 项目概述

`bili-hardcore` 是一个 Rust 终端 UI（TUI）工具，通过 OpenAI 兼容的 LLM 自动完成 B 站"硬核会员"试炼答题。流程为：二维码登录 → 获取题目 → 流式发给 LLM → 解析返回的 1–4 选项序号 → 提交答案；遇到解析/LLM 错误时自动重试。

- **语言/运行时：** Rust，edition 2024，构建需 Rust 1.88+（CI 固定 1.95）。
- **异步栈：** tokio（多线程），配合 `tokio::sync::mpsc` 无界通道与 `tokio_util::sync::CancellationToken` 取消答题后台任务。
- **TUI 栈：** ratatui 0.29 + crossterm 0.28（event-stream 特性）。`ratatui-image` 用于内联渲染验证码图片。
- **HTTP：** reqwest + rustls-tls（不依赖 native-tls / openssl）。LLM 流式响应用 `eventsource-stream` 解析。

## 构建 / 运行 / 检查

```bash
cargo build --release        # 优化构建（profile.release：opt-level="z"、lto、panic=abort、strip）
cargo run                    # debug 构建；日志写入 ./logs/bili-hardcore.log（仅 debug 构建启用）
cargo run -- <url> <model> -k <api-key>   # 通过命令行参数直接启动（三者必须同时给出）
cargo run -- update          # 从最新 GitHub release 自更新
cargo run -- uninstall       # 卸载
cargo fmt                    # 格式化（沿用现有风格）
cargo clippy --all-targets   # lint，无项目级配置，使用默认规则
```

本仓库没有任何测试文件或测试框架，`cargo test` 只会执行编译。

## 源码结构

```
src/
  main.rs        CLI（clap）、终端初始化、主运行循环、update/uninstall 子命令
  app.rs         核心状态机：App 结构体、QuizPhase 枚举、AppEvent 分发、
                 所有 tokio::spawn 后台任务、事件处理
  input.rs       键盘事件处理（按当前 Page 分发）
  config.rs      ~/.bili-hardcore/ 下的持久化配置（OpenAI 配置、登录态、分类、历史），
                 LLM prompt 构造器（build_quiz_prompt）
  crypto.rs      B 站 appsign 签名（MD5 + APPSEC，ticket 用 HMAC-SHA256）
  error.rs       AppError（thiserror）—— api/ 与 IO 的统一错误类型
  api/client.rs  BiliClient —— 所有 B 站接口（ticket、二维码、题目、验证码、提交）
  llm/openai.rs  OpenAiClient —— OpenAI 兼容流式对话，通过 channel 发出 LlmChunk
  ui/            ratatui 视图：home.rs、config_page.rs、quiz.rs（由 ui::draw 选择）
```

## 改动时需遵守的架构规则

- **UI 单线程，任务多异步。** 所有网络/LLM 工作都在 `tokio::spawn` 中运行，通过 `mpsc` 通道以 `AppEvent` 变体回传。主循环每个 tick 清空 `app.rx`（见 `main.rs` 的 `run_app`）。严禁在 `App::tick`/`App::process`/UI 绘制中做阻塞 IO——必须 spawn 后发送事件。
- **`App::process` 仅在 `page == Page::Quiz` 时处理答题事件。** 这是有意为之：防止用户 ESC 退出答题后后台继续答题。不要移除该守卫。离开答题页还会取消 `quiz_token`（见 `App::back`），后台任务在发送事件前必须检查 `token.is_cancelled()`。
- **`BiliClient` 通过 `clone_for_async`/`async_clone` 克隆后传入任务**（克隆共享的 `reqwest::Client` 与 header map）。禁止跨任务共享 `&mut BiliClient`。
- **LLM 重试策略：** `App::MAX_LLM_RETRIES = 3`，指数退避（`2 << (attempt-1)` 秒）。LLM 返回空或无法解析时触发 `LlmRetry`；`parse_answer` 接受纯数字 1–4、"回答：N" 前缀、或文本中任意 1–4 数字。
- **分层：** `api/` 与 `llm/` 不得依赖 `app.rs` 或 `ui/`。`app.rs` 是唯一的编排者。`ui/` 只读取 `App` 状态，不得自行 spawn 任务或直接修改配置。

## 编码约定

- **注释与所有面向用户的字符串均为简体中文。** 修改既有代码时请保持一致（如错误信息 `"获取 ticket 失败"`、tracing 文案）。
- **日志：** 使用 `tracing::{info,warn,error}`。`setup_logging` 仅在 `#[cfg(debug_assertions)]` 下初始化订阅器——release 构建无日志。INFO/WARN 记录 LLM prompt 与原始 API 响应以便排查。
- **配置持久化** 位于 `~/.bili-hardcore/`：`openai_config.json`、`auth.json`（`load_auth` 中 7 天后自动过期）、`categories.json`、`history.json`。请使用 `config.rs` 中的辅助函数，不要直接读写这些路径。
- **CLI 配置陷阱：** `main.rs` 中只有 `url`、`model`、`api_key` 三者同时给出才构造 CLI 配置，否则回退到已保存的文件；且 CLI 配置会强制 `enable_thinking=false`，覆盖已保存配置。
- **OpenAI 兼容接口分叉：** `llm/openai.rs` 按 `base_url` 是否包含 `api.openai.com` 分支——OpenAI 走 `reasoning_effort`，其余额外加 `enable_thinking` + `thinking` 对象（DeepSeek/Qwen 风格）。修改请求体时需同时维护两个分支。
- **`presets.json`** 在编译期由 `include_str!` 嵌入；`input.rs` 中的 `PRESET_COUNT` 必须与其长度匹配。

## 发布流程

- 发布由 **git tag `v*`** 驱动，触发 `.github/workflows/release.yml`。矩阵构建 macOS（universal，通过 lipo 合并）、Windows x64、Linux x64/arm64（musl + glibc）。**Linux 用户优先使用 `-musl` 版本**（静态链接）。
- Release notes 从 `docs/release-notes/vX.Y.Z.md` 读取（文件末尾必须有换行——此前已修复过 heredoc EOF 解析问题）。打 tag 前需新增对应文件。
- 使用 `/release` skill（`.agents/skills/version-release/`）完成版本流程：按 Conventional Commits 前缀推断 bump（`feat!`→major、`feat:`→minor、`fix:`/`refactor:`/`chore:`→patch），更新 `Cargo.toml` 的 `version`，写入 release note 并打 tag——但**必须等用户明确确认版本号后才能执行任何 git 操作。**
- 提交信息遵循 Conventional Commits（`feat:`、`fix:`、`chore:`、`release:`、`docs:`），正文用中文。

## 注意事项

- Windows 无法替换运行中的 exe——`run_update`/`uninstall` 会先重命名为 `.old.exe` / 退出后用 `.bat` 删除。请保留此平台分支。
- 二维码登录流程每 ~1 秒轮询一次（10 ticks × 100ms tick 率），倒计时 60 秒；超时转入 `LoginTimeout { retry: true }`。
- `LevelOk` 会从 `history`（统计 `correct==true`）恢复累计得分，而不是重置为 0——需与 `SubmitOk` 推导 `correct` 的方式（`score > self.score`）保持一致。
- 答满 100 题后切换到 `Submitting` 并调用 `fetch_final` 取回各分区得分。
