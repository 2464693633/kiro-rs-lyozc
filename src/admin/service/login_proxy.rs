//! Resolve the login route before any OAuth/OIDC request, then reuse it at import.

use super::{AdminService, AdminServiceError, KiroCredentials, ProxyConfig};
use crate::kiro::token_manager::CredentialProxyBinding;

impl AdminService {
    pub(super) fn resolve_login_proxy(
        &self,
        proxy_id: Option<u64>,
        proxy_url: Option<&str>,
        existing: Option<&KiroCredentials>,
    ) -> Result<(KiroCredentials, Option<ProxyConfig>), AdminServiceError> {
        let mut template = existing.cloned().unwrap_or_default();
        // Relogin with no explicit choice follows this account, including auth
        // and explicit direct, rather than silently switching to the global IP.
        if existing.is_none() || proxy_id.is_some() || proxy_url.is_some() {
            let setting = self.resolve_proxy_setting(proxy_id, proxy_url, None, None, None)?;
            let binding = if let Some(endpoint) = setting.adopt {
                let (entry, _) = self
                    .proxy_pool
                    .find_or_add(endpoint)
                    .map_err(|error| super::proxy::pool_error(error, 0))?;
                super::proxy::linked_binding(0, &entry)
            } else {
                setting.binding(0)
            };
            template.proxy_id = binding.proxy_id;
            template.proxy_url = binding.proxy_url;
            template.proxy_username = binding.proxy_username;
            template.proxy_password = binding.proxy_password;
            template.proxy_auto_assigned = false;
            template.proxy_temporary_direct = false;
        }
        let proxy = template.effective_proxy(self.token_manager.proxy().as_ref());
        Ok((template, proxy))
    }

    pub(super) fn save_login_proxy(
        &self,
        credential_id: u64,
        template: &KiroCredentials,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_credential_proxy(CredentialProxyBinding {
                credential_id,
                proxy_id: template.proxy_id,
                proxy_url: template.proxy_url.clone(),
                proxy_username: template.proxy_username.clone(),
                proxy_password: template.proxy_password.clone(),
            })
            .map_err(|error| self.classify_error(error, credential_id))
    }
}
