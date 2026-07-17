//! 半结构化凭证 JSON 的宽松字段抽取。Codex/Kimi 的 blob 结构各异，只按 key 递归找。

/// 在任意嵌套 JSON 里递归查找第一个名为 `key` 的非空字符串值。
pub fn extract_token(raw: &str, key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    fn walk(v: &serde_json::Value, key: &str) -> Option<String> {
        match v {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(s)) = map.get(key) {
                    if !s.is_empty() {
                        return Some(s.clone());
                    }
                }
                map.values().find_map(|c| walk(c, key))
            }
            serde_json::Value::Array(items) => items.iter().find_map(|c| walk(c, key)),
            _ => None,
        }
    }
    walk(&value, key)
}

/// 抽 access_token（兼容扁平与 `tokens.access_token` 嵌套）。
pub fn extract_access_token(raw: &str) -> Option<String> {
    extract_token(raw, "access_token")
}

/// 抽非空 refresh_token。
pub fn extract_refresh_token(raw: &str) -> Option<String> {
    extract_token(raw, "refresh_token")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nested_and_flat() {
        assert_eq!(
            extract_access_token(r#"{"tokens":{"access_token":"t1"}}"#).as_deref(),
            Some("t1")
        );
        assert_eq!(
            extract_access_token(r#"{"access_token":"t2"}"#).as_deref(),
            Some("t2")
        );
    }

    #[test]
    fn empty_refresh_is_none() {
        assert!(extract_refresh_token(r#"{"refresh_token":""}"#).is_none());
        assert!(extract_refresh_token(r#"{"access_token":"x"}"#).is_none());
    }
}
