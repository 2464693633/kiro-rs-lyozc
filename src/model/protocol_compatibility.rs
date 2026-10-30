//! Shared protocol settings. Readers see the old snapshot until persistence
//! completes; a failed save never changes the active request behavior.

use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use super::config::ProtocolCompatibilityConfig;

pub const MODULE_VERSION: &str = "3";

#[derive(Debug, Clone)]
pub struct ProtocolCompatibilitySnapshot {
    pub config: ProtocolCompatibilityConfig,
    pub updated_at: Option<String>,
}

pub type SharedProtocolCompatibility = Arc<ProtocolCompatibilityRuntime>;

#[derive(Debug)]
pub struct ProtocolCompatibilityRuntime {
    snapshot: RwLock<ProtocolCompatibilitySnapshot>,
    update_lock: Mutex<()>,
    pub(crate) native_search_contexts: Arc<crate::anthropic::search_context::NativeContextCache>,
}

impl Default for ProtocolCompatibilityRuntime {
    fn default() -> Self {
        Self::new(ProtocolCompatibilityConfig::default())
    }
}

impl ProtocolCompatibilityRuntime {
    pub fn new(mut config: ProtocolCompatibilityConfig) -> Self {
        config.normalize();
        Self {
            snapshot: RwLock::new(ProtocolCompatibilitySnapshot {
                config,
                updated_at: None,
            }),
            update_lock: Mutex::new(()),
            native_search_contexts: Arc::new(Default::default()),
        }
    }

    pub fn snapshot(&self) -> ProtocolCompatibilitySnapshot {
        self.snapshot.read().clone()
    }

    pub fn for_key(&self, key_id: u64) -> ProtocolCompatibilityConfig {
        self.snapshot.read().config.for_key(key_id)
    }

    pub fn update_with_persistence(
        &self,
        mut config: ProtocolCompatibilityConfig,
        persist: impl FnOnce(&ProtocolCompatibilityConfig) -> anyhow::Result<()>,
    ) -> anyhow::Result<ProtocolCompatibilitySnapshot> {
        let _guard = self.update_lock.lock();
        config.normalize();
        persist(&config)?;
        let next = ProtocolCompatibilitySnapshot {
            config,
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        *self.snapshot.write() = next.clone();
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::CodeExecutionMode;

    #[test]
    fn failed_persistence_keeps_the_active_snapshot_and_scope() {
        let runtime = ProtocolCompatibilityRuntime::default();
        let candidate = ProtocolCompatibilityConfig {
            code_execution_mode: CodeExecutionMode::Bridge,
            search_incremental_streaming: true,
            signature_metadata_compat: true,
            structured_identity_compat: true,
            search_context_compat: true,
            enabled_key_ids: vec![7],
        };
        assert!(
            runtime
                .update_with_persistence(candidate, |_| anyhow::bail!("fixture save failure"))
                .is_err()
        );
        assert_eq!(runtime.for_key(7), ProtocolCompatibilityConfig::default());
        assert!(runtime.snapshot().updated_at.is_none());
    }

    #[test]
    fn readers_share_the_new_settings_only_after_persistence_succeeds() {
        let runtime = Arc::new(ProtocolCompatibilityRuntime::default());
        let reader = runtime.clone();
        let candidate = ProtocolCompatibilityConfig {
            code_execution_mode: CodeExecutionMode::Bridge,
            search_incremental_streaming: true,
            signature_metadata_compat: true,
            structured_identity_compat: true,
            search_context_compat: true,
            enabled_key_ids: vec![9, 7, 9],
        };
        let saved = runtime
            .update_with_persistence(candidate, |persisted| {
                assert_eq!(persisted.enabled_key_ids, vec![7, 9]);
                assert_eq!(
                    reader.for_key(7).code_execution_mode,
                    CodeExecutionMode::Reject
                );
                Ok(())
            })
            .unwrap();
        assert!(saved.updated_at.is_some());
        assert_eq!(
            reader.for_key(7).code_execution_mode,
            CodeExecutionMode::Bridge
        );
        assert!(reader.for_key(9).search_incremental_streaming);
        assert!(reader.for_key(9).signature_metadata_compat);
        assert!(reader.for_key(9).structured_identity_compat);
        assert!(reader.for_key(9).search_context_compat);
        assert_eq!(reader.for_key(8), ProtocolCompatibilityConfig::default());
        runtime
            .update_with_persistence(
                ProtocolCompatibilityConfig {
                    enabled_key_ids: Vec::new(),
                    ..saved.config
                },
                |_| Ok(()),
            )
            .unwrap();
        assert!(reader.for_key(8).search_incremental_streaming);
        assert!(!reader.for_key(8).signature_metadata_compat);
        assert!(!reader.for_key(8).structured_identity_compat);
        assert!(!reader.for_key(8).search_context_compat);
    }
}
