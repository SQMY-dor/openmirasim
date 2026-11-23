//! PWA Web Push subscription store and validation (W2-4).
//!
//! Stores per-member Web Push subscriptions (optionally persisted to JSON under
//! `OM_DATA_DIR/push-subscriptions.json`) with strict endpoint SSRF validation.

use om_core::ids::MemberId;
use om_core::{CoreError, CoreResult, now_ms};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const MAX_SUBSCRIPTIONS_PER_MEMBER: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterPushSubscriptionRequest {
    pub endpoint: String,
    pub keys: PushKeys,
    #[serde(default)]
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSubscriptionRecord {
    pub id: String,
    pub member_id: MemberId,
    pub endpoint: String,
    pub keys: PushKeys,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushConfigResponse {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vapid_public_key: Option<String>,
    pub subscription_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPushSubscriptionsResponse {
    pub subscriptions: Vec<PushSubscriptionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushTestResponse {
    pub delivered: usize,
    pub mode: String,
    pub message: String,
}

/// Validates a Web Push endpoint URL to prevent SSRF against internal/link-local addresses.
pub fn validate_push_endpoint(raw: &str, allow_loopback_http: bool) -> CoreResult<String> {
    let trimmed = raw.trim();
    let url = reqwest::Url::parse(trimmed)
        .map_err(|e| CoreError::InvalidInput(format!("invalid push endpoint URL: {e}")))?;

    let scheme = url.scheme();
    let Some(host) = url.host_str() else {
        return Err(CoreError::InvalidInput(
            "push endpoint URL must include a host".into(),
        ));
    };

    if !url.username().is_empty() || url.password().is_some() {
        return Err(CoreError::InvalidInput(
            "push endpoint URL must not include embedded credentials".into(),
        ));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        let is_loopback = ip.is_loopback();
        if is_loopback {
            if !allow_loopback_http {
                return Err(CoreError::InvalidInput(
                    "loopback push endpoints are disabled".into(),
                ));
            }
            if scheme != "http" && scheme != "https" {
                return Err(CoreError::InvalidInput(
                    "push endpoint must use https (or loopback http in dev/test)".into(),
                ));
            }
            return Ok(trimmed.to_string());
        }
        // Reject private, link-local (including cloud metadata 169.254.169.254), unspecified, multicast.
        let disallowed = match ip {
            IpAddr::V4(v4) => {
                v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_broadcast()
                    || v4.is_multicast()
            }
            IpAddr::V6(v6) => v6.is_unspecified() || v6.is_multicast() || v6.is_unique_local(),
        };
        if disallowed {
            return Err(CoreError::InvalidInput(
                "push endpoint IP address is not a public destination".into(),
            ));
        }
    } else if host.eq_ignore_ascii_case("localhost") {
        if !allow_loopback_http {
            return Err(CoreError::InvalidInput(
                "localhost push endpoints are disabled".into(),
            ));
        }
        if scheme != "http" && scheme != "https" {
            return Err(CoreError::InvalidInput(
                "push endpoint must use https".into(),
            ));
        }
        return Ok(trimmed.to_string());
    }

    if scheme != "https" {
        return Err(CoreError::InvalidInput(
            "push endpoint URL must use https".into(),
        ));
    }
    Ok(trimmed.to_string())
}

#[derive(Clone)]
pub struct PushStore {
    inner: Arc<Mutex<Vec<PushSubscriptionRecord>>>,
    file_path: Option<PathBuf>,
    vapid_public_key: Option<String>,
    allow_loopback_http: bool,
}

impl PushStore {
    pub fn new(
        file_path: Option<PathBuf>,
        vapid_public_key: Option<String>,
        allow_loopback_http: bool,
    ) -> Self {
        let initial = file_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<Vec<PushSubscriptionRecord>>(&bytes).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(Mutex::new(initial)),
            file_path,
            vapid_public_key: vapid_public_key.filter(|k| !k.trim().is_empty()),
            allow_loopback_http,
        }
    }

    fn persist_locked(&self, records: &[PushSubscriptionRecord]) {
        let Some(path) = &self.file_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(records) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        }
    }

    pub fn config_for(&self, member_id: &MemberId) -> PushConfigResponse {
        let count = self
            .inner
            .lock()
            .map(|list| list.iter().filter(|r| &r.member_id == member_id).count())
            .unwrap_or(0);
        PushConfigResponse {
            enabled: true,
            vapid_public_key: self.vapid_public_key.clone(),
            subscription_count: count,
        }
    }

    pub fn list_for(&self, member_id: &MemberId) -> Vec<PushSubscriptionRecord> {
        self.inner
            .lock()
            .map(|list| {
                list.iter()
                    .filter(|r| &r.member_id == member_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn register(
        &self,
        member_id: &MemberId,
        req: RegisterPushSubscriptionRequest,
    ) -> CoreResult<PushSubscriptionRecord> {
        let endpoint = validate_push_endpoint(&req.endpoint, self.allow_loopback_http)?;
        if req.keys.p256dh.trim().is_empty() || req.keys.auth.trim().is_empty() {
            return Err(CoreError::InvalidInput(
                "push subscription keys (p256dh, auth) are required".into(),
            ));
        }

        let mut list = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Upsert by (member_id, endpoint)
        if let Some(existing) = list
            .iter_mut()
            .find(|r| &r.member_id == member_id && r.endpoint == endpoint)
        {
            existing.keys = req.keys;
            existing.user_agent = req.user_agent;
            let cloned = existing.clone();
            self.persist_locked(&list);
            return Ok(cloned);
        }

        let current_count = list.iter().filter(|r| &r.member_id == member_id).count();
        if current_count >= MAX_SUBSCRIPTIONS_PER_MEMBER {
            return Err(CoreError::LimitExceeded(format!(
                "member already has {MAX_SUBSCRIPTIONS_PER_MEMBER} push subscriptions"
            )));
        }

        let record = PushSubscriptionRecord {
            id: format!("push-{}", uuid::Uuid::now_v7()),
            member_id: member_id.clone(),
            endpoint,
            keys: req.keys,
            user_agent: req.user_agent,
            created_at: now_ms(),
        };
        list.push(record.clone());
        self.persist_locked(&list);
        Ok(record)
    }

    pub fn delete(&self, member_id: &MemberId, sub_id: &str) -> CoreResult<()> {
        let mut list = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = list.len();
        list.retain(|r| !(&r.member_id == member_id && r.id == sub_id));
        if list.len() == before {
            return Err(CoreError::NotFound(format!("push subscription {sub_id}")));
        }
        self.persist_locked(&list);
        Ok(())
    }
}
