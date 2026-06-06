# 2026-06-06 · 凭证改用明文文件后端，根治 macOS 钥匙串弹窗 + Claude 额度跳过

## 现象

macOS 上 `subswap` 默认状态页：

```text
claude
     1 scottethanjfgg@gmail.com
  *  2 stromandanika707621@gmail.com  quota skipped on macOS

codex
     3 achesjeremy819@gmail.com
  *  4 stromandanika707621@gmail.com  5h [ 87% left ... ]  7d [ 85% left ... ]
```

- 非激活账号（1、3）整行空白。
- **激活的 Claude 账号（2）显示 `quota skipped on macOS`**，而激活的 Codex 账号正常。

## 根因

是 [2026-05-29-macos-keychain-prompts.md](2026-05-29-macos-keychain-prompts.md) 那套「尽量不碰
Keychain」缓解方案的副作用：

1. **非激活账号空白**：默认入口在 macOS 上对 `!active` 账号**主动跳过** quota 查询
   （`quota_query_would_touch_inactive_keychain`），避免逐个账号弹钥匙串框 → 渲染成空白。这是有意为之，不是报错。
2. **激活 Claude 账号被跳过**：查激活账号 quota 时先读本地实体文件 `~/.claude/.credentials.json`。
   但**新版 Claude Code 在 macOS 把凭证搬进了系统钥匙串**（`Claude Code-credentials`），不再写这个文件。
   于是 `read_active_credentials_if_matches` 读不到 → 落进「需钥匙串才能查」分支 → macOS 默认禁用 → 报
   `quota skipped on macOS`。Codex 因 `~/.codex/auth.json` 是真实文件，激活账号走读文件路径，不受影响。

根本矛盾：激活 Claude 账号的 token 在 macOS 上**只存在于钥匙串**，磁盘无明文副本；而读钥匙串就会弹框。

## 方案：默认换用 `FileStore` 明文文件后端

放开 AGENTS「敏感字段一律走 keyring，不许明文」的硬约束后，把凭证仓库默认后端从钥匙串换成明文文件：

- 新增 `crates/core/src/store.rs::FileStore`：单文件 `<data_dir>/credentials.json`，Unix `0600`，
  fs2 建议锁 + 临时文件 rename 原子写。
- `AppContext::build()`（CLI）与 daemon `run()` 默认装配 `FileStore::with_legacy_keyring(...)`。
- **懒迁移**：`FileStore::get` 未命中时回退旧 `KeyringStore` 读出并落盘；迁移后该项永不再碰钥匙串。
  首启会对尚未迁移的账号弹一次授权框（点「始终允许」即可），之后彻底无弹窗。
- 既然不再依赖钥匙串，移除所有为规避弹窗而生的 skip / 门控：
  - `cli`：删 `quota_query_would_touch_inactive_keychain`，激活/非激活一律查额度。
  - `claude`：删 `quota_keychain_access_enabled` / `keychain_write_back_enabled` / `is_active_account`；
    实体文件缺失直接回落 `FileStore`，刷新后无条件写回。
  - `codex`：删 `quota_keychain_access_enabled` / `active_keychain_repair_enabled`；激活账号用实时
    `auth.json` 无条件刷新仓库副本。
  - 废弃环境变量 `SUBSWAP_QUERY_INACTIVE_KEYCHAIN`、`SUBSWAP_SYNC_KEYCHAIN_ON_START`。

## 更深一层：macOS 上 subswap 从未捕获过 claude 凭证

换 `FileStore` 后实测：codex 两个账号都迁移成功，但 claude 两个都报 `missing credentials`。
落盘的 `credentials.json` 里只有 `codex:*:auth_json`，**claude 一条都没有**。根因：

- macOS 上 Claude Code 把凭证存进**自己的钥匙串 item** `Claude Code-credentials`（account = 登录用户名），
  既不写 `~/.claude/.credentials.json`，也不在 subswap 的 `service=subswap` 命名空间下。
- subswap 的 `import_active`（含 `subswap login claude` 登录后那步）只读 `~/.claude/.credentials.json`
  这个**在 macOS 上不存在的文件** → 一直失败 → **claude 凭证从未进过 subswap 的仓库**，只留了元数据。
- 所以 `FileStore` 迁移无从迁起（仓库里本就没有），`login claude` 在 macOS 上同样存不进。

**修法**：claude provider 增加读取 Claude Code 钥匙串 item 的能力
（`read_claude_code_keychain`，`service = "Claude Code-credentials"`，内容与 `.credentials.json` 同构）。
- `import_active` / `read_live_credentials`：实体文件缺失时回落该 item，`login claude` 因此在 macOS 可用。
- 查询路径 `load_credentials`：仓库未命中时，对**当前激活账号**（用 `~/.claude.json` 的
  `oauthAccount.emailAddress` 判断归属）做**一次性捕获**——读 Claude Code 钥匙串 → 落盘进 `FileStore`，
  之后查询走文件、不再读钥匙串。即激活 claude 账号**只在首次弹一次授权框**，之后无感。
- 非激活 claude 账号：Claude Code 钥匙串只存当前激活账号的凭证，故仍报 `missing credentials`，
  需 `swap` 过去（成为激活账号后即被捕获）或 `subswap login`。这是 macOS 的固有限制，非缺陷。
- subswap 捕获到的副本带 `refreshToken`，过期由自身 401→refresh→写回 `FileStore` 维持，不再依赖钥匙串。

## 影响

- macOS 上状态页**所有账号（激活/非激活）都能正常显示 quota**，不再有 `skipped on macOS`。
- 首启一次性迁移外，**默认入口不再弹钥匙串**。
- Linux keyutils 的 session 隔离问题（见 2026-05-29-daemon-keyutils-session-isolation.md）随之消失：
  文件对所有 session 可见且跨重启持久，daemon 保活不再空转。
- 代价：token 明文落盘（`0600`），安全模型与 Codex `~/.codex/auth.json` 对齐。

## 验证

- `cargo clippy --workspace` 无警告；`cargo test --workspace` 全绿（含 `FileStore` roundtrip / 命名空间 / `0600` 权限测试）。
- 覆盖安装本机 `subswap` / `subswapd` 后跑真实状态页，确认四个账号均出 quota、无 `skipped`、无反复弹框。
