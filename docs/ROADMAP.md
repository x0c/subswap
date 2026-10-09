# 里程碑

| 里程碑 | 目标 | 状态 |
|---|---|---|
| M1 | workspace 骨架 + core trait/模型 + doctor 可跑 | ✅ 已完成 |
| M2 | Claude Provider：keyring 切换 + 5h/7d quota + best-effort token 刷新 | ✅ 已完成 |
| M3 | Codex Provider：opaque blob 透传 + auth.json 原子写 + wham/usage | ✅ 已完成 |
| Phase A | rm / audit log / AutoSwapPolicy / 命令面大瘦身 / defaults 集中 | ✅ 已完成 |
| M4 | `subswapd` daemon：周期轮询 + 自动切换 + Claude token 后台保活 + CLI 自动拉起 | ✅ 已完成 |
| M5 | 账号隔离运行、quota 缓存、自动换号开关与手动切换宽限期 | ✅ 已完成 |
| M6 | Kimi / Cursor Provider、Codex 官方额度通道、三平台 CLI/Provider 支持 | ✅ 已完成 |
| M7 | OpenCode 官方账号与 API Key 分列、各自监控 Go 额度；仅官方账号自动换号，Key 仅手动选择；V2 官方数据库切换与 V1 文件兼容 | ✅ 已完成 |

## Open improvements

- Decide whether macOS can start background switching by default for providers that do not require an extra Keychain authorization. The [current opt-in](CLI.md) avoids possible Keychain dialogs. A changed default needs verification of authorization behavior and must tell users that already-running Codex sessions still need a restart after a global swap. Status: pending product decision; the current opt-in remains in place.

<!-- 该文档整理/压缩于 2026-09-05 -->
