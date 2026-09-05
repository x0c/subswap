# 2026-07-28 — Claude 能正常使用，但 subswap 仍显示旧 401 / parked 账号长期 429

## 现象

```text
账号 A  (cached ~12h ago · 401 auth failed)   # Claude Code 已能对话
账号 B  (cached ~39h ago · 429 rate limited)  # 旧余量不更新
```

## 根因（两缺口叠加）

1. **鉴权失败退避过长**：usage 先 401，数秒后 Claude Code 才刷新 live；旧 401 指数退避到 **15 分钟**，刷新成功后仍展示旧错误。
2. **空 access 被回灌**：激活期间 live 短暂空 access；capture-on-leave 只保护 refresh → 空 access 覆盖 store。之后 usage 可能先 429，掩盖本地凭据不完整。
3. **旧 quota 仍作自动候选**：损坏账号实时失败，但 39h 前缓存未越过窗口 reset → 自动切回损坏号。

## 判别

- 失败缓存时间早于凭据文件更新数秒，且新 access 有未来过期时间 → 「能对话」与「旧 401 仍在退避」可并存。
- pre-swap 快照：切入 access 非空、切出变空，store 最终空而 refresh 仍在 → 非单纯限流。
- refresh 有 `invalid_grant` → 需重登；**429 ≠ token 失效**，须结合本地凭据完整性与 refresh 响应。

## 修复

| 缺口 | 修复 |
|---|---|
| 旧 401 仍展示 15 分钟 | 401/403 仍退避，封顶一个基础间隔（默认 **90 秒**）；其他失败指数退避、最长 15 分钟 |
| live access 空覆盖 store | store 有非空 access → 整份保留，不接受半成品回灌 |
| 空 access 请求 usage 显示误导 429 | 请求前判空 → `needs re-login`，不消耗限流桶 |
| 鉴权失败仍凭旧 quota 成候选 | 401/403、`needs re-login`、凭据缺失一律排除；网络/超时/429 等瞬态保留原兜底 |

## 长期约束

- access 与 refresh 都是完整性边界；只保护 refresh 不够。
- 401/403 防风暴，但退避须容纳「原生客户端刚刷新」；默认 90s 已比 daemon 轮询更保守，不应再指数增到 15 分钟。
- 空 access 是本地确定性错误，禁止拿它请求远端再按 HTTP 猜原因。
- 当前错误已明确鉴权/凭据问题 → 旧缓存不得覆盖。排查先比「失败时间」与「原生凭据更新时间」。

## 关联

- [2026-06-14 429 vs invalid_grant](2026-06-14-claude-quota-unqueryable-429-vs-invalid-grant.md)
- [2026-06-18 live capture 覆盖 refresh](2026-06-18-live-capture-clobbers-refresh-token.md)
- [2026-07-26 usage 字段漂移](2026-07-26-claude-usage-schema-drift-bad-response.md)

<!-- 该文档整理/压缩于 2026-09-05 -->
