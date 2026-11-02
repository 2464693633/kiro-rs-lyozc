//! Use the structured IP-management entries for upstream failover decisions.

use super::{ProxyEndpoint, ProxyHealth, ProxyPoolManager, fresh_entry};
use crate::http_client::ProxyConfig;
use std::sync::atomic::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyAvailability {
    Available,
    Unavailable,
    Untracked,
}

fn endpoint_for(proxy: &ProxyConfig) -> Result<ProxyEndpoint, String> {
    let mut endpoint = ProxyEndpoint::parse(&proxy.url)?;
    if let (Some(username), Some(password)) = (&proxy.username, &proxy.password) {
        endpoint.username = Some(username.clone());
        endpoint.password = Some(password.clone());
    }
    endpoint.normalize()
}

impl ProxyPoolManager {
    pub fn availability_for_proxy(&self, proxy: &ProxyConfig) -> ProxyAvailability {
        let Ok(endpoint) = endpoint_for(proxy) else {
            return ProxyAvailability::Untracked;
        };
        match self
            .entries
            .lock()
            .iter()
            .find(|entry| entry.endpoint == endpoint)
        {
            None => ProxyAvailability::Untracked,
            Some(entry) if !entry.enabled && !entry.auto_disabled => ProxyAvailability::Unavailable,
            Some(entry) if entry.auto_detected && entry.health == ProxyHealth::Unknown => {
                ProxyAvailability::Untracked
            }
            Some(entry) if entry.enabled && entry.health != ProxyHealth::Unhealthy => {
                ProxyAvailability::Available
            }
            Some(_) => ProxyAvailability::Unavailable,
        }
    }

    /// Register exact endpoints, including authentication, using the same storage
    /// and redacted Admin representation as manually added IP-management entries.
    pub fn register_detected(&self, proxies: &[ProxyConfig]) -> usize {
        let mut entries = self.entries.lock();
        let original_len = entries.len();
        for proxy in proxies {
            let Ok(endpoint) = endpoint_for(proxy) else {
                continue;
            };
            if entries.iter().any(|entry| entry.endpoint == endpoint) {
                continue;
            }
            let mut entry = fresh_entry(
                self.next_id.fetch_add(1, Ordering::Relaxed),
                endpoint.default_name(),
                endpoint,
            );
            entry.auto_detected = true;
            entries.push(entry);
        }
        let added = entries.len() - original_len;
        if added > 0 {
            if let Err(error) = self.persist(&entries) {
                entries.truncate(original_len);
                tracing::warn!("自动发现的代理保存失败: {error}");
                return 0;
            }
        }
        added
    }

    /// Internal routing needs the full authenticated URL. Admin list responses
    /// continue to expose only redacted endpoint fields.
    pub fn assignable_urls(&self) -> Vec<String> {
        self.entries
            .lock()
            .iter()
            .filter(|entry| {
                entry.enabled
                    && entry.health != ProxyHealth::Unhealthy
                    && (!entry.auto_detected || entry.health == ProxyHealth::Healthy)
            })
            .map(|entry| entry.endpoint.full_url())
            .collect()
    }
}
