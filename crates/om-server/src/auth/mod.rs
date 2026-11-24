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

use crate::db::{AuditEntry, Authorize, Db};
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

/// Shortest lifetime a token may be issued with.
pub const MIN_TOKEN_TTL: Duration = Duration::from_millis(1);
/// Most live (unrevoked, unexpired) tokens one member may hold.
pub const MAX_LIVE_TOKENS_PER_MEMBER: u32 = 50;

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
        // Checked again inside the write, against the actor's current role & token.
        let auth = Authorize::with_token(p.member_id.clone(), p.token_id.clone(), move |actor, _| {
            if actor.role.can_assign(role) {
                Ok(())
            } else {
                Err(CoreError::Forbidden(format!("add a {role:?}")))
            }
        });
        self.db
            .create_member_checked(
                &p.space_id,
                display_name,
                role,
                Some(p.member_id.clone()),
                Some(auth),
            )
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
        let auth =
            Authorize::with_token(p.member_id.clone(), p.token_id.clone(), move |actor, target| {
                let target = target.expect("role change has a target");
                let self_owner = target.id == actor.id && actor.role == Role::Owner;
                if self_owner
                    || (actor.role.can_manage(target.role) && actor.role.can_assign(role))
                {
                    Ok(())
                } else {
                    Err(CoreError::Forbidden(format!(
                        "change a {:?} to {role:?}",
                        target.role
                    )))
                }
            });
        self.db
            .set_member_role_checked(member_id, role, Some(p.member_id.clone()), Some(auth))
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
        let auth =
            Authorize::with_token(p.member_id.clone(), p.token_id.clone(), move |actor, target| {
                let target = target.expect("disable has a target");
                let self_owner = target.id == actor.id && actor.role == Role::Owner;
                if self_owner || actor.role.can_manage(target.role) {
                    Ok(())
                } else {
                    Err(CoreError::Forbidden(format!("disable a {:?}", target.role)))
                }
            });
        self.db
            .disable_member_checked(member_id, Some(p.member_id.clone()), Some(auth))
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
        let now = now_ms();
        let expires_at = match ttl {
            Some(ttl) if ttl < MIN_TOKEN_TTL => {
                return Err(CoreError::InvalidInput(format!(
                    "token ttl must be at least {MIN_TOKEN_TTL:?}"
                )));
            }
            Some(ttl) => Some(now.saturating_add(ttl.as_millis().min(i64::MAX as u128) as i64)),
            None => None,
        };
        // Caller token usability & expiry ceiling are enforced atomically inside the
        // `create_token_checked` write transaction so a transient read error or concurrent
        // revocation can never mint an unrevoked or permanent token.
        let auth =
            Authorize::with_token(p.member_id.clone(), p.token_id.clone(), |actor, target| {
                let target = target.expect("token has an owner");
                if target.id == actor.id || actor.role.can_manage(target.role) {
                    Ok(())
                } else {
                    Err(CoreError::Forbidden(format!(
                        "issue a token for a {:?}",
                        target.role
                    )))
                }
            });
        let secret = generate_secret()?;
        let token = self
            .db
            .create_token_checked(
                member_id,
                hash_secret(&secret),
                label,
                expires_at,
                Some(p.member_id.clone()),
                Some(auth),
                Some(MAX_LIVE_TOKENS_PER_MEMBER),
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
        let owner = match self.member_in_space(p, &token.member_id).await {
            Ok(m) => m,
            Err(CoreError::NotFound(_)) => {
                return Err(CoreError::NotFound(format!("token {token_id}")));
            }
            Err(e) => return Err(e),
        };
        self.ensure_manages(p, &owner, "revoke a token of")?;
        let auth =
            Authorize::with_token(p.member_id.clone(), p.token_id.clone(), |actor, owner| {
                let owner = owner.expect("token has an owner");
                if owner.id == actor.id || actor.role.can_manage(owner.role) {
                    Ok(())
                } else {
                    Err(CoreError::Forbidden(format!(
                        "revoke a token of a {:?}",
                        owner.role
                    )))
                }
            });
        self.db
            .revoke_token_checked(token_id, Some(p.member_id.clone()), Some(auth))
            .await
    }

    /// Re-verifies a cached [`Principal`] (e.g. on long-lived WebSocket connections) against
    /// current member status, role, and device-token usability.
    pub async fn verify_principal(&self, p: &Principal) -> CoreResult<Principal> {
        let member = match self.db.get_member(&p.member_id).await {
            Ok(m) if m.is_active() && m.space_id == p.space_id => m,
            Ok(_) | Err(CoreError::NotFound(_)) => {
                return Err(CoreError::Unauthenticated(
                    "member is disabled or no longer active".into(),
                ));
            }
            Err(e) => return Err(e),
        };
        if p.token_id.as_str() != "static-bootstrap" {
            let tok = match self.db.get_token(&p.token_id).await {
                Ok(t) => t,
                Err(CoreError::NotFound(_)) => {
                    return Err(CoreError::Unauthenticated(
                        "token is expired or revoked".into(),
                    ));
                }
                Err(e) => return Err(e),
            };
            if tok.member_id != member.id || !tok.is_usable(now_ms()) {
                return Err(CoreError::Unauthenticated(
                    "token is expired or revoked".into(),
                ));
            }
        }
        Ok(Principal {
            member_id: member.id,
            space_id: member.space_id,
            display_name: member.display_name,
            role: member.role,
            token_id: p.token_id.clone(),
        })
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
