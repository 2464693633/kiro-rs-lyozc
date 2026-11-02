//! 账号与代理池的关联：解析账号提交的代理选择、把自定义代理收进代理池、启动对齐、分配。

use super::AdminService;
use super::proxy::{linked_binding, pool_error};
use crate::admin::error::AdminServiceError;
use crate::admin::proxy_pool::ProxyEndpoint;
use crate::admin::types::{AssignProxyRequest, AssignRoundRobinResponse};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::CredentialProxyBinding;

/// 解析后的账号代理配置（凭据 ID 未知时使用）
pub(super) struct ProxySetting {
    pub proxy_id: Option<u64>,
    pub proxy_url: Option<String>,
    pub proxy_username: Option<String>,
    pub proxy_password: Option<String>,
    /// 自定义代理的地址：写入账号后需要收进代理池
    pub adopt: Option<ProxyEndpoint>,
}

impl ProxySetting {
    fn global() -> Self {
        Self {
            proxy_id: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            adopt: None,
        }
    }

    pub fn binding(&self, credential_id: u64) -> CredentialProxyBinding {
        CredentialProxyBinding {
            credential_id,
            proxy_id: self.proxy_id,
            proxy_url: self.proxy_url.clone(),
            proxy_username: self.proxy_username.clone(),
            proxy_password: self.proxy_password.clone(),
        }
    }
}

/// 账号代理副本实际生效的地址：单独的用户名与密码都存在时覆盖地址里的认证（与运行时一致）。
fn copy_endpoint(
    url: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<ProxyEndpoint, String> {
    let mut endpoint = ProxyEndpoint::parse(url)?;
    if let (Some(user), Some(pass)) = (username, password) {
        endpoint.username = Some(user.to_string());
        endpoint.password = Some(pass.to_string());
        endpoint = endpoint.normalize()?;
    }
    Ok(endpoint)
}

impl AdminService {
    /// 解析添加或编辑账号时提交的代理字段：
    /// `proxy_id` 优先；否则 `proxy_url` 为空表示全局代理，`direct` 表示直连，其余为自定义代理。
    /// `current_proxy_id` 是账号当前关联的代理：重新保存已禁用的当前代理不报错。
    pub(super) fn resolve_proxy_setting(
        &self,
        proxy_id: Option<u64>,
        proxy_url: Option<&str>,
        proxy_username: Option<&str>,
        proxy_password: Option<&str>,
        current_proxy_id: Option<u64>,
    ) -> Result<ProxySetting, AdminServiceError> {
        if let Some(id) = proxy_id {
            let entry = self
                .proxy_pool
                .get(id)
                .ok_or(AdminServiceError::ProxyNotFound { id })?;
            if !entry.enabled && current_proxy_id != Some(id) {
                return Err(AdminServiceError::BadRequest(format!(
                    "代理 #{id} 已被禁用，请先启用后再分配"
                )));
            }
            let binding = linked_binding(0, &entry);
            return Ok(ProxySetting {
                proxy_id: binding.proxy_id,
                proxy_url: binding.proxy_url,
                proxy_username: None,
                proxy_password: None,
                adopt: None,
            });
        }
        let url = proxy_url.map(str::trim).unwrap_or("");
        if url.is_empty() {
            return Ok(ProxySetting::global());
        }
        if url.eq_ignore_ascii_case(KiroCredentials::PROXY_DIRECT) {
            return Ok(ProxySetting {
                proxy_url: Some(KiroCredentials::PROXY_DIRECT.to_string()),
                ..ProxySetting::global()
            });
        }
        let clean = |v: Option<&str>| {
            v.map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let mut endpoint = ProxyEndpoint::parse(url).map_err(AdminServiceError::BadRequest)?;
        // 单独更新用户名或密码时，沿用 URL 中未修改的另一项认证信息。
        if let Some(username) = clean(proxy_username) {
            endpoint.username = Some(username);
        }
        if let Some(password) = clean(proxy_password) {
            endpoint.password = Some(password);
        }
        let endpoint = endpoint
            .normalize()
            .map_err(AdminServiceError::BadRequest)?;
        Ok(ProxySetting {
            proxy_id: None,
            // 先写入完整认证地址，收进代理池失败时本次修改也能立即生效。
            proxy_url: Some(endpoint.full_url()),
            proxy_username: None,
            proxy_password: None,
            adopt: Some(endpoint),
        })
    }
    /// 把账号的自定义代理收进代理池（已存在则复用）并关联，副本改写为代理池的配置。
    pub(super) fn adopt_custom_proxy(
        &self,
        credential_id: u64,
        endpoint: ProxyEndpoint,
    ) -> Result<(), AdminServiceError> {
        let (entry, created) = self
            .proxy_pool
            .find_or_add(endpoint)
            .map_err(|e| pool_error(e, 0))?;
        if created {
            tracing::info!(
                "凭据 #{} 的自定义代理已加入 IP 管理: #{}",
                credential_id,
                entry.id
            );
        }
        self.token_manager
            .set_credential_proxy(linked_binding(credential_id, &entry))
            .map_err(|e| self.classify_error(e, credential_id))
    }

    /// 启动时把账号与代理池对齐：
    /// - 未入池的自定义代理收进代理池并关联（只写关联，不改动账号原有的代理字段）；
    /// - 已关联的账号与代理池配置不一致时，以代理池为准同步副本；
    /// - 关联的代理已不存在时，按账号自身的代理配置重新收进代理池；
    /// - 全局代理或直连的账号清除残留的关联。
    pub fn migrate_credential_proxies(&self) {
        let (mut linked, mut synced) = (0usize, 0usize);
        let mut fixes = Vec::new();
        for binding in self.token_manager.proxy_bindings() {
            let url = binding.proxy_url.as_deref().map(str::trim).unwrap_or("");
            if url.is_empty() || url.eq_ignore_ascii_case(KiroCredentials::PROXY_DIRECT) {
                if binding.proxy_id.is_some() {
                    fixes.push(CredentialProxyBinding {
                        proxy_id: None,
                        ..binding
                    });
                }
                continue;
            }
            let endpoint = match copy_endpoint(
                url,
                binding.proxy_username.as_deref(),
                binding.proxy_password.as_deref(),
            ) {
                Ok(endpoint) => endpoint,
                Err(e) => {
                    tracing::warn!(
                        "凭据 #{} 的代理地址无法解析，保持原样: {}",
                        binding.credential_id,
                        e
                    );
                    continue;
                }
            };
            if let Some(entry) = binding.proxy_id.and_then(|id| self.proxy_pool.get(id)) {
                if entry.endpoint != endpoint {
                    fixes.push(linked_binding(binding.credential_id, &entry));
                    synced += 1;
                }
                continue;
            }
            match self.proxy_pool.find_or_add(endpoint) {
                Ok((entry, _)) => {
                    fixes.push(CredentialProxyBinding {
                        proxy_id: Some(entry.id),
                        ..binding
                    });
                    linked += 1;
                }
                Err(e) => tracing::warn!(
                    "凭据 #{} 的代理加入 IP 管理失败: {}",
                    binding.credential_id,
                    e
                ),
            }
        }
        if fixes.is_empty() {
            return;
        }
        match self.token_manager.align_credential_proxies(&fixes) {
            Ok(_) => tracing::info!(
                "账号代理已与 IP 管理对齐：关联 {} 个，同步 {} 个，共更新 {} 个账号",
                linked,
                synced,
                fixes.len()
            ),
            Err(e) => tracing::warn!("账号代理对齐后保存凭据失败: {}", e),
        }
    }
    /// 将代理池中的代理分配给指定凭据；`proxy_id` 为空表示改回全局代理
    pub fn assign_proxy_to_credential(
        &self,
        credential_id: u64,
        req: AssignProxyRequest,
    ) -> Result<(), AdminServiceError> {
        let current = self
            .token_manager
            .proxy_bindings()
            .into_iter()
            .find(|b| b.credential_id == credential_id)
            .ok_or(AdminServiceError::NotFound { id: credential_id })?;
        let setting =
            self.resolve_proxy_setting(req.proxy_id, None, None, None, current.proxy_id)?;
        self.token_manager
            .set_credential_proxy(setting.binding(credential_id))
            .map_err(|e| self.classify_error(e, credential_id))
    }

    /// 将可用代理（已启用且最近一次探测未失败）按轮询方式批量分配给凭据
    ///
    /// - `credential_ids` 为 None 时对全部凭据分配
    /// - 无可用代理时返回错误
    pub fn assign_proxies_round_robin(
        &self,
        credential_ids: Option<Vec<u64>>,
    ) -> Result<AssignRoundRobinResponse, AdminServiceError> {
        let proxies: Vec<_> = self
            .proxy_pool
            .assignable_ids()
            .into_iter()
            .filter_map(|id| self.proxy_pool.get(id))
            .collect();
        if proxies.is_empty() {
            return Err(AdminServiceError::BadRequest(
                "没有可用代理（需已启用且健康检查未失败）".to_string(),
            ));
        }

        let existing: Vec<u64> = self
            .token_manager
            .proxy_bindings()
            .into_iter()
            .map(|b| b.credential_id)
            .collect();
        let targets: Vec<u64> = match credential_ids {
            Some(ids) if !ids.is_empty() => {
                ids.into_iter().filter(|id| existing.contains(id)).collect()
            }
            _ => existing,
        };
        let bindings: Vec<CredentialProxyBinding> = targets
            .iter()
            .enumerate()
            .map(|(i, id)| linked_binding(*id, &proxies[i % proxies.len()]))
            .collect();
        self.token_manager
            .set_credential_proxies(&bindings)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(AssignRoundRobinResponse {
            assigned: bindings.len(),
            proxy_count: proxies.len(),
        })
    }
}
