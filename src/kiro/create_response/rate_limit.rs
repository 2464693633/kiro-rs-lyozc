//! Kiro's native delay headers are milliseconds; downstream Retry-After is seconds.
use crate::kiro::error::UpstreamRateLimitError;

pub(crate) fn rate_limit_error(headers: &http::HeaderMap, body: &str) -> UpstreamRateLimitError {
    let standard = UpstreamRateLimitError::from_headers(headers);
    if standard.retry_after().is_some() {
        return standard;
    }
    let delay = ["x-amzn-kiro-ratelimit-retry-after", "x-amz-retry-after"]
        .into_iter()
        .find_map(|key| headers.get(key)?.to_str().ok()?.parse::<u64>().ok())
        .or_else(|| {
            serde_json::from_str::<serde_json::Value>(body)
                .ok()?
                .get("retryAfterMilliseconds")?
                .as_u64()
        });
    match delay {
        Some(ms) => {
            UpstreamRateLimitError::new(Some((ms / 1000 + u64::from(ms % 1000 != 0)).to_string()))
        }
        None => standard,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_milliseconds_round_up_and_standard_retry_after_is_preserved() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-amzn-kiro-ratelimit-retry-after",
            http::HeaderValue::from_static("1500"),
        );
        assert_eq!(rate_limit_error(&headers, "{}").retry_after(), Some("2"));
        headers.insert("retry-after", http::HeaderValue::from_static("90"));
        assert_eq!(rate_limit_error(&headers, "{}").retry_after(), Some("90"));
        assert_eq!(
            rate_limit_error(
                &http::HeaderMap::new(),
                r#"{"retryAfterMilliseconds":1001}"#
            )
            .retry_after(),
            Some("2")
        );
        assert_eq!(
            rate_limit_error(&http::HeaderMap::new(), r#"{"retryAfterMilliseconds":-1}"#)
                .retry_after(),
            None
        );
    }
}
