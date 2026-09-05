# 2026-05-28 — CLAUDE_CONFIG_DIR 自定义时 global config 写到上级目录

## 现象

旧 `crates/providers/claude/src/paths.rs::global_config_path`：

```rust
if let Some(parent) = home.parent() {
    return parent.join(".claude.json");
}
```

`CLAUDE_CONFIG_DIR=/tmp/foo/claude-x` 时写入 `/tmp/foo/.claude.json`（污染上级）；`CLAUDE_CONFIG_DIR=/` 时尝试写 `/.claude.json`。

## 根因

隐式假设「`.claude/` 同级 = HOME」——仅 `~/.claude` 成立；设了 `CLAUDE_CONFIG_DIR` 后不成立。

## 修复

`global_config_path`：

1. 旧版 `<home>/.config.json` 存在 → 永远优先
2. 否则 `is_standard_claude_home(home)`：未设 `CLAUDE_CONFIG_DIR` 且 `home == $HOME/.claude` → `parent()/.claude.json`；否则就近 `<home>/.claude.json`

单测：`paths::tests::custom_dir_does_not_escape_to_parent`。冒烟：`CLAUDE_CONFIG_DIR=$SMOKE/custom/claude-x` 验证落在 `claude-x/` 内、不在 `custom/.claude.json`。

## 通用经验

跨目录路径函数：不能默认 home 是 `~/.claude`、不能默认上级可写；自定义目录优先就近。

<!-- 该文档整理/压缩于 2026-09-05 -->
