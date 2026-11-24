//! PWA Web Push subscription store and validation (W2-4).
//!
//! Stores per-member Web Push subscriptions (optionally persisted to JSON under
//! `OM_DATA_DIR/push-subscriptions.json`) with strict endpoint SSRF validation.

use om_core::ids::MemberId;
use om_core::{CoreError, CoreResult, now_ms};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const MAX_SUBSCRIPTIONS_PER_MEMBER: usize = 10;
const MAX_URL_LEN: usize = 2048;
const MAX_P256DH_LEN: usize = 256;
const MAX_AUTH_LEN: usize = 128;
const MAX_USER_AGENT_LEN: usize = 256;

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
    #[serde(default)]
    pub registered: usize,
    pub mode: String,
    pub message: String,
}

fn is_disallowed_ipv4(v4: Ipv4Addr) -> bool {
    let oct = v4.octets();
    let is_zero_net = oct[0] == 0;
    let is_cgnat = oct[0] == 100 && (oct[1] & 0xc0) == 64; // 100.64.0.0/10
    let is_benchmark = oct[0] == 198 && (oct[1] & 0xfe) == 18; // 198.18.0.0/15
    let is_reserved = oct[0] >= 240; // 240.0.0.0/4 + 255.255.255.255
    v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || is_zero_net
        || is_cgnat
        || is_benchmark
        || is_reserved
}

fn is_disallowed_ipv6(v6: Ipv6Addr) -> bool {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return mapped.is_loopback() || is_disallowed_ipv4(mapped);
    }
    #[allow(deprecated)]
    if let Some(compat) = v6.to_ipv4() {
        return compat.is_loopback() || is_disallowed_ipv4(compat);
    }
    let seg0 = v6.segments()[0];
    let is_unique_local = (seg0 & 0xfe00) == 0xfc00; // fc00::/7
    let is_link_local = (seg0 & 0xffc0) == 0xfe80; // fe80::/10
    let is_site_local = (seg0 & 0xffc0) == 0xfec0; // fec0::/10 (deprecated)
    let is_doc = seg0 == 0x2001 && v6.segments()[1] == 0x0db8; // 2001:db8::/32
    v6.is_unspecified()
        || v6.is_multicast()
        || is_unique_local
        || is_link_local
        || is_site_local
        || is_doc
}

/// Validates an external HTTP(S) URL against SSRF to loopback, link-local, metadata,
/// private, CGNAT, or IPv6-mapped internal destinations.
pub(crate) fn validate_external_url(
    raw: &str,
    label: &str,
    require_https: bool,
    allow_loopback_http: bool,
) -> CoreResult<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_URL_LEN {
        return Err(CoreError::InvalidInput(format!(
            "{label} must be 1..={MAX_URL_LEN} characters"
        )));
    }
    let url = reqwest::Url::parse(trimmed)
        .map_err(|e| CoreError::InvalidInput(format!("invalid {label}: {e}")))?;

    let scheme = url.scheme();
    let Some(host) = url.host_str() else {
        return Err(CoreError::InvalidInput(format!(
            "{label} must include a host"
        )));
    };

    if !url.username().is_empty() || url.password().is_some() {
        return Err(CoreError::InvalidInput(format!(
            "{label} must not include embedded credentials"
        )));
    }

    let unbracketed = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    // Strip any IPv6 zone index (`%25eth0` / `%eth0`) before parsing.
    let ip_candidate = unbracketed.split('%').next().unwrap_or(unbracketed);

    if let Ok(ip) = ip_candidate.parse::<IpAddr>() {
        let is_loopback = match ip {
            IpAddr::V4(v4) => v4.is_loopback(),
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|m| m.is_loopback())
            }
        };
        if is_loopback {
            if !allow_loopback_http {
                return Err(CoreError::InvalidInput(format!(
                    "loopback {label} destinations are disabled"
                )));
            }
            if scheme != "http" && scheme != "https" {
                return Err(CoreError::InvalidInput(format!(
                    "{label} must use http or https"
                )));
            }
            return Ok(trimmed.to_string());
        }
        let disallowed = match ip {
            IpAddr::V4(v4) => is_disallowed_ipv4(v4),
            IpAddr::V6(v6) => is_disallowed_ipv6(v6),
        };
        if disallowed {
            return Err(CoreError::InvalidInput(format!(
                "{label} IP address is not a public destination"
            )));
        }
    } else if host.starts_with('[') || host.ends_with(']') {
        return Err(CoreError::InvalidInput(format!(
            "invalid IPv6 host in {label}"
        )));
    } else {
        let domain = host.trim_end_matches('.').to_ascii_lowercase();
        if domain == "localhost" || domain.ends_with(".localhost") {
            if !allow_loopback_http {
                return Err(CoreError::InvalidInput(format!(
                    "localhost {label} destinations are disabled"
                )));
            }
            if scheme != "http" && scheme != "https" {
                return Err(CoreError::InvalidInput(format!(
                    "{label} must use http or https"
                )));
            }
            return Ok(trimmed.to_string());
        }
        if domain.ends_with(".internal")
            || domain.ends_with(".local")
            || domain == "metadata"
            || domain == "metadata.google.internal"
        {
            return Err(CoreError::InvalidInput(format!(
                "{label} host `{domain}` is not a public destination"
            )));
        }
    }

    if require_https {
        if scheme != "https" {
            return Err(CoreError::InvalidInput(format!("{label} must use https")));
        }
    } else if scheme != "http" && scheme != "https" {
        return Err(CoreError::InvalidInput(format!(
            "{label} must use http or https"
        )));
    }
    Ok(trimmed.to_string())
}

/// Validates a Web Push endpoint URL to prevent SSRF against internal/link-local addresses.
pub fn validate_push_endpoint(raw: &str, allow_loopback_http: bool) -> CoreResult<String> {
    validate_external_url(raw, "push endpoint URL", true, allow_loopback_http)
}

pub(crate) fn write_atomic_private_json(path: &std::path::Path, bytes: &[u8]) -> CoreResult<()> {
    if let Some(parent) = path.parent() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            if !parent.exists() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)?;
            }
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp_name = format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("store"),
        uuid::Uuid::now_v7().simple()
    );
    let tmp = path.with_file_name(tmp_name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        return Err(CoreError::Io(e));
    }
    drop(file);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CoreError::Io(e));
    }
    Ok(())
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

    fn persist_snapshot(&self, records: &[PushSubscriptionRecord]) -> CoreResult<()> {
        let Some(path) = &self.file_path else {
            return Ok(());
        };
        let bytes = serde_json::to_vec_pretty(records)
            .map_err(|e| CoreError::Storage(format!("serialize push store: {e}")))?;
        write_atomic_private_json(path, &bytes)
    }

    pub fn config_for(&self, member_id: &MemberId) -> PushConfigResponse {
        let count = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|r| &r.member_id == member_id)
            .count();
        PushConfigResponse {
            enabled: true,
            vapid_public_key: self.vapid_public_key.clone(),
            subscription_count: count,
        }
    }

    /// Returns a member's subscriptions with `keys.auth` redacted.
    pub fn list_for(&self, member_id: &MemberId) -> Vec<PushSubscriptionRecord> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|r| &r.member_id == member_id)
            .cloned()
            .map(|mut rec| {
                rec.keys.auth.clear();
                rec
            })
            .collect()
    }

    pub fn register(
        &self,
        member_id: &MemberId,
        req: RegisterPushSubscriptionRequest,
    ) -> CoreResult<PushSubscriptionRecord> {
        let endpoint = validate_push_endpoint(&req.endpoint, self.allow_loopback_http)?;
        let p256dh = req.keys.p256dh.trim();
        let auth = req.keys.auth.trim();
        if p256dh.is_empty()
            || auth.is_empty()
            || p256dh.len() > MAX_P256DH_LEN
            || auth.len() > MAX_AUTH_LEN
        {
            return Err(CoreError::InvalidInput(
                "push subscription keys (p256dh, auth) are required and bounded".into(),
            ));
        }
        let user_agent = req
            .user_agent
            .map(|ua| ua.trim().to_string())
            .filter(|ua| !ua.is_empty());
        if user_agent.as_ref().is_some_and(|ua| ua.len() > MAX_USER_AGENT_LEN) {
            return Err(CoreError::InvalidInput(format!(
                "user_agent exceeds {MAX_USER_AGENT_LEN} bytes"
            )));
        }

        let mut list = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = list.clone();

        // Upsert by (member_id, endpoint)
        if let Some(existing) = next
            .iter_mut()
            .find(|r| &r.member_id == member_id && r.endpoint == endpoint)
        {
            existing.keys = PushKeys {
                p256dh: p256dh.to_string(),
                auth: auth.to_string(),
            };
            existing.user_agent = user_agent;
            let mut returned = existing.clone();
            self.persist_snapshot(&next)?;
            *list = next;
            returned.keys.auth.clear();
            return Ok(returned);
        }

        let current_count = next.iter().filter(|r| &r.member_id == member_id).count();
        if current_count >= MAX_SUBSCRIPTIONS_PER_MEMBER {
            return Err(CoreError::LimitExceeded(format!(
                "member already has {MAX_SUBSCRIPTIONS_PER_MEMBER} push subscriptions"
            )));
        }

        let record = PushSubscriptionRecord {
            id: format!("push-{}", uuid::Uuid::now_v7()),
            member_id: member_id.clone(),
            endpoint,
            keys: PushKeys {
                p256dh: p256dh.to_string(),
                auth: auth.to_string(),
            },
            user_agent,
            created_at: now_ms(),
        };
        next.push(record.clone());
        self.persist_snapshot(&next)?;
        *list = next;
        let mut returned = record;
        returned.keys.auth.clear();
        Ok(returned)
    }

    pub fn delete(&self, member_id: &MemberId, sub_id: &str) -> CoreResult<()> {
        let mut list = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = list.clone();
        let before = next.len();
        next.retain(|r| !(&r.member_id == member_id && r.id == sub_id));
        if next.len() == before {
            return Err(CoreError::NotFound(format!("push subscription {sub_id}")));
        }
        self.persist_snapshot(&next)?;
        *list = next;
        Ok(())
    }
}
