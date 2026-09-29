# 2026-09-29 — OpenCode 换号/切模型后 `reasoning encrypted_content was not issued to this caller`

## 现象

- OpenCode（含 Zen/Console 链路上的 Muse Spark）多轮对话中突然固定报 400：
  `reasoning encrypted_content was not issued to this caller`。
- 重试同一 session 必现，换 prompt 也没用；全新 session 只跑 2～3 轮也可能出现。
- subswap 语境下最易踩中的姿势：`subswap swap` 把 `auth.json` 的 `opencode-go` 切到另一个号（= 换了上游眼中的 caller/key），然后**继续用旧 OpenCode 会话**接着问。

## 根因

1. OpenAI Responses API 的 `reasoning.encrypted_content` 是模型隐藏 reasoning state 的加密 blob，客户端多轮需原样 replay 才能延续 reasoning；OpenCode 把它（含 `metadata.openai.itemId` / `reasoningEncryptedContent`）持久化到了 session/SQLite。
2. 该 blob 绑定签发时的 caller / 网关路由身份。caller 变化（换 Go key、OpenCode 重启、Muse 1.2↔1.3 切模型、tool call、图片、网络中断、Zen 路由切换、旧 session resume）后，上游拒绝 replay 旧 blob。
3. 失败后 OpenCode 不自动剔除坏 blob → 同一 session 每次都 replay 同一个坏块 → session 被污染 / bricked。`/compact` 同样可能带过去（Claude Code 侧同类 issue #49994 有相同观察）。
4. 这是 OpenCode ↔ Muse/Zen 的 Runtime 集成问题，不要定性为「Muse 1.3 模型能力差」或「账号没额度」。

## 处理

- 不保上下文：`/new` 开新会话（最干净）。
- 准备切 Muse 模型前：先 `/clear` 再切，避免旧 blob 被带过去。
- 保上下文（session 很重要）：删历史 reasoning part 的 `metadata.openai.itemId` + `metadata.openai.reasoningEncryptedContent`，保留 reasoning text/summary、普通 assistant message、tool call/result。社区有人直接清 OpenCode SQLite 这两个字段后旧 session 恢复；也有人用 `experimental.chat.messages.transform` 在发请求前剥掉这两个字段的 workaround 插件（`opencode-reasoning-sanitizer`，本仓库未验证）。
- subswap 侧：切 `opencode-go` 号后，**不要在旧 OpenCode 会话里继续问**；开新会话，或先 `/clear`。这与 Codex「切号须重启客户端」是同一类边界（见下关联），只是 Codex 是进程内存旧号，OpenCode 这里是 transcript 里旧 reasoning blob。

## 上游状态（截至 2026-09-29，未完全确认已修）

- 用户侧报告：OpenCode v1.18.32（2026-09-21）release notes 无此修复，相关 Muse Spark issue 仍 open；讨论指向检测 `encrypted_content was not issued to this caller` / `invalid_encrypted_content` / `Referenced reasoning item ... was not found or has expired` 后删 stale reasoning 并自动 retry（#48918 / #48908）。以上为 2026-09-29 会话记录时的外部信息，未在本仓库独立验证。
- 2026-09-29 另做一次 web 检索：`@oh-my-pi/pi-catalog` changelog 提到对 OpenCode Zen/Go Muse Spark SKU 停掉 encrypted reasoning 的请求与 replay（#11928），理由是网关把 Responses 链路代理到 Meta 但无法 round-trip 该加密块。是否为官方正式修复、覆盖哪些版本，仍需按需再查证。

## 不采用

- 把该 400 当成额度耗尽去切号（越切越糟：每个旧 session 照样坏）。
- 高频重试同一 session 指望自愈（坏 blob 不删，重试必败）。
- 为复现手动连发 quota/usage（打爆限流桶，见项目不变量）。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「OpenCode Go」
- [2026-09-11 Codex 切号后须重启](2026-09-11-codex-swap-requires-restart.md)（同类「切号后旧会话不跟新号」边界）
- [2026-09-05 号池≠切号](../PROVIDER_KNOWLEDGE_BASE.md)（请求途中换 key vs 改登录文件）

<!-- 该文档整理/压缩于 2026-09-29 -->
