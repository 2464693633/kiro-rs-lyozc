//! API Key 订阅源（拼车链接）拉取与解析。
//!
//! 面向返回如下形状 JSON 的 HTTP 源：
//!
//! ```jsonc
//! { "code": 0, "msg": "…",
//!   "data": [{ "claimId": 149, "channelName": "sddy 8.7 下午",
//!              "seatNo": 1, "seatsTotal": 20,
//!              "claimTime": 1786241375667, "warrantyUntil": 1786248575667,
//!              "account": { "key": "ksk_…", "zone": "eu",
//!                           "aws_region": "eu-central-1", "status": "active" } }] }
//! ```
//!
//! 本模块只负责「拉 + 解析 + 映射成 [`AddCredentialRequest`]」，
//! 入库/去重/验活全部交给 [`crate::admin::AdminService::import_one_credential`]。
//!
//! # 为什么不需要自己记账已导入的 key
//!
//! [`crate::kiro::token_manager::MultiTokenManager::add_credential`] 对 api_key 类
//! 凭据按 `sha256(kiroApiKey)` 去重，且这一步发生在任何网络调用**之前**。
//! 所以重复拉到已入库的 key 的代价是零次上游请求，轮询天然幂等。

use serde::Deserialize;

use crate::admin::types::AddCredentialRequest;
use crate::http_client::{self, ProxyConfig};
use crate::model::config::{KeyFeedConfig, TlsBackend};

/// 拉取超时（秒）。发号接口通常很快，给 30 秒足够容忍一次慢响应。
const FETCH_TIMEOUT_SECS: u64 = 30;

/// `source_channel` 上的机器可读前缀。
///
/// reaper 靠它区分「本订阅源导入的凭据」与手工添加的凭据，务必不要改动已有格式，
/// 否则历史凭据会脱离清理范围（只会漏清，不会误删）。
pub const SOURCE_CHANNEL_PREFIX: &str = "keyfeed:";

/// 订阅源顶层响应。
#[derive(Debug, Deserialize)]
pub struct FeedResponse {
    /// 业务状态码，0 表示成功。
    #[serde(default)]
    pub code: i64,
    /// 人类可读提示，失败时作为错误信息回显给前端。
    #[serde(default)]
    pub msg: Option<String>,
    /// 已发到的号位列表。队列中暂无可用号时为空数组（而非 null）。
    #[serde(default)]
    pub data: Vec<FeedItem>,
}

/// 单个号位（一个「座位」）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedItem {
    #[serde(default)]
    pub claim_id: Option<i64>,
    #[serde(default)]
    pub channel_name: Option<String>,
    #[serde(default)]
    pub seat_no: Option<i64>,
    /// 保修截止（毫秒时间戳）。仅用于日志观测，不参与「到点即删」判断。
    #[serde(default)]
    pub warranty_until: Option<i64>,
    /// 号位上的账号。可能为 null（占位但还没发号）。
    #[serde(default)]
    pub account: Option<FeedAccount>,
}

/// 号位携带的账号信息。
///
/// 字段名混用 camelCase（外层）与 snake_case（`aws_region` / `issuer_url`），
/// 故此结构体不加 `rename_all`，按原样声明。
#[derive(Debug, Deserialize)]
pub struct FeedAccount {
    /// Kiro API Key，形如 `ksk_` + 36 字符。
    #[serde(default)]
    pub key: Option<String>,
    /// 粗粒度区域（如 `eu` / `us`）。仅日志用，实际取 [`Self::aws_region`]。
    #[serde(default)]
    pub zone: Option<String>,
    /// AWS 区域（如 `eu-central-1`），映射到凭据的 `apiRegion`。
    #[serde(default)]
    pub aws_region: Option<String>,
    /// 账号状态，仅 `active` 会被导入。
    #[serde(default)]
    pub status: Option<String>,
}

/// 一条可导入的号位（已过滤掉无 key / 非 active 的条目）。
#[derive(Debug)]
pub struct FeedCandidate {
    pub request: AddCredentialRequest,
    /// 原始号位标识，用于日志（不含密钥）。
    pub label: String,
    /// 保修截止（RFC3339）。仅作展示/观测，不参与「到点即删」判断 ——
    /// 保修期过了 key 未必失效，硬删会浪费还能用的号。
    pub warranty_until: Option<String>,
}

/// 毫秒时间戳 → RFC3339。非法值返回 `None` 而不是塞个假时间。
fn ms_to_rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.to_rfc3339())
}

/// 拉取失败原因。
#[derive(Debug, thiserror::Error)]
pub enum KeyFeedError {
    #[error("未配置订阅源 URL")]
    NotConfigured,
    #[error("构建 HTTP Client 失败: {0}")]
    ClientBuild(String),
    #[error("请求订阅源失败: {0}")]
    Request(String),
    #[error("订阅源返回 HTTP {0}")]
    HttpStatus(u16),
    #[error("解析订阅源响应失败: {0}")]
    Parse(String),
    #[error("订阅源返回业务错误 (code={code}): {msg}")]
    Upstream { code: i64, msg: String },
}

/// 把用户粘贴的完整链接拆成 (base_url, token)。
///
/// 用户拿到的往往是 `https://host/get?token=abc` 这种完整链接。整条存进 `url` 会让
/// Admin API 回显令牌明文，所以入口处就拆开：令牌单独存字段，响应只回 `tokenSet`。
///
/// `token_param` 是令牌的查询参数名（不同源可能叫 `token` / `key` / `accessKey`），
/// 大小写不敏感匹配。
///
/// - 含该参数 → 摘出来，其余参数保留在 base；
/// - 不含 → **原样返回输入字符串**，token 为 `None`。
///
/// # 为什么「不含该参数」要走原样返回的快速路径
///
/// 订阅源地址由用户自行配置，可能是任意第三方服务。`Url::query_pairs_mut` 重建查询串
/// 会按 `application/x-www-form-urlencoded` 重新编码，把 `%20` 改写成 `+`
/// （已实测）。多数服务器两者都当空格，但只要链接里带 HMAC / 签名类参数，
/// 任何一个字节的改写都会让签名校验失败。既然无事可做，就一个字节都别碰。
pub fn split_url_and_token(raw: &str, token_param: &str) -> (String, Option<String>) {
    let trimmed = raw.trim();
    let param = {
        let p = token_param.trim();
        if p.is_empty() { "token" } else { p }
    };

    let Ok(mut url) = reqwest::Url::parse(trimmed) else {
        // 解析不了就原样返回，交给后续请求阶段报错，避免在这里吞掉用户输入
        return (trimmed.to_string(), None);
    };

    // 先只做检测，不改 URL：没有目标参数就走零改写的快速路径
    let matches_param = |k: &str| k.eq_ignore_ascii_case(param);
    if !url.query_pairs().any(|(k, _)| matches_param(&k)) {
        return (trimmed.to_string(), None);
    }

    let mut token: Option<String> = None;
    let retained: Vec<(String, String)> = url
        .query_pairs()
        .filter_map(|(k, v)| {
            if matches_param(&k) {
                if !v.trim().is_empty() {
                    token = Some(v.trim().to_string());
                }
                None
            } else {
                Some((k.into_owned(), v.into_owned()))
            }
        })
        .collect();

    // 重建查询串：全部摘完则彻底清空 query（避免留下裸 "?"）
    {
        let mut query = url.query_pairs_mut();
        query.clear();
        for (k, v) in &retained {
            query.append_pair(k, v);
        }
    }
    if retained.is_empty() {
        url.set_query(None);
    }

    (url.to_string(), token)
}

/// 按配置拉取一次订阅源并返回解析后的响应。
pub async fn fetch(
    config: &KeyFeedConfig,
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> Result<FeedResponse, KeyFeedError> {
    let url = config.request_url().ok_or(KeyFeedError::NotConfigured)?;

    let client = http_client::build_client(proxy, FETCH_TIMEOUT_SECS, tls_backend)
        .map_err(|e| KeyFeedError::ClientBuild(e.to_string()))?;

    let response = client
        .get(&url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| KeyFeedError::Request(redact_token(&e.to_string(), config)))?;

    let status = response.status();
    if !status.is_success() {
        return Err(KeyFeedError::HttpStatus(status.as_u16()));
    }

    let body = response
        .text()
        .await
        .map_err(|e| KeyFeedError::Request(redact_token(&e.to_string(), config)))?;

    let feed: FeedResponse =
        serde_json::from_str(&body).map_err(|e| KeyFeedError::Parse(e.to_string()))?;

    if feed.code != 0 {
        return Err(KeyFeedError::Upstream {
            code: feed.code,
            msg: feed.msg.clone().unwrap_or_default(),
        });
    }

    Ok(feed)
}

/// reqwest 的错误信息会把请求 URL 原样带上，其中含 `?token=`。
/// 这些字符串会进日志和 Admin API 响应，先把 token 换成占位符。
fn redact_token(message: &str, config: &KeyFeedConfig) -> String {
    let token = config
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    match token {
        Some(t) => message.replace(t, "<REDACTED>"),
        None => message.to_string(),
    }
}

/// 把订阅源响应映射成待导入的凭据请求。
///
/// 过滤规则：
/// - `account` 为 null（占位未发号）→ 跳过；
/// - `key` 缺失/空 → 跳过；
/// - `status` 存在且不等于 `active`（忽略大小写）→ 跳过。
///   `status` 缺失时视为可用，宁可交给验活去判断，也不因源加了新字段而漏导。
pub fn to_candidates(feed: &FeedResponse, config: &KeyFeedConfig) -> Vec<FeedCandidate> {
    feed.data
        .iter()
        .filter_map(|item| to_candidate(item, config))
        .collect()
}

fn to_candidate(item: &FeedItem, config: &KeyFeedConfig) -> Option<FeedCandidate> {
    let account = item.account.as_ref()?;

    let key = account
        .key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())?;

    if let Some(status) = account.status.as_deref().map(str::trim)
        && !status.is_empty()
        && !status.eq_ignore_ascii_case("active")
    {
        return None;
    }

    let api_region = account
        .aws_region
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string);

    // 号位标识：进日志与前端明细，不含密钥。
    // 带上 seat/zone 便于在多渠道多区域下一眼定位是哪个号位。
    let label = {
        let channel = item
            .channel_name
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .unwrap_or("unknown");
        let mut label = match item.claim_id {
            Some(id) => format!("{}#{}", channel, id),
            None => channel.to_string(),
        };
        if let Some(seat) = item.seat_no {
            label.push_str(&format!(" seat{}", seat));
        }
        if let Some(zone) = account
            .zone
            .as_deref()
            .map(str::trim)
            .filter(|z| !z.is_empty())
        {
            label.push_str(&format!(" [{}]", zone));
        }
        label
    };

    let request = AddCredentialRequest {
        refresh_token: None,
        access_token: None,
        profile_arn: None,
        expires_at: None,
        auth_method: "api_key".to_string(),
        provider: None,
        client_id: None,
        client_secret: None,
        start_url: None,
        token_endpoint: None,
        issuer_url: None,
        scopes: None,
        priority: config.priority,
        // region 走的是 OIDC 刷新链路，api_key 凭据不刷新 token，填了只是噪音。
        // endpoint 层用 effective_api_region 拼 q.{region}.amazonaws.com，故只填 api_region。
        region: None,
        auth_region: None,
        api_region,
        machine_id: None,
        email: None,
        proxy_url: None,
        proxy_username: None,
        proxy_password: None,
        kiro_api_key: Some(key.to_string()),
        endpoint: config.endpoint.clone(),
        groups: config.groups.clone(),
        source_channel: Some(format!("{}{}", SOURCE_CHANNEL_PREFIX, label)),
        upstream_base_url: None,
        upstream_api_key: None,
    };

    Some(FeedCandidate {
        request,
        label,
        warranty_until: item.warranty_until.and_then(ms_to_rfc3339),
    })
}

/// 判断某个 `source_channel` 是否由本订阅源写入。
pub fn is_feed_source(source_channel: Option<&str>) -> bool {
    source_channel.is_some_and(|s| s.starts_with(SOURCE_CHANNEL_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_config() -> KeyFeedConfig {
        KeyFeedConfig {
            enabled: true,
            url: Some("https://example.com/get".to_string()),
            token: Some("tok123".to_string()),
            ..KeyFeedConfig::default()
        }
    }

    const SAMPLE: &str = r#"{
        "code": 0,
        "msg": "ok",
        "data": [
            {"claimId": 149, "channelId": 2, "channelName": "sddy 8.7 下午",
             "seatNo": 1, "seatsTotal": 20, "claimType": 3,
             "claimTime": 1786241375667, "warrantyUntil": 1786248575667,
             "account": {"key": "ksk_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                         "account": null, "password": null, "issuer_url": null,
                         "zone": "eu", "aws_region": "eu-central-1", "status": "active"}},
            {"claimId": 151, "channelName": "sddy 8.7 下午", "seatNo": 2,
             "account": {"key": "ksk_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                         "zone": "us", "aws_region": "us-east-1", "status": "active"}}
        ]
    }"#;

    #[test]
    fn test_parse_sample_feed() {
        let feed: FeedResponse = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(feed.code, 0);
        assert_eq!(feed.data.len(), 2);
        let first = &feed.data[0];
        assert_eq!(first.claim_id, Some(149));
        assert_eq!(first.warranty_until, Some(1786248575667));
        let account = first.account.as_ref().unwrap();
        assert_eq!(account.aws_region.as_deref(), Some("eu-central-1"));
        assert_eq!(account.zone.as_deref(), Some("eu"));
    }

    #[test]
    fn test_empty_data_is_not_error() {
        // 「暂无可用号」是正常状态：code 仍为 0，data 为空数组
        let json = r#"{"code":0,"msg":"暂无可用号，正在补货","data":[]}"#;
        let feed: FeedResponse = serde_json::from_str(json).unwrap();
        assert_eq!(feed.code, 0);
        assert!(feed.data.is_empty());
        assert!(to_candidates(&feed, &feed_config()).is_empty());
    }

    #[test]
    fn test_to_candidates_maps_region_and_source() {
        let feed: FeedResponse = serde_json::from_str(SAMPLE).unwrap();
        let mut config = feed_config();
        config.priority = 7;
        config.groups = vec!["pool-a".to_string()];
        config.endpoint = Some("ide".to_string());

        let candidates = to_candidates(&feed, &config);
        assert_eq!(candidates.len(), 2);

        let first = &candidates[0];
        assert_eq!(first.label, "sddy 8.7 下午#149 seat1 [eu]");
        assert_eq!(first.request.auth_method, "api_key");
        assert_eq!(
            first.request.kiro_api_key.as_deref(),
            Some("ksk_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        // aws_region 落到 api_region，不写 region
        assert_eq!(first.request.api_region.as_deref(), Some("eu-central-1"));
        assert_eq!(first.request.region, None);
        assert_eq!(first.request.auth_region, None);
        // 配置项透传
        assert_eq!(first.request.priority, 7);
        assert_eq!(first.request.groups, vec!["pool-a".to_string()]);
        assert_eq!(first.request.endpoint.as_deref(), Some("ide"));
        // source_channel 带机器可读前缀，供 reaper 识别
        let source = first.request.source_channel.as_deref().unwrap();
        assert!(source.starts_with(SOURCE_CHANNEL_PREFIX));
        assert!(is_feed_source(Some(source)));

        assert_eq!(candidates[1].request.api_region.as_deref(), Some("us-east-1"));
    }

    #[test]
    fn test_warranty_until_converted_to_rfc3339() {
        let feed: FeedResponse = serde_json::from_str(SAMPLE).unwrap();
        let candidates = to_candidates(&feed, &feed_config());

        // 1786248575667 ms → 2026-08-09T04:09:35.667+00:00
        let warranty = candidates[0].warranty_until.as_deref().unwrap();
        assert!(
            warranty.starts_with("2026-08-09T04:09:35"),
            "预期 2026-08-09T04:09:35 开头，实际 {}",
            warranty
        );

        // 源未给 warrantyUntil → None，不编造时间
        assert_eq!(candidates[1].warranty_until, None);
    }

    #[test]
    fn test_label_without_optional_parts() {
        // 缺 seatNo / zone 时标签不应出现空的 "seat" / "[]" 片段
        let json = r#"{"code":0,"data":[
            {"claimId":7,"channelName":"c1","account":{"key":"ksk_x","status":"active"}}
        ]}"#;
        let feed: FeedResponse = serde_json::from_str(json).unwrap();
        let candidates = to_candidates(&feed, &feed_config());
        assert_eq!(candidates[0].label, "c1#7");
    }

    #[test]
    fn test_filters_inactive_and_missing_key() {
        let json = r#"{"code":0,"data":[
            {"claimId":1,"account":{"key":"ksk_active_one","status":"active"}},
            {"claimId":2,"account":{"key":"ksk_expired_one","status":"expired"}},
            {"claimId":3,"account":{"key":"","status":"active"}},
            {"claimId":4,"account":null},
            {"claimId":5,"account":{"key":"ksk_no_status_field"}}
        ]}"#;
        let feed: FeedResponse = serde_json::from_str(json).unwrap();
        let candidates = to_candidates(&feed, &feed_config());

        // 保留：active 的、以及 status 缺失的（交给验活判断）
        let keys: Vec<&str> = candidates
            .iter()
            .map(|c| c.request.kiro_api_key.as_deref().unwrap())
            .collect();
        assert_eq!(keys, vec!["ksk_active_one", "ksk_no_status_field"]);
    }

    #[test]
    fn test_status_case_insensitive() {
        let json = r#"{"code":0,"data":[
            {"claimId":1,"account":{"key":"ksk_one","status":"ACTIVE"}}
        ]}"#;
        let feed: FeedResponse = serde_json::from_str(json).unwrap();
        assert_eq!(to_candidates(&feed, &feed_config()).len(), 1);
    }

    #[test]
    fn test_request_url_composition() {
        let config = feed_config();
        assert_eq!(
            config.request_url().as_deref(),
            Some("https://example.com/get?token=tok123")
        );

        // base 自带查询串 → 用 & 拼接
        let mut with_query = feed_config();
        with_query.url = Some("https://example.com/get?zone=eu".to_string());
        assert_eq!(
            with_query.request_url().as_deref(),
            Some("https://example.com/get?zone=eu&token=tok123")
        );

        // 无 token → 只返回 base
        let mut no_token = feed_config();
        no_token.token = None;
        assert_eq!(
            no_token.request_url().as_deref(),
            Some("https://example.com/get")
        );

        // 无 url → None
        let mut no_url = feed_config();
        no_url.url = None;
        assert_eq!(no_url.request_url(), None);
    }

    #[test]
    fn test_split_url_and_token() {
        let (url, token) = split_url_and_token("https://example.com/get?token=abc123", "token");
        assert_eq!(url, "https://example.com/get");
        assert_eq!(token.as_deref(), Some("abc123"));

        // 其它查询参数保留
        let (url, token) =
            split_url_and_token("https://example.com/get?zone=eu&token=abc123", "token");
        assert!(url.starts_with("https://example.com/get?"));
        assert!(url.contains("zone=eu"));
        assert!(!url.contains("token"));
        assert_eq!(token.as_deref(), Some("abc123"));

        // 无 token 参数 → 原样
        let (url, token) = split_url_and_token("https://example.com/get", "token");
        assert_eq!(url, "https://example.com/get");
        assert_eq!(token, None);

        // 空 token 值不算配了 token
        let (_, token) = split_url_and_token("https://example.com/get?token=", "token");
        assert_eq!(token, None);

        // 非法 URL 原样返回，不吞输入
        let (url, token) = split_url_and_token("not a url", "token");
        assert_eq!(url, "not a url");
        assert_eq!(token, None);
    }

    /// 换源后令牌参数名不同（`accessKey` / `key` …）时，指定名字即可正常拆分脱敏。
    #[test]
    fn test_split_url_with_custom_token_param() {
        let (url, token) = split_url_and_token(
            "http://127.0.0.1:8899/v2/dispense?accessKey=SECRET123",
            "accessKey",
        );
        assert_eq!(url, "http://127.0.0.1:8899/v2/dispense");
        assert_eq!(token.as_deref(), Some("SECRET123"));

        // 参数名大小写不敏感
        let (url, token) = split_url_and_token("https://e.com/get?ACCESSKEY=S1", "accessKey");
        assert_eq!(url, "https://e.com/get");
        assert_eq!(token.as_deref(), Some("S1"));

        // 指定了 accessKey 时，叫 token 的参数不该被摘走（它可能是业务参数）
        let (url, token) = split_url_and_token("https://e.com/get?token=notsecret", "accessKey");
        assert_eq!(url, "https://e.com/get?token=notsecret");
        assert_eq!(token, None);

        // 空参数名回退到默认 token
        let (url, token) = split_url_and_token("https://e.com/get?token=abc", "   ");
        assert_eq!(url, "https://e.com/get");
        assert_eq!(token.as_deref(), Some("abc"));
    }

    /// 自定义参数名要贯穿到请求 URL 的拼接。
    #[test]
    fn test_request_url_uses_custom_token_param() {
        let config = KeyFeedConfig {
            url: Some("http://127.0.0.1:8899/v2/dispense".to_string()),
            token: Some("SECRET123".to_string()),
            token_param: "accessKey".to_string(),
            ..KeyFeedConfig::default()
        };
        assert_eq!(
            config.request_url().as_deref(),
            Some("http://127.0.0.1:8899/v2/dispense?accessKey=SECRET123")
        );

        // 空参数名回退默认
        let fallback = KeyFeedConfig {
            url: Some("https://e.com/get".to_string()),
            token: Some("t1".to_string()),
            token_param: String::new(),
            ..KeyFeedConfig::default()
        };
        assert_eq!(fallback.effective_token_param(), "token");
        assert_eq!(
            fallback.request_url().as_deref(),
            Some("https://e.com/get?token=t1")
        );
    }

    /// 订阅源地址由用户自填，可能是任意第三方服务。不含 `token` 参数时必须
    /// **一个字节都不改写** —— 否则带签名的链接会因重新编码而失效。
    #[test]
    fn test_split_url_preserves_bytes_when_no_token_param() {
        for raw in [
            // %20 曾被重建逻辑改写成 +
            "https://other.example.com/fetch?t=xyz&note=hello%20world",
            // 签名类参数：任何改写都会让校验失败
            "https://other.example.com/api/get?key=abc&sig=a%2Bb%2Fc%3D",
            // 无查询串
            "https://other.example.com/plain",
            // 保留裸参数与大小写
            "https://Other.example.com/Get?A=1&flag",
            // 路径里带 token 的源（不是查询参数，不该被摘）
            "https://other.example.com/token/abc123/get",
        ] {
            let (url, token) = split_url_and_token(raw, "token");
            assert_eq!(url, raw, "不含 token 参数的 URL 必须逐字节保持原样: {}", raw);
            assert_eq!(token, None, "不该从 {} 里摘出 token", raw);
        }
    }

    /// 换源后 token 可能叫别的名字（`key` / `t` / `auth`…）。此时不拆分，
    /// 整条 URL 原样作为 base 发出去 —— 功能照常，只是响应里会回显该参数。
    #[test]
    fn test_non_token_param_name_still_works() {
        let raw = "https://other.example.com/get?key=secret123";
        let (base, token) = split_url_and_token(raw, "token");
        assert_eq!(base, raw);
        assert_eq!(token, None);

        // 未拆出 token 时，request_url() 直接用 base，不追加 ?token=
        let config = KeyFeedConfig {
            url: Some(base),
            token: None,
            ..KeyFeedConfig::default()
        };
        assert_eq!(config.request_url().as_deref(), Some(raw));
    }

    /// 已存 token 且 base 自带其它参数时，拼接不能破坏原有查询串。
    #[test]
    fn test_request_url_appends_without_clobbering_existing_query() {
        let config = KeyFeedConfig {
            url: Some("https://other.example.com/get?zone=eu&v=2".to_string()),
            token: Some("tok/with+special=chars".to_string()),
            ..KeyFeedConfig::default()
        };
        let url = config.request_url().unwrap();
        assert!(url.starts_with("https://other.example.com/get?zone=eu&v=2&token="));
        // token 需 URL 编码，否则 / + = 会破坏查询串结构
        assert!(!url.contains("tok/with+special=chars"));
        assert!(url.contains("tok%2Fwith%2Bspecial%3Dchars"));
    }

    #[test]
    fn test_interval_floor() {
        let mut config = feed_config();
        config.interval_secs = 5;
        assert_eq!(config.effective_interval_secs(), 60);
        config.interval_secs = 600;
        assert_eq!(config.effective_interval_secs(), 600);
    }

    #[test]
    fn test_redact_token_in_error_message() {
        let config = feed_config();
        let raw = "error sending request for url (https://example.com/get?token=tok123)";
        let redacted = redact_token(raw, &config);
        assert!(!redacted.contains("tok123"));
        assert!(redacted.contains("<REDACTED>"));
    }

    #[test]
    fn test_is_feed_source() {
        assert!(is_feed_source(Some("keyfeed:chan#1")));
        assert!(!is_feed_source(Some("手工添加")));
        assert!(!is_feed_source(None));
    }
}
