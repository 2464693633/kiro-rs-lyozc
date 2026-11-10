//! Per-account selection of Kiro's CreateResponse protocol. The provider keeps
//! its existing event contract by translating SSE incrementally at this boundary.

mod rate_limit;
mod request;
mod stream;

pub(crate) use rate_limit::rate_limit_error;
pub(crate) use request::{decorate_request, transform_body};
pub(crate) use stream::{normalize_error, normalize_response};

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use futures::StreamExt;
use parking_lot::Mutex;
use reqwest::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::kiro::{endpoint::RequestContext, kiro_version};

const FEATURE_TTL: Duration = Duration::from_secs(900);
const FEATURE_RETRY: Duration = Duration::from_secs(60);
const FEATURE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FEATURE_BYTES: usize = 1024 * 1024;
const MAX_CACHED_ACCOUNTS: usize = 512;

#[derive(Default)]
struct Selection {
    identity: [u8; 32],
    expires: Option<Instant>,
    create_response: bool,
}

#[derive(Default)]
pub(crate) struct FeatureCache {
    accounts: Mutex<HashMap<u64, Arc<AsyncMutex<Selection>>>>,
    #[cfg(test)]
    pub(crate) test_runtime_base: Option<String>,
}

impl FeatureCache {
    pub(crate) fn runtime_base(&self, ctx: &RequestContext<'_>) -> String {
        #[cfg(test)]
        if let Some(base) = &self.test_runtime_base {
            return base.clone();
        }
        format!(
            "https://runtime.{}.kiro.dev",
            ctx.credentials.effective_api_region(ctx.config)
        )
    }

    /// API-key/enterprise IdP and custom endpoints retain their own protocol.
    pub(crate) async fn select(
        &self,
        client: &Client,
        ctx: &RequestContext<'_>,
        endpoint: &str,
    ) -> bool {
        if !matches!(endpoint, "ide" | "cli")
            || ctx.credentials.is_api_key_credential()
            || ctx.credentials.is_external_idp_credential()
        {
            return false;
        }
        let id = ctx.credentials.id.unwrap_or(0);
        let version = kiro_version::effective(&ctx.config.kiro_version);
        let base = self.runtime_base(ctx);
        // Never store bearer tokens in the selection cache. A changed sign-in,
        // profile, region or client version cannot reuse another identity's arm.
        let identity: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&(
                ctx.token,
                ctx.credentials.streaming_profile_arn(),
                &base,
                &version,
                ctx.credentials.auth_method.as_deref(),
                ctx.machine_id,
            ))
            .expect("serializable identity"),
        )
        .into();
        let entry = {
            let mut entries = self.accounts.lock();
            if entries.len() >= MAX_CACHED_ACCOUNTS && !entries.contains_key(&id) {
                // In-flight readers own Arcs, so eviction does not cancel them.
                if let Some(old) = entries.keys().copied().next() {
                    entries.remove(&old);
                }
            }
            entries.entry(id).or_default().clone()
        };
        // Coalesce simultaneous cold requests for the same sign-in.
        let mut entry = entry.lock().await;
        if entry.identity != identity {
            *entry = Selection {
                identity,
                ..Default::default()
            };
        }
        if entry.expires.is_some_and(|until| until > Instant::now()) {
            return entry.create_response;
        }
        match tokio::time::timeout(
            FEATURE_TIMEOUT,
            read_selection(client, ctx, &base, &version),
        )
        .await
        {
            Ok(Ok(selected)) => {
                entry.create_response = selected;
                entry.expires = Some(Instant::now() + FEATURE_TTL);
            }
            _ => {
                // A transient read failure preserves the last valid arm for this
                // identity; a first failure leaves the established legacy path.
                entry.expires = Some(Instant::now() + FEATURE_RETRY);
                tracing::debug!(
                    credential_id = id,
                    "Kiro feature configuration unavailable; retaining cached protocol"
                );
            }
        }
        entry.create_response
    }
}

async fn read_selection(
    client: &Client,
    ctx: &RequestContext<'_>,
    base: &str,
    version: &str,
) -> anyhow::Result<bool> {
    let app = request::app_identity(ctx, version);
    let mut req = client.get(format!("{base}/featureConfiguration"))
        .query(&[("origin", "AI_EDITOR"), ("version", version)])
        .header("authorization", format!("Bearer {}", ctx.token))
        .header("accept", "application/json")
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("x-amzn-kiro-client-attribution", "kiro-ide")
        .header("x-amz-user-agent", format!("aws-sdk-js/1.0.0 {app}"))
        .header("user-agent", format!("aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/kiroruntime#1.0.0 m/N {app}",
            ctx.config.system_version, ctx.config.node_version));
    if let Some(profile) = ctx.credentials.streaming_profile_arn() {
        req = req.header("x-amzn-kiro-profile-arn", profile);
    }
    if let Some(kind) = request::token_type(ctx) {
        req = req.header("tokentype", kind);
    }
    let response = req.send().await?.error_for_status()?;
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > MAX_FEATURE_BYTES {
            bail!("feature configuration exceeds size limit");
        }
        body.extend_from_slice(&chunk);
    }
    let data: Value = serde_json::from_slice(&body)?;
    let configuration = data
        .get("configuration")
        .and_then(Value::as_object)
        .context("missing feature configuration")?;
    let key = hex::encode(Sha256::digest(
        b"kiro-feature-config-key-salt-3e9b1d7acreate_response_api",
    ));
    Ok(configuration.get(&key).and_then(Value::as_str) == Some("create_response"))
}

#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod tests;
