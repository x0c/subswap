# 窗口预热（prewarm）设计提案

> 状态：**提案 / 未实现**（2026-05-29 调研定稿）。实现前需先完成「待验证」清单。
> 关联不变量：AGENTS.md #9、ARCHITECTURE.md §5.6（风控边界）——本功能是 #9 的**显式豁免**，实现时必须同步改这两处。

## 1. 动机

Claude 的 5h 用量窗口是「首条消息锚定」：第一条消息时刻窗口起跑。多账号时存在窗口错位问题：
在 A 号干到接近耗尽才切到闲置的 B，B 的窗口从切换时刻起跑，下一次重置延后。若提前给 B 发一条消息锚定窗口，则各号窗口错峰对齐，连续可干活时长更长。

## 2. 机制调研结论（先读这条）

| Provider | 5h 窗口锚定方式 | 预热是否有效 | 依据 |
|---|---|---|---|
| **Claude** | **首条消息锚定**（2026-04 起精确到分钟，如 6:00 发 → 跑到 11:00） | ✅ 有效 | 官方 headless 文档 + `vdsmon/claude-warmup` 项目实证 |
| **Codex** | **存疑，大概率固定时钟重置**（多处资料称常在固定时刻如 UTC 午夜重置；多个 issue 抱怨重置时间 variable、不按 `/status` 报告值） | ❓ **未确认，可能是空操作** | 见参考链接；官方未明说，社区无人做 Codex 预热 |

**结论**：Claude 先做；**Codex 必须先实测**（给一个闲置 codex 号发 `codex exec hi`，看 `resets_at` 是否提前到 ~5h 后）再决定加不加，避免做个无效功能。

## 3. 无头命令（官方 CLI，非裸调 API）

预热走**官方 CLI 无头模式**，不直接调 `/v1/messages` 等生成端点。

- **Claude**：`claude -p "hi" --model haiku --no-session-persistence`
  - `-p`/`--print`：非交互，发一条、输出到 stdout、退出；
  - `--model haiku`：用最便宜模型；`--no-session-persistence`：不落会话。
- **Codex**：`codex exec --ephemeral "hi"`
  - `codex exec`：非交互单轮；`--ephemeral`：不落盘 session。
  - 默认复用已登录凭证。

> 成本：一条 haiku/最小请求可忽略。注意预热消息**也会计入 7d 周限**，只优化 5h 窗口时机、不增加周额度。

## 4. 多账号关键约束（网上单账号脚本不涉及）

网上的预热工具都是**单账号**：把那一个号的 OAuth token 塞进 secret 直接发。
但 `claude -p` / `codex exec` 用的是**当前 active 的凭证文件**。subswap 是多账号，要预热闲号 B，
必须先把 active 切到 B 才能用官方无头命令发。因此多账号预热的真实形态：

```
保存当前 active → for 每个目标账号 { activate(账号) → 跑无头 hi } → 恢复原 active
```

推论：
- 预热过程会**连续改写客户端凭证文件（N 次 swap）**，**必须在用户没在干活时跑**
  （契合成熟做法的「早上 cron 预热」），不能在用户正用 Claude Code 时插入抢 session；
- 跑完**必须恢复到原本的 active 账号**；
- 复用现有 `activate` 的快照/回滚（不变量 #2），任一步失败要能回到原状态。

## 5. 设计方案

新增显式命令 **`subswap prewarm`**（不进默认入口、不进 daemon 自动触发）：

1. 记录当前各 provider 的 active 账号；
2. 遍历已注册账号，对支持预热的 provider（先仅 Claude）逐个 `activate` + 跑无头 hi；
   - 可选：只预热「无活动窗口」的号（usage 无 `resets_at`），已在窗口内的跳过，减少无谓请求；
   - 单账号失败只 warn、继续下一个（best-effort）；
3. 结束恢复步骤 1 记录的 active 账号；
4. 用户自行用 cron 在开工前定时跑（推荐做法，subswap 不内建调度器）。

**不做**：daemon 后台自动预热（churn 凭证 + 更贴 #9 精神红线，留作后续单独评估）。

## 6. 配置参数（实现时走 settings.rs，遵不变量 #8）

| 字段（拟） | 默认 | 说明 |
|---|---|---|
| `prewarm.enabled` | `false` | 默认关，显式开启 |
| `prewarm.message` | `"hi"` | 预热消息内容 |
| `prewarm.only_idle_windows` | `true` | 仅预热无活动窗口的账号 |
| `prewarm.cooldown_ms` | 待定（≥ 单窗口长度量级） | 同一账号两次预热最小间隔，防重复发 |

> 命令行无头调用本身不是数值调优参数，但上述阈值/开关需进 `defaults.rs` → `settings.rs` → CONFIG.md。

## 7. 与不变量 #9 的关系（实现时必办）

#9 禁的是「**高频**请求模拟限流 / 请求风暴」。预热是**极低频**（每号每窗口最多一次）、走官方 CLI、
仅对本人已注册账号 —— **不违反 #9 字面**，但碰了它「subswap 不主动制造与任务无关流量」的精神。
项目 owner 已决定做此功能。**实现 PR 必须同步**：

- 在 `AGENTS.md #9` 增补预热豁免边界（仅官方 CLI、仅本人号、每窗口≤1 次、失败退避、默认关、仅显式命令）；
- 在 `ARCHITECTURE.md §5.6` 同步该豁免说明。

否则后续（含 AI agent）会把它当违规「修掉」。

## 8. 待验证 / 待办（实现前）

- [ ] **实测 Codex**：闲置 codex 号发 `codex exec hi` 后 `resets_at` 是否提前 → 决定 Codex 是否纳入；
- [ ] 确认 `claude -p` 在「仅写了凭证、无项目上下文」的目录下能正常发（无需 trust/权限交互）；
- [ ] 确认无头预热不会触发 Claude Code 的首次 onboarding / 目录信任提示而卡住。

## 9. 风险

单条 hi 走官方 CLI、本人付费号 → 封号风险基本为零（风控针对高频风暴/绕限流/共享倒卖）。代价：subswap 主动发起的非任务流量（已作为 #9 豁免记录在案）+ 占用极少 7d 周额度。

## 10. 参考

- [Claude Code headless 文档](https://code.claude.com/docs/en/headless)
- [vdsmon/claude-warmup](https://github.com/vdsmon/claude-warmup)（`claude -p "hi" --model haiku --no-session-persistence` + cron）
- [Codex 非交互模式](https://developers.openai.com/codex/noninteractive)
- [Codex usage limits 说明](https://knightli.com/en/2026/04/15/codex-usage-limits-five-hour-weekly-credits/)
