# 2026-05-28 — TOML 序列化报 `unsupported unit type`

## 现象

`subswap add codex --auth-file` 对缺字段 fixture（如无 `account_name` / `auth_mode`）报：

```
TOML 序列化错误: unsupported unit type
```

## 根因

`crates/providers/codex/src/codex_files.rs::AuthMetadata` 的 `Option<String>` 只有 `#[serde(default)]`，无 `skip_serializing_if`：

1. `serde_json::to_value` 把 `None` → JSON `null`
2. 塞进 `Account.extra`
3. `AccountRegistry::save()` → `toml::to_string_pretty`
4. **TOML 不支持 null** → `unsupported unit type`

## 修复

凡最终写入 `registry.toml` 的 `Option<T>`：

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
```

`AuthMetadata` 7 个 Option 字段全部补上。`claude_files.rs::OauthAccount` 一开始就有，故未踩。

## 预防

`serde_json` ↔ `toml` 桥接隐式约束，编译器不提示。新 Provider 元数据进 `Account.extra` 时人工检查 Option 字段。

<!-- 该文档整理/压缩于 2026-09-05 -->
