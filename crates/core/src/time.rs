//! 时间相关的通用工具：epoch 秒/毫秒识别、转 [`chrono::DateTime`]。
//!
//! 各 Provider 上游 API 给 epoch 时有时是秒、有时是毫秒（甚至同一接口都会漂移），
//! 集中在这里做识别，避免散落多份判断。

use chrono::{DateTime, Utc};

/// 1e12 是「秒级时间戳」与「毫秒级时间戳」的天然分界：
/// - 秒级 32-bit epoch 不超过 ~2^31 ≈ 2.1e9
/// - 毫秒级 32-bit epoch 超过 1e12（≈ 2001 年）
const EPOCH_SECS_VS_MS_THRESHOLD: i64 = 1_000_000_000_000;

/// 把 epoch（秒或毫秒）归一为毫秒。绝对值 > 1e12 视为毫秒，否则视为秒。
pub fn epoch_to_millis(epoch: i64) -> i64 {
    if epoch.abs() > EPOCH_SECS_VS_MS_THRESHOLD {
        epoch
    } else {
        epoch.saturating_mul(1000)
    }
}

/// 把 epoch（秒或毫秒）转 UTC 时间。解析失败回退到 `Utc::now()`。
pub fn epoch_to_datetime(epoch: i64) -> DateTime<Utc> {
    let secs = if epoch.abs() > EPOCH_SECS_VS_MS_THRESHOLD {
        epoch / 1000
    } else {
        epoch
    };
    DateTime::<Utc>::from_timestamp(secs, 0).unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_and_millis_resolve_to_same_instant() {
        let s = epoch_to_datetime(1_700_000_000).timestamp();
        let m = epoch_to_datetime(1_700_000_000_000).timestamp();
        assert_eq!(s, m);
    }

    #[test]
    fn millis_normalization() {
        assert_eq!(epoch_to_millis(1_700_000_000), 1_700_000_000_000);
        assert_eq!(epoch_to_millis(1_700_000_000_000), 1_700_000_000_000);
    }
}
