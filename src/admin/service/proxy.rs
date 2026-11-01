//! IP 管理：代理池的增删改查与探测，编辑代理后同步到关联账号。

use std::collections::HashMap;

use super::AdminService;
use crate::admin::error::AdminServiceError;
use crate::admin::proxy_pool::{ProxyEndpoint, ProxyEntry, ProxyInput, ProxyPatch, ProxyPoolError};
use crate::admin::types::{
    AddProxyRequest, BatchAddProxyRequest, ProxyCheckAllResponse, ProxyCredentialItem,
    ProxyPoolEntry, ProxyPoolResponse, UpdateProxyRequest,
};
use crate::kiro::token_manager::CredentialProxyBinding;

pub(super) fn pool_error(e: ProxyPoolError, id: u64) -> AdminServiceError {
    match e {
        ProxyPoolError::NotFound => AdminServiceError::ProxyNotFound { id },
        ProxyPoolError::Duplicate(url) => AdminServiceError::Conflict(format!("代理已存在: {url}")),
        ProxyPoolError::Invalid(msg) => AdminServiceError::BadRequest(msg),
        ProxyPoolError::Persist(msg) => {
            AdminServiceError::InternalError(format!("保存代理池失败: {msg}"))
        }
    }
}

fn entry_view(entry: ProxyEntry, credential_count: u32) -> ProxyPoolEntry {
    ProxyPoolEntry {
        id: entry.id,
        name: entry.name,
        url: entry.endpoint.display_url(),
        has_password: entry.endpoint.password.is_some(),
        protocol: entry.endpoint.protocol,
        host: entry.endpoint.host,
        port: entry.endpoint.port,
        username: entry.endpoint.username,
        enabled: entry.enabled,
        credential_count,
        exit: entry.exit,
        last_error: entry.last_error,
        health: entry.health,
        latency_ms: entry.latency_ms,
        last_checked_at: entry.last_checked_at,
        consecutive_failures: entry.consecutive_failures,
        auto_disabled: entry.auto_disabled,
    }
}

/// 关联代理池条目时写入凭据的配置副本：认证并入地址，单独的用户名/密码字段清空。
pub(super) fn linked_binding(credential_id: u64, entry: &ProxyEntry) -> CredentialProxyBinding {
    CredentialProxyBinding {
        credential_id,
        proxy_id: Some(entry.id),
        proxy_url: Some(entry.endpoint.full_url()),
        proxy_username: None,
        proxy_password: None,
    }
}

impl AdminService {
    /// 每个代理关联的凭据数
    fn proxy_credential_counts(&self) -> HashMap<u64, u32> {
        let mut counts = HashMap::new();
        for binding in self.token_manager.proxy_bindings() {
            if let Some(id) = binding.proxy_id {
                *counts.entry(id).or_insert(0) += 1;
            }
        }
        counts
    }

    fn proxy_view(&self, entry: ProxyEntry) -> ProxyPoolEntry {
        let count = self
            .proxy_credential_counts()
            .get(&entry.id)
            .copied()
            .unwrap_or(0);
        entry_view(entry, count)
    }

    /// 代理名称表，供凭据列表展示关联的代理
    pub(super) fn proxy_names(&self) -> HashMap<u64, String> {
        self.proxy_pool
            .list()
            .into_iter()
            .map(|e| (e.id, e.name))
            .collect()
    }

    /// 获取代理池列表（含关联凭据数）
    pub fn get_proxy_pool(&self) -> ProxyPoolResponse {
        let counts = self.proxy_credential_counts();
        let proxies: Vec<ProxyPoolEntry> = self
            .proxy_pool
            .list()
            .into_iter()
            .map(|entry| {
                let count = counts.get(&entry.id).copied().unwrap_or(0);
                entry_view(entry, count)
            })
            .collect();
        ProxyPoolResponse {
            total: proxies.len(),
            proxies,
        }
    }

    /// 新建代理：填结构化字段，或只给完整地址（兼容旧接口）
    pub fn add_proxy(&self, req: AddProxyRequest) -> Result<ProxyPoolEntry, AdminServiceError> {
        let endpoint = match req.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(url) => ProxyEndpoint::parse(url).map_err(AdminServiceError::BadRequest)?,
            None => ProxyEndpoint {
                protocol: req.protocol.unwrap_or_default(),
                host: req.host.unwrap_or_default(),
                port: req.port.unwrap_or(0),
                username: req.username,
                password: req.password,
            },
        };
        let name = req.name.or(req.label);
        let entry = self
            .proxy_pool
            .add(ProxyInput { name, endpoint })
            .map_err(|e| pool_error(e, 0))?;
        Ok(entry_view(entry, 0))
    }

    /// 编辑代理；地址或认证变化时同步到所有关联凭据。返回 `(条目, 地址是否变化)`。
    pub fn update_proxy(
        &self,
        id: u64,
        req: UpdateProxyRequest,
    ) -> Result<(ProxyPoolEntry, bool), AdminServiceError> {
        let patch = ProxyPatch {
            name: req.name,
            protocol: req.protocol,
            host: req.host,
            port: req.port,
            username: req.username,
            password: req.password,
        };
        let (entry, endpoint_changed) = self
            .proxy_pool
            .update(id, patch)
            .map_err(|e| pool_error(e, id))?;
        if endpoint_changed {
            self.sync_linked_credentials(&entry)?;
        }
        Ok((self.proxy_view(entry), endpoint_changed))
    }

    /// 把代理的最新地址与认证写入所有关联凭据
    fn sync_linked_credentials(&self, entry: &ProxyEntry) -> Result<usize, AdminServiceError> {
        let bindings: Vec<CredentialProxyBinding> = self
            .token_manager
            .proxy_bindings()
            .into_iter()
            .filter(|b| b.proxy_id == Some(entry.id))
            .map(|b| linked_binding(b.credential_id, entry))
            .collect();
        self.token_manager
            .set_credential_proxies(&bindings)
            .map_err(|e| {
                AdminServiceError::InternalError(format!("代理已保存，但同步到关联账号失败: {e}"))
            })
    }
    /// 删除代理；仍有账号关联时拒绝，需先给这些账号换代理
    pub fn delete_proxy(&self, id: u64) -> Result<(), AdminServiceError> {
        let in_use = self
            .proxy_credential_counts()
            .get(&id)
            .copied()
            .unwrap_or(0);
        if in_use > 0 {
            return Err(AdminServiceError::Conflict(format!(
                "代理 #{id} 仍有 {in_use} 个账号在使用，请先为这些账号更换代理"
            )));
        }
        self.proxy_pool.delete(id).map_err(|e| pool_error(e, id))
    }

    /// 设置代理启用/禁用状态。禁用只影响新的选择与轮询分配，已关联的账号继续使用。
    pub fn set_proxy_enabled(&self, id: u64, enabled: bool) -> Result<(), AdminServiceError> {
        self.proxy_pool
            .set_enabled(id, enabled)
            .map_err(|e| pool_error(e, id))
    }

    /// 批量添加代理
    pub fn batch_add_proxies(
        &self,
        req: BatchAddProxyRequest,
    ) -> (Vec<ProxyPoolEntry>, Vec<String>) {
        let (added, errors) = self.proxy_pool.batch_add(req.urls);
        let added = added.into_iter().map(|e| entry_view(e, 0)).collect();
        (added, errors)
    }

    /// 列出关联某个代理的凭据
    pub fn list_proxy_credentials(
        &self,
        id: u64,
    ) -> Result<Vec<ProxyCredentialItem>, AdminServiceError> {
        if self.proxy_pool.get(id).is_none() {
            return Err(AdminServiceError::ProxyNotFound { id });
        }
        Ok(self
            .token_manager
            .snapshot()
            .entries
            .into_iter()
            .filter(|e| e.proxy_id == Some(id))
            .map(|e| ProxyCredentialItem {
                id: e.id,
                email: e.email,
                disabled: e.disabled,
            })
            .collect())
    }

    /// 即时探测单个代理（供 UI「测试」按钮调用）
    pub async fn check_proxy(&self, id: u64) -> Result<ProxyPoolEntry, AdminServiceError> {
        let entry = self
            .proxy_pool
            .check_one(id)
            .await
            .map_err(|e| pool_error(e, id))?;
        Ok(self.proxy_view(entry))
    }

    /// 触发全部代理的健康检查
    pub async fn check_all_proxies(&self) -> ProxyCheckAllResponse {
        let summary = self.proxy_pool.check_all().await;
        ProxyCheckAllResponse {
            healthy: summary.healthy,
            unhealthy: summary.unhealthy,
            auto_disabled: summary.auto_disabled,
        }
    }

    /// 把代理池中的代理设为全局代理（认证随代理一起写入全局配置）
    pub fn set_proxy_as_global(&self, id: u64) -> Result<(), AdminServiceError> {
        let endpoint = self
            .proxy_pool
            .get(id)
            .ok_or(AdminServiceError::ProxyNotFound { id })?
            .endpoint;
        self.set_global_proxy(
            Some(endpoint.display_url()),
            endpoint.username,
            Some(endpoint.password),
        )
    }
}
