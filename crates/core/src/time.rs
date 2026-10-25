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

/// 解析 HTTP `Retry-After` 的 delta-seconds 或 HTTP-date（RFC 2822/IMF-fixdate）。
///
/// 返回绝对 UTC 截止时间；缺失、非正数、过去时间或溢出返回 `None`，交由调用方的
/// 通用退避下限兜底。该函数是纯计算，不依赖 HTTP 客户端，供各 Provider 共用。
pub fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<i64>() {
        return (seconds > 0)
            .then(|| {
                chrono::Duration::try_seconds(seconds)
                    .and_then(|duration| now.checked_add_signed(duration))
            })
            .flatten();
    }
    DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| date.with_timezone(&Utc))
        .filter(|date| *date > now)
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

    #[test]
    fn retry_after_accepts_delta_seconds_and_http_date() {
        let now = Utc::now();
        assert_eq!(
            parse_retry_after("7200", now),
            Some(now + chrono::Duration::hours(2))
        );
        let future = now + chrono::Duration::hours(3);
        assert_eq!(
            parse_retry_after(&future.to_rfc2822(), now).map(|date| date.timestamp()),
            Some(future.timestamp())
        );
        let http_date = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        assert_eq!(
            parse_retry_after(&http_date, now).map(|date| date.timestamp()),
            Some(future.timestamp()),
            "{http_date}"
        );
        assert_eq!(
            parse_retry_after(" Wed, 21 Oct 2015 07:28:00 GMT ", Utc::now()),
            None,
            "past HTTP-date must not create a deadline"
        );
    }

    #[test]
    fn retry_after_rejects_zero_negative_garbage_and_overflow() {
        let now = Utc::now();
        for value in ["", "0", "-1", "garbage", "9223372036854775807"] {
            assert_eq!(parse_retry_after(value, now), None, "{value:?}");
        }
    }
}
