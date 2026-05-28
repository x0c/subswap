# 2026-05-28 — CLAUDE_CONFIG_DIR 自定义时 global config 写到上级目录

## 现象

第三方 code review 指出 `crates/providers/claude/src/paths.rs::global_config_path` 旧实现：

```rust
if let Some(parent) = home.parent() {
    return parent.join(".claude.json");
}
```

不分场景地走 `parent()`。当 `CLAUDE_CONFIG_DIR=/tmp/foo/claude-x` 时：

| 用户期望 | 旧实现行为 |
|---|---|
| 写入 `/tmp/foo/claude-x/.claude.json`（自定义目录内） | 写入 `/tmp/foo/.claude.json`（**污染无关上级**） |

极端：`CLAUDE_CONFIG_DIR=/` 时尝试写 `/.claude.json`。

## 根因

旧实现沿用了一个隐式假设：
**`.claude/` 的同级目录 = HOME**。

这个假设只在 `~/.claude` 时成立；用户设了 `CLAUDE_CONFIG_DIR` 后不再成立。

## 修复

`crates/providers/claude/src/paths.rs::global_config_path`：

1. 旧版 `<home>/.config.json` 存在 → 永远优先（兼容旧布局）
2. 否则判断 `is_standard_claude_home(home)`：
   - `CLAUDE_CONFIG_DIR` 未设置 **且** `home == $HOME/.claude` → 走 `parent()/.claude.json`
   - 否则就近放 `<home>/.claude.json`

新增单测 `paths::tests::custom_dir_does_not_escape_to_parent` 覆盖。
冒烟也通过：smoke 脚本下设 `CLAUDE_CONFIG_DIR=$SMOKE/custom/claude-x` 跑 swap，
验证文件落在 `claude-x/` 内而**不在** `custom/.claude.json`。

## 通用经验

所有「跨目录」路径函数：
- 不能默认假设 home 是 `~/.claude`
- 不能默认假设上级目录可写或属于用户
- 自定义目录路径优先「就近」，远离用户其它私有空间
