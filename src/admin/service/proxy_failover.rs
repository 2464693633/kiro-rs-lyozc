//! Upstream proxy failover integrated with the structured IP-management pool.

use super::*;

impl AdminService {
    pub(super) fn detect_credential_proxies(&self) -> usize {
        let mut in_use: Vec<ProxyConfig> = Vec::new();

        // 全局代理即使当前没有凭据回退到它，也纳入检测：它是新增凭据的默认出口
        if let Some(proxy) = self.token_manager.proxy() {
            in_use.push(proxy);
        }
        for cred in self.token_manager.credential_proxy_snapshot() {
            for proxy in [cred.in_use_proxy, cred.recovery_proxy]
                .into_iter()
                .flatten()
            {
                if !in_use.contains(&proxy) {
                    in_use.push(proxy);
                }
            }
        }

        self.proxy_pool.register_detected(&in_use)
    }

    /// 按代理池最新可用性判定轮换凭据代理。
    ///
    /// 处理三类凭据：
    /// - 在用代理被判定不可用：改为池内其它可用代理；无可用代理时，按
    ///   `proxy_direct_fallback_enabled` 决定回退直连或保持原配置；
    /// - 此前被自动回退直连、当前池内已有可用代理：改回代理；
    /// - 其余凭据（代理可用、未登记、或用户显式直连）不做改动。
    ///
    /// `proxy_auto_rotate_enabled` 关闭时直接返回空结果，不做任何改写。
    pub(super) fn rotate_unavailable_credential_proxies(&self) -> ProxyRotationResponse {
        let (auto_rotate_enabled, direct_fallback_enabled) =
            self.token_manager.get_proxy_failover_config();
        if !auto_rotate_enabled {
            return ProxyRotationResponse::default();
        }

        let available = self.proxy_pool.assignable_urls();
        let credentials = self.token_manager.credential_proxy_snapshot();

        let mut result = ProxyRotationResponse::default();
        let mut updates: Vec<ProxyRotationUpdate> = Vec::new();
        // 轮换目标在可用代理间轮转，避免所有凭据挤到同一个代理上
        let mut cursor = 0usize;
        // 无可用代理的凭据汇总后只记一条日志：这类凭据可能成批出现（例如整批账号
        // 共用一个失效代理），逐条打 ERROR 会把日志刷满
        let mut unresolved_ids: Vec<u64> = Vec::new();

        for cred in &credentials {
            // 已禁用的凭据不参与调度，轮换它的代理没有意义，也不该产生告警
            if cred.disabled {
                continue;
            }
            let update = |target| ProxyRotationUpdate {
                id: cred.id,
                target,
                generation: cred.generation,
                global_generation: cred.global_generation,
            };

            let current_url = match &cred.in_use_proxy_url {
                Some(url) => url.clone(),
                None => {
                    if cred.temporary_direct
                        && cred.recovery_proxy.as_ref().is_some_and(|proxy| {
                            self.proxy_pool.availability_for_proxy(proxy)
                                == ProxyAvailability::Available
                        })
                    {
                        updates.push(update(None));
                        result.restored_to_proxy += 1;
                        continue;
                    }
                    // 当前直连。仅回收「自动回退直连」的凭据，用户显式配置的直连不动
                    if cred.explicit_direct && cred.auto_assigned && !available.is_empty() {
                        let target = available[cursor % available.len()].clone();
                        cursor += 1;
                        tracing::info!(
                            "凭据 #{} 此前因无可用代理回退直连，准备改回代理: {}",
                            cred.id,
                            mask_proxy_url(&target)
                        );
                        updates.push(update(Some(target)));
                        result.restored_to_proxy += 1;
                    }
                    continue;
                }
            };

            // 未登记的代理没有探测记录，不做可用性推断
            if cred.in_use_proxy.as_ref().is_none_or(|proxy| {
                self.proxy_pool.availability_for_proxy(proxy) != ProxyAvailability::Unavailable
            }) {
                continue;
            }

            // 从 cursor 起找第一个不等于当前地址的可用代理；命中后把 cursor 推到它之后，
            // 让后续凭据继续往下轮转，避免都落到同一个代理上
            let target = (0..available.len()).find_map(|offset| {
                let index = (cursor + offset) % available.len();
                (available[index] != current_url).then(|| (index, available[index].clone()))
            });

            match target {
                Some((index, url)) => {
                    cursor = index + 1;
                    tracing::warn!(
                        "凭据 #{} 在用代理不可用，准备轮换: {} -> {}",
                        cred.id,
                        mask_proxy_url(&current_url),
                        mask_proxy_url(&url)
                    );
                    updates.push(update(Some(url)));
                    result.rotated += 1;
                }
                None if direct_fallback_enabled => {
                    tracing::warn!(
                        "凭据 #{} 在用代理 {} 不可用且无其它可用代理，准备按配置回退直连",
                        cred.id,
                        mask_proxy_url(&current_url)
                    );
                    updates.push(update(Some(KiroCredentials::PROXY_DIRECT.to_string())));
                    result.switched_to_direct += 1;
                }
                None => {
                    unresolved_ids.push(cred.id);
                    result.unresolved += 1;
                }
            }
        }

        if !unresolved_ids.is_empty() {
            // 只列前若干个 id，避免单行日志过长
            const MAX_LISTED: usize = 10;
            let listed: Vec<String> = unresolved_ids
                .iter()
                .take(MAX_LISTED)
                .map(u64::to_string)
                .collect();
            let suffix = if unresolved_ids.len() > MAX_LISTED {
                format!(" 等 {} 个", unresolved_ids.len())
            } else {
                String::new()
            };
            tracing::error!(
                "{} 个启用中的凭据在用代理不可用且无其它可用代理；直连回退未开启，保持原配置（凭据 #{}{}）",
                unresolved_ids.len(),
                listed.join(", #"),
                suffix
            );
        }

        if !updates.is_empty()
            && let Err(e) = self.token_manager.apply_proxy_rotation(&updates)
        {
            tracing::error!("代理自动轮换结果持久化失败: {}", e);
            return ProxyRotationResponse {
                unresolved: result.unresolved + updates.len(),
                ..ProxyRotationResponse::default()
            };
        }

        // Rebind rotated endpoints to IP-management entries so a later restart
        // or proxy edit cannot restore the stale pre-rotation association.
        if !updates.is_empty() {
            self.migrate_credential_proxies();
        }

        result
    }

    /// 读取代理故障转移配置
    pub fn get_proxy_failover_config(&self) -> ProxyFailoverConfigResponse {
        let (auto_rotate_enabled, direct_fallback_enabled) =
            self.token_manager.get_proxy_failover_config();
        ProxyFailoverConfigResponse {
            auto_rotate_enabled,
            direct_fallback_enabled,
        }
    }

    /// 更新代理故障转移配置
    pub fn set_proxy_failover_config(
        &self,
        req: SetProxyFailoverConfigRequest,
    ) -> Result<ProxyFailoverConfigResponse, AdminServiceError> {
        if req.auto_rotate_enabled.is_none() && req.direct_fallback_enabled.is_none() {
            return Err(AdminServiceError::InvalidCredential(
                "至少提供 autoRotateEnabled / directFallbackEnabled 一个字段".to_string(),
            ));
        }

        self.token_manager
            .set_proxy_failover_config(req.auto_rotate_enabled, req.direct_fallback_enabled)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(self.get_proxy_failover_config())
    }

    /// 按当前代理池状态立即执行一次凭据代理轮换（不重新探测）
    pub fn rotate_credential_proxies(&self) -> ProxyRotationResponse {
        self.rotate_unavailable_credential_proxies()
    }
}
