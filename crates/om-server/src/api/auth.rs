//! REST & WebSocket authentication helpers (Phase 0 static token + W2-1 RBAC device tokens).
//!
//! Resolves every request into a real DB-backed [`om_core::auth::Principal`]:
//! 1. Device tokens (`omt_...`) are verified through [`crate::auth::Auth::authenticate`]
//!    against hashed rows in SQLite (revocation, expiration, disabled member, and role
//!    changes take effect immediately).
//! 2. When a static bootstrap token (`OM_TOKEN`) is configured and matches in constant
//!    time (SHA-256 + `subtle::ConstantTimeEq`), the request resolves to the default
//!    space's active `Owner` record (auto-bootstrapping one if the space has no members
//!    yet), preserving full backward compatibility with Phase 0/1 tooling while still
//!    running all RBAC/audit transactions against real database members.
//!
//! Never logs tokens.

use crate::auth::Auth;
use crate::db::Db;
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use om_core::auth::{Principal, Role, TOKEN_PREFIX};
use om_core::ids::{SpaceId, TokenId};
use om_core::{CoreError, CoreResult};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Constant-time equality check for secret token strings.
///
/// Compares fixed-length SHA-256 digests via [`subtle::ConstantTimeEq`] so neither byte
/// differences nor length differences leak timing information.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    bool::from(ha.as_slice().ct_eq(hb.as_slice()))
}

/// Extracts `<token>` from `Authorization: Bearer <token>` (case-insensitive scheme).
pub fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = raw.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    None
}

/// Validates a static REST Bearer token (used by unit tests and static fallback checks).
pub fn verify_rest_auth(headers: &HeaderMap, expected_token: &str) -> bool {
    if expected_token.is_empty() {
        return false;
    }
    extract_bearer_token(headers)
        .map(|candidate| constant_time_eq(candidate, expected_token))
        .unwrap_or(false)
}

/// Validates a static WebSocket token via `Authorization: Bearer <token>` OR `?token=<token>`.
pub fn verify_ws_auth(
    headers: &HeaderMap,
    query_token: Option<&str>,
    expected_token: &str,
) -> bool {
    if expected_token.is_empty() {
        return false;
    }
    if let Some(candidate) = extract_bearer_token(headers) {
        return constant_time_eq(candidate, expected_token);
    }
    query_token
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|candidate| constant_time_eq(candidate, expected_token))
        .unwrap_or(false)
}

/// Extracts the raw access token from WebSocket upgrade headers or `?token=` query param.
pub fn extract_ws_token<'a>(
    headers: &'a HeaderMap,
    query_token: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(tok) = extract_bearer_token(headers) {
        return Some(tok);
    }
    query_token.map(str::trim).filter(|s| !s.is_empty())
}

/// Resolves the active bootstrap `Owner` member in `space_id` for static-token requests,
/// lazily creating the initial `Owner` if the space has no members yet.
async fn resolve_static_owner_principal(
    auth: &Auth,
    db: &Db,
    space_id: &SpaceId,
) -> CoreResult<Principal> {
    let members = db.list_members(space_id).await?;
    if let Some(owner) = members
        .into_iter()
        .find(|m| m.is_active() && m.role == Role::Owner)
    {
        return Ok(Principal {
            member_id: owner.id,
            space_id: owner.space_id,
            display_name: owner.display_name,
            role: owner.role,
            token_id: TokenId::from("static-bootstrap"),
        });
    }

    if let Some((owner, issued)) = auth
        .bootstrap_owner(space_id, "Owner", "static-bootstrap")
        .await?
    {
        return Ok(Principal {
            member_id: owner.id,
            space_id: owner.space_id,
            display_name: owner.display_name,
            role: owner.role,
            token_id: issued.token.id,
        });
    }

    // Space has members, none of which is an active owner (defensive fallback).
    Err(CoreError::Unauthenticated(
        "no active owner available for static bootstrap token".into(),
    ))
}

/// Authenticates any presented secret (either an `omt_...` RBAC device token or the
/// optional static `OM_TOKEN` bootstrap secret) into a DB-backed [`Principal`].
pub async fn authenticate_secret(
    auth: &Auth,
    db: &Db,
    space_id: &SpaceId,
    static_token: &str,
    candidate: &str,
) -> CoreResult<Principal> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Unauthenticated(
            "invalid or missing access token".into(),
        ));
    }

    if trimmed.starts_with(TOKEN_PREFIX) {
        match auth.authenticate(trimmed).await {
            Ok(p) => return Ok(p),
            Err(err) => {
                if !static_token.is_empty() && constant_time_eq(trimmed, static_token) {
                    return resolve_static_owner_principal(auth, db, space_id).await;
                }
                return Err(err);
            }
        }
    }

    if !static_token.is_empty() && constant_time_eq(trimmed, static_token) {
        return resolve_static_owner_principal(auth, db, space_id).await;
    }

    Err(CoreError::Unauthenticated(
        "invalid, expired or revoked token".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn bearer_and_query_auth_validate_correctly() {
        let mut headers = HeaderMap::new();
        assert!(!verify_rest_auth(&headers, "secret-123"));

        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret-123"));
        assert!(verify_rest_auth(&headers, "secret-123"));
        assert!(!verify_rest_auth(&headers, "wrong"));
        assert!(!verify_rest_auth(&headers, ""));

        let empty = HeaderMap::new();
        assert!(verify_ws_auth(&empty, Some("secret-123"), "secret-123"));
        assert!(!verify_ws_auth(&empty, Some("nope"), "secret-123"));
    }
}
