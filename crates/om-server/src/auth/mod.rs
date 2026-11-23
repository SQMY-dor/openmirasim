//! Team RBAC and device tokens (W2-1).
//!
//! [`Auth`] is the only door the API layer needs:
//!
//! - [`Auth::authenticate`] turns a bearer secret into a [`Principal`], or a generic
//!   `Unauthenticated` error that never says *why* (unknown, revoked, expired or
//!   disabled all look the same to the caller).
//! - [`Auth::require`] checks a [`Principal`] against an [`Action`] with [`Role::can`].
//! - The member and token operations check who may act on whom, then call the
//!   database, which enforces the invariants (member cap, last owner, hashed secrets)
//!   inside one transaction and writes the audit entry with the change.
//!
//! Members of other spaces are reported as `NotFound`, never `Forbidden`, so a principal
//! cannot probe ids outside its space.
//!
//! Secrets are `omt_` + 64 hex chars (256 bits from the OS RNG). Only their SHA-256 is
//! stored; with that much entropy a plain hash is enough and lookup stays an indexed
//! equality match instead of a per-token comparison.

#[cfg(test)]
mod tests;

use crate::db::{AuditEntry, Db};
use om_core::auth::{
    Action, DeviceTokenRecord, IssuedToken, MemberRecord, Principal, Role, TOKEN_PREFIX,
};
use om_core::ids::{MemberId, SpaceId, TokenId};
use om_core::{CoreError, CoreResult, now_ms};
use sha2::{Digest, Sha256};
use std::time::Duration;

const SECRET_BYTES: usize = 32;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// A new random token secret: `omt_` followed by 64 lowercase hex characters.
pub fn generate_secret() -> CoreResult<String> {
    let mut buf = [0u8; SECRET_BYTES];
    getrandom::fill(&mut buf)
        .map_err(|e| CoreError::Storage(format!("OS random source failed: {e}")))?;
    Ok(format!("{TOKEN_PREFIX}{}", hex(&buf)))
}

/// The value stored for a secret: lowercase hex SHA-256.
pub fn hash_secret(secret: &str) -> String {
    hex(&Sha256::digest(secret.as_bytes()))
}

/// Cheap shape check so obviously wrong input never reaches the database.
fn well_formed(secret: &str) -> bool {
    secret.strip_prefix(TOKEN_PREFIX).is_some_and(|rest| {
        rest.len() == SECRET_BYTES * 2 && rest.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

fn unauthenticated() -> CoreError {
    CoreError::Unauthenticated("invalid, expired or revoked token".into())
}

fn denied(p: &Principal, what: impl std::fmt::Display) -> CoreError {
    CoreError::Forbidden(format!("role {:?} may not {what}", p.role))
}

/// Membership, roles and tokens for one server.
#[derive(Clone)]
pub struct Auth {
    db: Db,
}

impl Auth {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// On a space with no members yet, creates the first `Owner` and returns their token
    /// secret (to print once at first start). Returns `None` on every later call.
    pub async fn bootstrap_owner(
        &self,
        space_id: &SpaceId,
        display_name: &str,
        label: &str,
    ) -> CoreResult<Option<(MemberRecord, IssuedToken)>> {
        let secret = generate_secret()?;
        Ok(self
            .db
            .bootstrap_owner(space_id, display_name, hash_secret(&secret), label)
            .await?
            .map(|(member, token)| (member, IssuedToken { token, secret })))
    }

    /// Resolves a bearer secret to its principal.
    pub async fn authenticate(&self, secret: &str) -> CoreResult<Principal> {
        let secret = secret.trim();
        if !well_formed(secret) {
            return Err(unauthenticated());
        }
        let now = now_ms();
        let (principal, token) = self
            .db
            .principal_for_token_hash(hash_secret(secret), now)
            .await?
            .ok_or_else(unauthenticated)?;
        if token
            .last_used_at
            .is_none_or(|t| now - t >= crate::db::TOKEN_TOUCH_INTERVAL_MS)
        {
            self.db.touch_token(&token.id, now).await?;
        }
        Ok(principal)
    }

    /// `Forbidden` unless the principal's role allows `action`.
    pub fn require(&self, p: &Principal, action: Action) -> CoreResult<()> {
        if p.can(action) {
            Ok(())
        } else {
            Err(denied(p, format!("{action:?}")))
        }
    }

    /// Loads a member of the principal's own space; other spaces look like `NotFound`.
    async fn member_in_space(&self, p: &Principal, id: &MemberId) -> CoreResult<MemberRecord> {
        let member = self.db.get_member(id).await?;
        if member.space_id != p.space_id {
            return Err(CoreError::NotFound(format!("member {id}")));
        }
        Ok(member)
    }

    /// Ensures `p` may act on `target`: always for themselves, otherwise only when their
    /// role manages the target's role.
    fn ensure_manages(&self, p: &Principal, target: &MemberRecord, what: &str) -> CoreResult<()> {
        if target.id == p.member_id || p.role.can_manage(target.role) {
            Ok(())
        } else {
            Err(denied(p, format!("{what} a {:?}", target.role)))
        }
    }

    pub async fn list_members(&self, p: &Principal) -> CoreResult<Vec<MemberRecord>> {
        self.require(p, Action::ViewSessions)?;
        self.db.list_members(&p.space_id).await
    }

    pub async fn add_member(
        &self,
        p: &Principal,
        display_name: &str,
        role: Role,
    ) -> CoreResult<MemberRecord> {
        self.require(p, Action::ManageMembers)?;
        if !p.role.can_assign(role) {
            return Err(denied(p, format!("add a {role:?}")));
        }
        self.db
            .create_member(&p.space_id, display_name, role, Some(p.member_id.clone()))
            .await
    }

    /// Changes a member's role. The actor must manage both the member's current role and
    /// the new one, so an admin can neither touch another admin nor promote to admin.
    /// Owners may change their own role as long as another active owner remains.
    pub async fn change_role(
        &self,
        p: &Principal,
        member_id: &MemberId,
        role: Role,
    ) -> CoreResult<MemberRecord> {
        let target = self.member_in_space(p, member_id).await?;
        let self_owner = target.id == p.member_id && p.role == Role::Owner;
        if !self_owner && !(p.role.can_manage(target.role) && p.role.can_assign(role)) {
            return Err(denied(p, format!("change a {:?} to {role:?}", target.role)));
        }
        self.db
            .set_member_role(member_id, role, Some(p.member_id.clone()))
            .await
    }

    /// Disables a member and revokes their tokens. Owners may disable themselves while
    /// another active owner remains; nobody else may disable themselves.
    pub async fn disable_member(
        &self,
        p: &Principal,
        member_id: &MemberId,
    ) -> CoreResult<MemberRecord> {
        let target = self.member_in_space(p, member_id).await?;
        let self_owner = target.id == p.member_id && p.role == Role::Owner;
        if !self_owner && !p.role.can_manage(target.role) {
            return Err(denied(p, format!("disable a {:?}", target.role)));
        }
        self.db
            .disable_member(member_id, Some(p.member_id.clone()))
            .await
    }

    /// Issues a token for `member_id` (themselves, or a member the actor manages).
    /// `ttl` of `None` means the token lives until revoked.
    pub async fn issue_token(
        &self,
        p: &Principal,
        member_id: &MemberId,
        label: &str,
        ttl: Option<Duration>,
    ) -> CoreResult<IssuedToken> {
        let target = self.member_in_space(p, member_id).await?;
        self.ensure_manages(p, &target, "issue a token for")?;
        let expires_at = match ttl {
            Some(ttl) if ttl.is_zero() => {
                return Err(CoreError::InvalidInput("token ttl must be positive".into()));
            }
            Some(ttl) => {
                Some(now_ms().saturating_add(ttl.as_millis().min(i64::MAX as u128) as i64))
            }
            None => None,
        };
        let secret = generate_secret()?;
        let token = self
            .db
            .create_token(
                member_id,
                hash_secret(&secret),
                label,
                expires_at,
                Some(p.member_id.clone()),
            )
            .await?;
        Ok(IssuedToken { token, secret })
    }

    pub async fn list_tokens(
        &self,
        p: &Principal,
        member_id: &MemberId,
    ) -> CoreResult<Vec<DeviceTokenRecord>> {
        let target = self.member_in_space(p, member_id).await?;
        self.ensure_manages(p, &target, "list tokens of")?;
        self.db.list_tokens(member_id).await
    }

    /// Revokes one of the actor's own tokens, or a token of a member they manage.
    pub async fn revoke_token(
        &self,
        p: &Principal,
        token_id: &TokenId,
    ) -> CoreResult<DeviceTokenRecord> {
        let token = self.db.get_token(token_id).await?;
        let owner = self
            .member_in_space(p, &token.member_id)
            .await
            .map_err(|_| CoreError::NotFound(format!("token {token_id}")))?;
        self.ensure_manages(p, &owner, "revoke a token of")?;
        self.db
            .revoke_token(token_id, Some(p.member_id.clone()))
            .await
    }

    /// Newest-first audit entries (admins and owners only).
    pub async fn audit_log(
        &self,
        p: &Principal,
        target: Option<String>,
        limit: u32,
    ) -> CoreResult<Vec<AuditEntry>> {
        self.require(p, Action::ViewAudit)?;
        self.db
            .list_audit(&p.space_id, target, limit.clamp(1, 1000))
            .await
    }
}
