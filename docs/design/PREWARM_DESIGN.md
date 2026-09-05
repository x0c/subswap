# 窗口预热（prewarm）设计提案

> 状态：**提案 / 未实现**（2026-05-29 调研定稿）。实现前先完成「待验证」清单。
> 关联：AGENTS.md #9、ARCHITECTURE.md §5.6——本功能是 #9 的**显式豁免**，实现时必须同步改这两处。

## 1. 动机

Claude 5h 窗口「首条消息锚定」。多账号错位：A 近耗尽才切 B → B 窗口从切换时刻起跑，重置延后。提前给闲号发一条锚定 → 窗口错峰，连续可干活更长。

## 2. 机制调研结论（先读这条）

| Provider | 5h 窗口锚定方式 | 预热是否有效 | 依据 |
|---|---|---|---|
| **Claude** | **首条消息锚定**（2026-04 起精确到分钟，如 6:00 发 → 跑到 11:00） | ✅ 有效 | 官方 headless 文档 + `vdsmon/claude-warmup` 项目实证 |
| **Codex** | **存疑，大概率固定时钟重置**（多处资料称常在固定时刻如 UTC 午夜重置；多个 issue 抱怨重置时间 variable、不按 `/status` 报告值） | ❓ **未确认，可能是空操作** | 见参考链接；官方未明说，社区无人做 Codex 预热 |

**结论**：Claude 先做；**Codex 必须先实测**（闲置号发 `codex exec hi`，看 `resets_at` 是否提前到 ~5h 后）再决定是否纳入。

## 3. 无头命令（官方 CLI，非裸调 API）

- **Claude**：`claude -p "hi" --model haiku --no-session-persistence`
  - `-p`：非交互发一条后退出；`--model haiku` 最便宜；`--no-session-persistence` 不落会话。
- **Codex**：`codex exec --ephemeral "hi"`
  - 非交互单轮；`--ephemeral` 不落盘；默认复用已登录凭证。

> 成本可忽略。预热消息**也计入 7d 周限**——只优化 5h 时机，不增加周额度。

## 4. 多账号关键约束

网上预热工具多为单账号直塞 token；`claude -p` / `codex exec` 用**当前 active 凭证文件**。多账号真实形态：

```
保存当前 active → for 每个目标账号 { activate(账号) → 跑无头 hi } → 恢复原 active
```

- 连续改写凭证文件（N 次 swap）→ **必须在用户没在干活时跑**（如早上 cron），不能抢 session；
- 跑完**必须恢复原 active**；
- 复用 `activate` 快照/回滚（不变量 #2），任一步失败回到原状态。

## 5. 设计方案

显式命令 **`subswap prewarm`**（不进默认入口、不进 daemon 自动触发）：

1. 记录当前各 provider 的 active；
2. 遍历已注册账号，对支持预热的 provider（先仅 Claude）逐个 `activate` + 无头 hi；
   - 可选：只预热「无活动窗口」的号（usage 无 `resets_at`）；
   - 单账号失败只 warn、继续（best-effort）；
3. 恢复步骤 1 的 active；
4. 用户自行 cron；subswap 不内建调度器。

**不做**：daemon 后台自动预热（churn 凭证 + 贴 #9 红线；留作后续单独评估）。

## 6. 配置参数（实现时走 settings.rs，遵不变量 #8）

| 字段（拟） | 默认 | 说明 |
|---|---|---|
| `prewarm.enabled` | `false` | 默认关，显式开启 |
| `prewarm.message` | `"hi"` | 预热消息内容 |
| `prewarm.only_idle_windows` | `true` | 仅预热无活动窗口的账号 |
| `prewarm.cooldown_ms` | 待定（≥ 单窗口长度量级） | 同一账号两次预热最小间隔，防重复发 |

> 上述阈值/开关需进 `defaults.rs` → `settings.rs` → CONFIG.md。

## 7. 与不变量 #9 的关系（实现时必办）

#9 禁「**高频**请求模拟限流 / 请求风暴」。预热极低频（每号每窗口≤1）、走官方 CLI、仅本人已注册号——不违反字面，但碰「不主动制造与任务无关流量」精神。owner 已决定做。**实现 PR 必须同步**：

- `AGENTS.md #9` 增补豁免（仅官方 CLI、仅本人号、每窗口≤1、失败退避、默认关、仅显式命令）；
- `ARCHITECTURE.md §5.6` 同步豁免说明。

否则后续 agent 会当违规修掉。

## 8. 待验证 / 待办（实现前）

- [ ] **实测 Codex**：闲置号 `codex exec hi` 后 `resets_at` 是否提前 → 决定是否纳入；
- [ ] 确认 `claude -p` 在「仅写凭证、无项目上下文」目录下能发（无需 trust/权限交互）；
- [ ] 确认无头预热不触发首次 onboarding / 目录信任卡住。

## 9. 风险

单条 hi、官方 CLI、本人付费号 → 封号风险基本为零。代价：非任务流量（#9 豁免）+ 极少 7d 周额度。

## 10. 参考

- [Claude Code headless 文档](https://code.claude.com/docs/en/headless)
- [vdsmon/claude-warmup](https://github.com/vdsmon/claude-warmup)（`claude -p "hi" --model haiku --no-session-persistence` + cron）
- [Codex 非交互模式](https://developers.openai.com/codex/noninteractive)
- [Codex usage limits 说明](https://knightli.com/en/2026/04/15/codex-usage-limits-five-hour-weekly-credits/)

<!-- 该文档整理/压缩于 2026-09-05 -->
