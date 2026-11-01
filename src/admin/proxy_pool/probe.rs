//! 代理连通性与出口探测。
//!
//! 一次探测通过代理并发请求三个地址：ip-api.com（出口 IP + 地理位置）、ipify（只有 IP）、
//! gstatic 204（只看连通性）。任一成功即视为可用，出口信息取能拿到的最详细的一份；
//! 某些代理只放行特定域名，所以保留只测连通性的兜底。

use std::time::Instant;

use serde::Deserialize;

use super::storage::write_file;
use super::{
    CheckSummary, MAX_PROXY_PROBE_FAILURES, ProxyEndpoint, ProxyEntry, ProxyExitInfo, ProxyHealth,
    ProxyPoolError, ProxyPoolManager,
};
use crate::http_client::{ProxyConfig, build_client};

/// 出口 IP + 地理位置（免费接口只支持 HTTP）
const GEO_LOOKUP_URL: &str = "http://ip-api.com/json/?lang=zh-CN";
/// 只返回出口 IP 的备用接口
const IP_LOOKUP_URL: &str = "https://api64.ipify.org?format=json";
/// 只测连通性：返回 204 No Content 的轻量地址，不依赖上游 Kiro
const CONNECTIVITY_URL: &str = "https://www.gstatic.com/generate_204";
/// 单次探测超时（秒），三个请求并发，总耗时不超过它
const PROBE_TIMEOUT_SECS: u64 = 8;
/// 探测响应体读取上限，防止异常代理返回超大响应
const PROBE_BODY_LIMIT: usize = 64 * 1024;
/// 写入条目与响应的错误信息长度上限
const MAX_ERROR_LEN: usize = 240;

/// 单个代理的探测结果
pub(super) enum ProbeResult {
    Ok {
        latency_ms: u32,
        exit: Option<ProxyExitInfo>,
    },
    Err {
        error: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeoResponse {
    status: Option<String>,
    message: Option<String>,
    query: Option<String>,
    country: Option<String>,
    country_code: Option<String>,
    region_name: Option<String>,
    city: Option<String>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 解析 ip-api.com 响应
pub(super) fn parse_geo(body: &str) -> Result<ProxyExitInfo, String> {
    let geo: GeoResponse =
        serde_json::from_str(body).map_err(|e| format!("ip-api 响应无法解析: {e}"))?;
    if geo.status.as_deref() != Some("success") {
        return Err(format!(
            "ip-api 查询失败: {}",
            geo.message.as_deref().unwrap_or("未知原因")
        ));
    }
    Ok(ProxyExitInfo {
        ip: non_empty(geo.query).ok_or("ip-api 响应缺少出口 IP")?,
        country: non_empty(geo.country),
        country_code: non_empty(geo.country_code),
        region: non_empty(geo.region_name),
        city: non_empty(geo.city),
    })
}

/// 解析 ipify 响应
pub(super) fn parse_ipify(body: &str) -> Result<ProxyExitInfo, String> {
    #[derive(Deserialize)]
    struct IpifyResponse {
        ip: Option<String>,
    }
    let parsed: IpifyResponse =
        serde_json::from_str(body).map_err(|e| format!("ipify 响应无法解析: {e}"))?;
    Ok(ProxyExitInfo {
        ip: non_empty(parsed.ip).ok_or("ipify 响应缺少出口 IP")?,
        country: None,
        country_code: None,
        region: None,
        city: None,
    })
}

/// 从错误信息中抹掉代理密码（含百分号编码形式），并截断过长内容。
pub(super) fn redact(message: &str, endpoint: &ProxyEndpoint) -> String {
    let mut out = message.to_string();
    if let Some(password) = endpoint.password.as_deref().filter(|p| !p.is_empty()) {
        out = out.replace(password, "***");
        let encoded = urlencoding::encode(password);
        out = out.replace(&*encoded, "***");
    }
    if out.chars().count() > MAX_ERROR_LEN {
        out = out.chars().take(MAX_ERROR_LEN).collect::<String>() + "…";
    }
    out
}

/// 拼接错误及其 source 链，reqwest 的根因（如 connection refused）在链的末端。
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(err) = source {
        let text = err.to_string();
        if parts.last() != Some(&text) {
            parts.push(text);
        }
        source = err.source();
    }
    parts.join(": ")
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<(u32, String), String> {
    let started = Instant::now();
    let mut resp = client.get(url).send().await.map_err(|e| error_chain(&e))?;
    let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
    if !resp.status().is_success() {
        return Err(format!("{url} 返回状态 {}", resp.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| error_chain(&e))? {
        body.extend_from_slice(&chunk);
        if body.len() > PROBE_BODY_LIMIT {
            return Err(format!("{url} 响应过大"));
        }
    }
    Ok((latency_ms, String::from_utf8_lossy(&body).into_owned()))
}

async fn fetch_ok(client: &reqwest::Client, url: &str) -> Result<u32, String> {
    let started = Instant::now();
    let resp = client.get(url).send().await.map_err(|e| error_chain(&e))?;
    let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
    let status = resp.status();
    if status.is_success() || status.is_redirection() {
        Ok(latency_ms)
    } else {
        Err(format!("连通性探测返回非预期状态: {status}"))
    }
}
impl ProxyPoolManager {
    /// 通过代理并发请求三个探测地址，任一成功即视为可用。
    pub(super) async fn probe_one(&self, endpoint: &ProxyEndpoint) -> ProbeResult {
        let proxy = ProxyConfig::new(endpoint.full_url());
        let client = match build_client(Some(&proxy), PROBE_TIMEOUT_SECS, self.tls_backend) {
            Ok(client) => client,
            Err(e) => {
                return ProbeResult::Err {
                    error: redact(&format!("构建探测 client 失败: {e}"), endpoint),
                };
            }
        };
        let (geo, ip, connectivity) = tokio::join!(
            fetch_text(&client, GEO_LOOKUP_URL),
            fetch_text(&client, IP_LOOKUP_URL),
            fetch_ok(&client, CONNECTIVITY_URL),
        );
        let geo = geo.and_then(|(ms, body)| parse_geo(&body).map(|exit| (ms, exit)));
        let ip = ip.and_then(|(ms, body)| parse_ipify(&body).map(|exit| (ms, exit)));
        match (geo, ip, connectivity) {
            (Ok((latency_ms, exit)), _, _) | (Err(_), Ok((latency_ms, exit)), _) => {
                ProbeResult::Ok {
                    latency_ms,
                    exit: Some(exit),
                }
            }
            (Err(_), Err(_), Ok(latency_ms)) => ProbeResult::Ok {
                latency_ms,
                exit: None,
            },
            (Err(geo_err), Err(_), Err(conn_err)) => ProbeResult::Err {
                error: redact(&format!("{conn_err}；{geo_err}"), endpoint),
            },
        }
    }
}

/// 把一次探测结果写回条目；连续失败达阈值时自动禁用。
/// 返回 `(本次不健康, 本次新自动禁用)` 供摘要统计。
pub(super) fn apply_probe_result(entry: &mut ProxyEntry, result: &ProbeResult) -> (bool, bool) {
    entry.last_checked_at = Some(chrono::Utc::now().to_rfc3339());
    match result {
        ProbeResult::Ok { latency_ms, exit } => {
            entry.health = ProxyHealth::Healthy;
            entry.latency_ms = Some(*latency_ms);
            entry.consecutive_failures = 0;
            entry.last_error = None;
            if let Some(exit) = exit {
                // 只拿到 IP 且与上次一致时保留已知的地理位置
                let keep_geo = exit.country.is_none()
                    && entry.exit.as_ref().is_some_and(|old| old.ip == exit.ip);
                if !keep_geo {
                    entry.exit = Some(exit.clone());
                }
            }
            (false, false)
        }
        ProbeResult::Err { error } => {
            entry.health = ProxyHealth::Unhealthy;
            entry.latency_ms = None;
            entry.consecutive_failures += 1;
            entry.last_error = Some(error.clone());
            tracing::warn!(
                "代理 #{} 探测失败（{}/{}）: {}",
                entry.id,
                entry.consecutive_failures,
                MAX_PROXY_PROBE_FAILURES,
                error
            );
            let mut newly_disabled = false;
            if entry.consecutive_failures >= MAX_PROXY_PROBE_FAILURES && entry.enabled {
                entry.enabled = false;
                entry.auto_disabled = true;
                newly_disabled = true;
                tracing::error!(
                    "代理 #{} 连续探测失败 {} 次，已自动禁用",
                    entry.id,
                    entry.consecutive_failures
                );
            }
            (true, newly_disabled)
        }
    }
}
impl ProxyPoolManager {
    /// 并发探测所有已启用代理，回写结果并持久化一次。
    ///
    /// 探测期间被编辑了地址的条目不回写：旧地址的结果不代表新地址。
    pub async fn check_all(&self) -> CheckSummary {
        let targets: Vec<(u64, ProxyEndpoint)> = self
            .entries
            .lock()
            .iter()
            .filter(|e| e.enabled)
            .map(|e| (e.id, e.endpoint.clone()))
            .collect();
        if targets.is_empty() {
            return CheckSummary::default();
        }

        let probes = targets.into_iter().map(|(id, endpoint)| async move {
            let result = self.probe_one(&endpoint).await;
            (id, endpoint, result)
        });
        let results = futures::future::join_all(probes).await;

        let mut summary = CheckSummary::default();
        let mut entries = self.entries.lock();
        for (id, endpoint, result) in &results {
            let Some(entry) = entries.iter_mut().find(|e| e.id == *id) else {
                continue;
            };
            if entry.endpoint != *endpoint {
                continue;
            }
            let (unhealthy, newly_disabled) = apply_probe_result(entry, result);
            if unhealthy {
                summary.unhealthy += 1;
            } else {
                summary.healthy += 1;
            }
            if newly_disabled {
                summary.auto_disabled += 1;
            }
        }
        if let Err(e) = write_file(self.path.as_deref(), &entries) {
            tracing::warn!("健康检查后持久化代理池失败: {}", e);
        }
        summary
    }

    /// 即时探测单个代理（含已禁用的），回写结果并持久化。
    pub async fn check_one(&self, id: u64) -> Result<ProxyEntry, ProxyPoolError> {
        let endpoint = self.get(id).ok_or(ProxyPoolError::NotFound)?.endpoint;
        let result = self.probe_one(&endpoint).await;

        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(ProxyPoolError::NotFound)?;
        if entry.endpoint == endpoint {
            apply_probe_result(entry, &result);
        }
        let snapshot = entry.clone();
        if let Err(e) = write_file(self.path.as_deref(), &entries) {
            tracing::warn!("探测后持久化代理池失败: {}", e);
        }
        Ok(snapshot)
    }
}
