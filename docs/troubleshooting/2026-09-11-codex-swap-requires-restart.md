# 2026-09-11 — Codex 已切换（手动/自动）但对话仍用旧号 / 以为没自动切号

## 现象

- `subswap` / `subswap swap` 显示已切到新 Codex 号，但正在跑的 Codex CLI 或 IDE 扩展仍按旧号计额度、对话身份不变。
- 误以为「subswap 至今没做 Codex 自动切号」：列表里偶发有 `auto: swapped`，或磁盘 `auth.json` 已变，客户端却像没切。
- macOS 上很少看到后台自动切，只有偶尔跑无参 `subswap` 才动一下。

## 根因

1. **官方 Codex 不热读换号后的 `auth.json`。** `AuthManager` 启动缓存登录态；外部改写磁盘须显式 `reload()` 才观察。未授权自愈用的 `reload_if_account_id_matches` 在**账号 ID 不同时跳过**——换到另一号时不会灌进已运行进程。
2. **subswap 侧切号已完整**：手动与自动都经共享引擎原子写 live `auth.json`（默认入口 + `subswapd`）。「文件已换、进程未重读」不是缺自动切号。
3. **macOS 默认不自动拉起 `subswapd`** → 无后台轮询时，只有主动跑无参 `subswap` 才采样自动切。
4. **策略窗口**：小时级达阈值 / 耗尽才自动切；月度等长窗口接近阈值不触发。

## 排查

1. 对照 `subswap` 列表的 `*` 与 `~/.codex/auth.json`（或 `$CODEX_HOME/auth.json`）是否已是目标号——是则 subswap 已成功。
2. 重启 Codex CLI，或重载 IDE 窗口后再开对话，确认身份/额度是否跟上。
3. 需要**不改全局活号**立刻用某号：`subswap run codex <账号> -- …`（新进程 + 私有 `CODEX_HOME`）。
4. 要 macOS 后台自动切：设 `SUBSWAP_AUTO_DAEMON=1` 后再走默认入口拉起 daemon；确认 `pgrep` 有 `subswapd` / `__daemon`。
5. 月额度将满却不自动切：对照 [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) —— 长窗口非阈值触发。

## 不采用

- 把症状修成「补做 Codex 自动切号」（功能已在）。
- 捆绑或依赖社区 Codex 热补丁 / 改官方二进制做进程内换号。
- 为「实时生效」让 subswap 在后台抢刷 active refresh（与运行中 Codex 抢一次性 token）。

## 产品边界（可做 vs 不可做）

| 可做 | 不可做 |
|---|---|
| 切号成功后提示用户重启 Codex | 指望官方热读换账号 |
| 未来可选：协调退出并重启 Codex（对标 Cursor） | 把非官方热补丁当正式依赖 |
| 文档 / CLI 说清 macOS daemon 与窗口策略 | 用高频 usage 探测「切没切上」 |

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「切换生效边界」
- [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) §1.1 窗口策略；§5.5 Codex 无需 daemon 保活
- [CLI.md](../CLI.md) `subswapd` / `SUBSWAP_AUTO_DAEMON`
- [2026-07-09](2026-07-09-codex-quota-401-despite-working-cli.md)（内存态与磁盘态另一方向的错位）
