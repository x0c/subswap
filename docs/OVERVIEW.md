# subswap 文档总览

> 面向项目内协作的中文文档。对外（GitHub README、CLI 输出）一律英文，见根目录 `README.md`。

## 业务文档

- [Provider 知识库 — PROVIDER_KNOWLEDGE_BASE.md](PROVIDER_KNOWLEDGE_BASE.md)：Claude / Codex 上游端点、本地文件结构、schema 不稳定的应对策略（含 Codex refresh 不做的设计理由）。

## 设计与方案

- [架构设计 — ARCHITECTURE.md](design/ARCHITECTURE.md)：模块划分、Provider 抽象、扩展机制。
- [自动切换设计 — AUTO_SWAP_DESIGN.md](design/AUTO_SWAP_DESIGN.md)：阈值/限流双触发、降级到手动切换。

## 操作指南

- [运行时配置 — CONFIG.md](CONFIG.md)：`~/.config/subswap/config.toml` 字段表、热加载机制、风控约束。

## 故障排查

- [2026-05-28 — TOML 序列化报 `unsupported unit type`](troubleshooting/2026-05-28-toml-null-serialization.md)
- [2026-05-28 — CLAUDE_CONFIG_DIR 自定义时 global config 写到上级目录](troubleshooting/2026-05-28-claude-config-dir-parent-pollution.md)
- [2026-05-29 — Linux daemon keepalive 空转：keyutils 按 session 隔离](troubleshooting/2026-05-29-daemon-keyutils-session-isolation.md)

## Code Review 台账

（暂无；按模块归档到 `reviews/<module>/`。）

---

## CLI 命令面（当前）

| 命令 | 说明 |
|---|---|
| `subswap` | 默认入口：扫本地自动 import → 立即显示账号骨架 → quota 渐进刷新 → AutoSwap 决策 → 最终状态;同时 best-effort 拉起 `subswapd`(用户无感) |
| `subswap login <claude\|codex>` | 调用官方 CLI 登录流程，完成后导入/覆盖当前登录账号并标记为 active |
| `subswap swap [<id\|N>]` | 手动切换；`<id>` 用 id/label/`<provider>/<id>`，`<N>` 用默认入口列出的全局序号。无参打印编号清单 |
| `subswap rm <id\|N>` | 删除账号（registry + keyring），引用形式同 `swap` |
| `subswap doctor` | 环境自检 |

被砍的子命令：`add` / `list` / `quota` / `refresh` / `auto` / `daemon`（统一收进无参默认行为）。

隐藏的一次性命令:`subswap migrate-local` — 从旧版本地账号目录把账号搬到 subswap。
`--help` 里看不到,只给迁移旧数据的人用一次。

辅助二进制 `subswapd`:由 CLI 在默认入口自动 detach 拉起,负责周期 quota 轮询 / 自动切换 /
Claude token 后台保活。Unix-only,通过 `<state>/subswapd.pid` 上的文件锁保证单实例。
关掉:`pkill subswapd`;不想被自动拉起:导出 `SUBSWAP_NO_DAEMON=1`。

## 里程碑（roadmap）

| 里程碑 | 目标 | 状态 |
|---|---|---|
| M1 | workspace 骨架 + core trait/模型 + doctor 可跑 | ✅ 已完成 |
| M2 | Claude Provider：keyring 切换 + 5h/7d quota + best-effort token 刷新 | ✅ 已完成 |
| M3 | Codex Provider：opaque blob 透传 + auth.json 原子写 + wham/usage | ✅ 已完成 |
| Phase A | rm / audit log / AutoSwapPolicy / 命令面大瘦身 / defaults 集中 | ✅ 已完成 |
| M4 | `subswapd` daemon：周期轮询 + 自动切换 + Claude token 后台保活 + CLI 自动拉起 | ✅ 已完成 |
