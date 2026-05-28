# 2026-05-28 — TOML 序列化报 `unsupported unit type`

## 现象

M3 冒烟测试时，添加缺失部分字段的 Codex 账号（如 `auth-d.json` 没有 `account_name` 和 `auth_mode`）报：

```
Caused by:
    0: TOML 序列化错误: unsupported unit type
    1: unsupported unit type
```

旧版 `subswap add codex --auth-file <file>` 在某些 fixture 上成功、某些上失败，初看像是随机故障。

## 根因

`crates/providers/codex/src/codex_files.rs::AuthMetadata` 的所有 `Option<String>`
字段只加了 `#[serde(default)]`，没加 `skip_serializing_if`。流程：

1. `serde_json::to_value(&metadata)` 把 `None` 序列化为 JSON `null`
2. 这个 `Value` 塞进 `Account.extra`（`serde_json::Map`）
3. `AccountRegistry::save()` 调 `toml::to_string_pretty(&accounts)`
4. **TOML 规范不支持 null / unit 类型** → 报 `unsupported unit type`

只有字段全 Some 的 fixture 才能撞运气过。

## 修复

所有「最终可能写入 registry.toml」的 `Option<T>` 字段必须加：

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
```

具体修复：`AuthMetadata` 7 个 Option 字段全部补上。

## 类似风险点

- `claude_files.rs::OauthAccount` 一开始就加了，所以 M2 没踩到
- 未来新加任何会进 `Account.extra` 的结构体时，**Option 字段必须**有 `skip_serializing_if`

## 预防

这是 `serde_json` ↔ `toml` 桥接的隐式约束，编译器不会提示。
新增 Provider 元数据 struct 时，对照本规则人工检查。
