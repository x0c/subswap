# 2026-07-28 — Claude 能正常使用，但 subswap 仍显示旧 401 / parked 账号长期 429

## 现象

默认入口同时出现两类看似矛盾的状态：

```text
账号 A  (cached ~12h ago · 401 auth failed)
账号 B  (cached ~39h ago · 429 rate limited)
```

账号 A 已经能在 Claude Code 正常对话，`claude auth status` 也显示登录成功；账号 B 的旧余量长期不更新。

## 一句话结论

这是两个独立缺口叠加：

1. **鉴权失败退避过长**：账号 A 的 usage 查询先返回 401，数秒后 Claude Code 才刷新 live 凭据；
   subswap 已把旧 401 指数退避到 15 分钟，刷新成功后仍继续展示旧错误。
2. **空 access token 被回灌**：账号 B 激活期间，Claude Code 的 live 凭据短暂出现空 access；
   capture-on-leave 只保护了 refresh，没有保护 access，于是空 access 覆盖 store 完整副本。之后请求
   usage 时，上游可能先给 429，掩盖本地凭据已经不完整的事实。
3. **旧 quota 仍被当作自动候选**：损坏账号虽然实时查询失败，但 39 小时前的缓存尚未越过窗口 reset，
   自动切换仍把它当作可用候选，导致刚手动切回健康账号又被自动顶回损坏账号。

## 判别证据

- 失败缓存时间早于 Claude Code 凭据文件更新时间数秒，且刷新后的 access token 有新的未来过期时间：
  证明「能正常对话」与「旧 401 仍在退避」可以同时成立。
- pre-swap 快照连续对比显示：切入账号时 access 非空，切出账号时 access 变空；store 最终也为空，
  而 refresh 仍在。这不是单纯的 usage 限流。
- refresh 端点另有 `invalid_grant` 记录时，账号无法靠空 access 自愈，需要重新登录；但 **429 本身仍不等于
  token 失效**，必须结合本地凭据完整性与 refresh 响应判断。

## 修复

| 缺口 | 修复 |
|---|---|
| 登录已刷新但旧 401 仍展示 15 分钟 | 401/403 失败仍退避，但封顶为一个基础间隔（默认 90 秒）；其他失败保持指数退避、最长 15 分钟 |
| live access 为空时覆盖 store | 若 store 有非空 access，整份保留 store 凭据，不接受这次半成品回灌 |
| 空 access 继续请求 usage，可能显示误导性 429 | 请求前先判空，直接返回 `needs re-login`，不消耗 usage 限流桶 |
| 鉴权失败账号仍凭旧 quota 成为自动候选 | 401/403、`needs re-login`、凭据缺失一律从自动候选排除；网络、超时、429 等瞬态失败仍保留原兜底语义 |

## 长期约束

- access token 与 refresh token 都是凭据完整性边界；只保护 refresh 不够。
- 401/403 需要防请求风暴，但退避窗口必须容纳「原生客户端刚刚刷新完成」这一正常恢复路径；默认 90 秒
  已经比 daemon 轮询频率更保守，不应继续指数增长到 15 分钟。
- 空 access 是本地确定性错误，禁止拿它请求远端再按 HTTP 状态猜原因。
- 旧 quota 只证明过去可用；一旦当前错误已明确是鉴权/凭据问题，旧缓存不得覆盖这个确定性事实。
- 排查时先比较「失败发生时间」与「原生客户端凭据更新时间」，再决定是当前鉴权失败还是旧失败缓存。

## 关联

- [2026-06-14 429 vs invalid_grant](2026-06-14-claude-quota-unqueryable-429-vs-invalid-grant.md)
- [2026-06-18 live capture 覆盖 refresh token](2026-06-18-live-capture-clobbers-refresh-token.md)
- [2026-07-26 usage 响应字段漂移与失败退避](2026-07-26-claude-usage-schema-drift-bad-response.md)
