# 2026-08-14 — 多个 Cursor 账号显示完全相同的额度

## 现象

多个 Cursor 账号 First-Party / API 余量与重置时间全部一样。常见于刚导入或刚切换 CLI 账号后。

## 根因

CLI 登录拆两半：令牌（Linux 登录文件；macOS 钥匙串 `cursor-access-token` / `cursor-refresh-token`）与邮箱（`~/.cursor/cli-config.json` 的 `authInfo`，不含令牌）。旧切换只写令牌、留下上一号身份 → 按身份文件认主人，把当前令牌灌进「邮箱对、令牌是别人的」仓库副本 → 额度查询打到同一令牌。

更危险：停用号拿外来令牌刷新会刷废真正主人的一次性 refresh token。macOS 若「删掉再建」钥匙串条目，ACL 收成仅切换工具可读——必须只改内容、保留 Cursor 读取权限。

## 排查

1. 余量/重置时间字节级相同 → 先疑串号，勿先查额度接口。
2. 身份文件邮箱是否与令牌真正所属账号一致。
3. 各仓库副本令牌是否同一份（比对指纹，勿打 secret）。
4. 版本是否 ≥ 1.4.17（更旧不写身份文件；≤1.4.15 打开列表会把已删当前登录再加回）。

## 当前状态

**已修复（1.4.17）。** 切换成套写令牌+身份；live 主人只认令牌 JWT；令牌与账号对不上显示 `needs re-login`，不再查额度/刷新。已有钥匙串条目只更新、不删建。客户端仍登录则打开列表自动收入——含 `rm` 过的号（1.4.17 曾用删除墓碑拦截，**1.5.0 已移除**，见 [2026-08-15](2026-08-15-cursor-section-silently-missing.md)）。

写坏的停用号**不能从仓库救回**，删掉即可；当前令牌完整的号可留，或 `subswap login cursor` 再导入。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Cursor」
- [2026-08-14 CLI 已登录但无 Cursor 额度](2026-08-14-cursor-quota-missing-cli-keychain.md)
- [2026-06-18 live capture 覆盖 refresh](2026-06-18-live-capture-clobbers-refresh-token.md)
- [2026-06-11 keychain ACL 中毒](2026-06-11-claude-code-keychain-acl-poisoning.md)

<!-- 该文档整理/压缩于 2026-09-05 -->
