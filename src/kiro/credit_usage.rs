//! Accumulate credits for one physical upstream response, including partial reads.

use std::sync::Arc;

use super::{model::events::MeteringEvent, token_manager::MultiTokenManager};

pub(crate) struct CreditUsageGuard {
    manager: Option<Arc<MultiTokenManager>>,
    credential_id: u64,
    credits: f64,
}

impl CreditUsageGuard {
    pub fn new(manager: Option<Arc<MultiTokenManager>>, credential_id: u64) -> Self {
        Self { manager, credential_id, credits: 0.0 }
    }

    pub fn observe(&mut self, event: &MeteringEvent) {
        if let Some(amount) = event.credits() {
            self.set_usage(Some(self.credits + amount));
        }
    }

    /// Streaming contexts provide cumulative snapshots; replace instead of adding.
    pub fn set_usage(&mut self, credits: Option<f64>) {
        if let Some(credits) = credits.filter(|amount| amount.is_finite() && *amount >= 0.0) {
            self.credits = credits;
        }
    }
}

impl Drop for CreditUsageGuard {
    fn drop(&mut self) {
        if let Some(manager) = &self.manager {
            manager.record_credit_usage(self.credential_id, self.credits);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{kiro::model::credentials::KiroCredentials, model::config::Config};

    fn manager() -> Arc<MultiTokenManager> {
        Arc::new(MultiTokenManager::new(Config::default(),
            vec![KiroCredentials::default(), KiroCredentials::default()], None, None, false).unwrap())
    }

    fn event(usage: f64) -> MeteringEvent {
        MeteringEvent { unit: "credit".into(), unit_plural: "credits".into(), usage }
    }

    #[test]
    fn credit_usage_snapshots_settle_once_and_stay_with_the_actual_credential() {
        let manager = manager();
        {
            let mut first = CreditUsageGuard::new(Some(manager.clone()), 1);
            first.set_usage(Some(0.25));
            first.set_usage(Some(0.50));
            first.set_usage(Some(0.50));
            let mut second = CreditUsageGuard::new(Some(manager.clone()), 2);
            second.observe(&event(0.125));
            second.observe(&event(0.25));
            // Dropping these guards also covers cancellation after metering arrives.
        }
        let rows = manager.snapshot().entries;
        assert_eq!(rows[0].total_credits, 0.50);
        assert_eq!(rows[1].total_credits, 0.375);
        // A synthetic outer aggregate has no physical response guard and adds nothing.
        assert_eq!(rows.iter().map(|row| row.total_credits).sum::<f64>(), 0.875);
    }

    #[test]
    fn credit_usage_ignores_missing_invalid_and_non_credit_metering() {
        let manager = manager();
        {
            let _missing = CreditUsageGuard::new(Some(manager.clone()), 1);
            let mut guard = CreditUsageGuard::new(Some(manager.clone()), 1);
            guard.observe(&event(0.125));
            for value in [-1.0, f64::NAN, f64::INFINITY] {
                guard.observe(&event(value));
                guard.set_usage(Some(value));
            }
            guard.observe(&MeteringEvent { unit: "token".into(), usage: 999.0, ..event(0.0) });
            guard.set_usage(None);
        }
        assert_eq!(manager.snapshot().entries[0].total_credits, 0.125);
    }
}
