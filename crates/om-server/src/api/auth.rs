//! Phase 0 static Bearer token authentication helpers.
//!
//! Uses constant-time byte comparison to avoid timing side channels. Never logs tokens.

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;

/// Constant-time equality check for secret token strings.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut diff = ab.len() ^ bb.len();
    let max_len = ab.len().max(bb.len());
    for i in 0..max_len {
        let x = ab.get(i).copied().unwrap_or(0);
        let y = bb.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
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

/// Validates a REST request via `Authorization: Bearer <token>`.
pub fn verify_rest_auth(headers: &HeaderMap, expected_token: &str) -> bool {
    if expected_token.is_empty() {
        return false;
    }
    extract_bearer_token(headers)
        .map(|candidate| constant_time_eq(candidate, expected_token))
        .unwrap_or(false)
}

/// Validates a WebSocket upgrade via `Authorization: Bearer <token>` OR `?token=<token>`.
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
