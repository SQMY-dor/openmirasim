//! Members and device tokens (W2-1).
//!
//! The database enforces the invariants that must hold however a request got here:
//! the per-space member cap, "a space always keeps an active owner", "disabled members
//! hold no live tokens", and that only token hashes are ever stored. Who may ask for a
//! change is decided one layer up, in [`crate::auth`].

use super::{Db, audit, enum_str, parse_enum, storage};
use om_core::auth::{DeviceTokenRecord, MAX_MEMBERS_PER_SPACE, MemberRecord, Principal, Role};
use om_core::ids::{MemberId, SpaceId, TokenId};
use om_core::{CoreError, CoreResult, now_ms};
use rusqlite::{Connection, OptionalExtension, Row, params};

/// `last_used_at` is refreshed at most this often per token, so authenticating does not
/// turn every request into a write.
pub const TOKEN_TOUCH_INTERVAL_MS: i64 = 60_000;

const MEMBER_COLS: &str = "id, space_id, display_name, role, created_at, disabled_at";
const TOKEN_COLS: &str = "id, member_id, label, created_at, expires_at, last_used_at, revoked_at";

fn member_from_row(r: &Row<'_>) -> rusqlite::Result<MemberRecord> {
    Ok(MemberRecord {
        id: MemberId(r.get(0)?),
        space_id: SpaceId(r.get(1)?),
        display_name: r.get(2)?,
        role: parse_enum(3, &r.get::<_, String>(3)?)?,
        created_at: r.get(4)?,
        disabled_at: r.get(5)?,
    })
}

fn token_from_row(r: &Row<'_>) -> rusqlite::Result<DeviceTokenRecord> {
    Ok(DeviceTokenRecord {
        id: TokenId(r.get(0)?),
        member_id: MemberId(r.get(1)?),
        label: r.get(2)?,
        created_at: r.get(3)?,
        expires_at: r.get(4)?,
        last_used_at: r.get(5)?,
        revoked_at: r.get(6)?,
    })
}

fn get_member_in(conn: &Connection, id: &MemberId) -> CoreResult<MemberRecord> {
    conn.query_row(
        &format!("SELECT {MEMBER_COLS} FROM members WHERE id = ?1"),
        [id.as_str()],
        member_from_row,
    )
    .optional()
    .map_err(storage)?
    .ok_or_else(|| CoreError::NotFound(format!("member {id}")))
}

fn get_token_in(conn: &Connection, id: &TokenId) -> CoreResult<DeviceTokenRecord> {
    conn.query_row(
        &format!("SELECT {TOKEN_COLS} FROM device_tokens WHERE id = ?1"),
        [id.as_str()],
        token_from_row,
    )
    .optional()
    .map_err(storage)?
    .ok_or_else(|| CoreError::NotFound(format!("token {id}")))
}

fn active_count(conn: &Connection, space_id: &SpaceId, role: Option<Role>) -> CoreResult<usize> {
    let role = role.map(|r| enum_str(&r));
    conn.query_row(
        "SELECT COUNT(*) FROM members
         WHERE space_id = ?1 AND disabled_at IS NULL AND (?2 IS NULL OR role = ?2)",
        params![space_id.as_str(), role],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n as usize)
    .map_err(storage)
}

/// Fails when `member` is the space's only active owner, so it cannot stop being one.
fn ensure_not_last_owner(conn: &Connection, member: &MemberRecord) -> CoreResult<()> {
    if member.role == Role::Owner
        && member.is_active()
        && active_count(conn, &member.space_id, Some(Role::Owner))? <= 1
    {
        return Err(CoreError::Forbidden(
            "the space must keep at least one active owner".into(),
        ));
    }
    Ok(())
}

fn insert_member(
    conn: &Connection,
    space_id: &SpaceId,
    display_name: &str,
    role: Role,
) -> CoreResult<MemberRecord> {
    let display_name = display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > 64 {
        return Err(CoreError::InvalidInput(
            "display name must be 1 to 64 characters".into(),
        ));
    }
    if active_count(conn, space_id, None)? >= MAX_MEMBERS_PER_SPACE {
        return Err(CoreError::LimitExceeded(format!(
            "a space holds at most {MAX_MEMBERS_PER_SPACE} active members"
        )));
    }
    let member = MemberRecord {
        id: MemberId::new(),
        space_id: space_id.clone(),
        display_name: display_name.to_owned(),
        role,
        created_at: now_ms(),
        disabled_at: None,
    };
    conn.execute(
        "INSERT INTO members (id, space_id, display_name, role, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            member.id.as_str(),
            member.space_id.as_str(),
            member.display_name,
            enum_str(&member.role),
            member.created_at
        ],
    )
    .map_err(storage)?;
    Ok(member)
}

fn insert_token(
    conn: &Connection,
    member: &MemberRecord,
    token_hash: &str,
    label: &str,
    expires_at: Option<i64>,
) -> CoreResult<DeviceTokenRecord> {
    if !member.is_active() {
        return Err(CoreError::Forbidden(format!(
            "member {} is disabled",
            member.id
        )));
    }
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 64 {
        return Err(CoreError::InvalidInput(
            "token label must be 1 to 64 characters".into(),
        ));
    }
    let token = DeviceTokenRecord {
        id: TokenId::new(),
        member_id: member.id.clone(),
        label: label.to_owned(),
        created_at: now_ms(),
        expires_at,
        last_used_at: None,
        revoked_at: None,
    };
    conn.execute(
        "INSERT INTO device_tokens (id, member_id, token_hash, label, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            token.id.as_str(),
            token.member_id.as_str(),
            token_hash,
            token.label,
            token.created_at,
            token.expires_at
        ],
    )
    .map_err(storage)?;
    Ok(token)
}

fn token_detail(token: &DeviceTokenRecord) -> serde_json::Value {
    serde_json::json!({
        "token_id": token.id,
        "label": token.label,
        "expires_at": token.expires_at,
    })
}

/// A permission check run inside the same write transaction as the change it guards,
/// against the actor's and the target's *current* rows (review P1-3), so a role change
/// or disablement committed in between cannot be bypassed.
pub struct Authorize {
    pub actor: MemberId,
    /// `(actor now, target now)`; the target is `None` when the change has none yet.
    #[allow(clippy::type_complexity)]
    pub check: Box<dyn FnOnce(&MemberRecord, Option<&MemberRecord>) -> CoreResult<()> + Send>,
}

impl Authorize {
    pub fn new(
        actor: MemberId,
        check: impl FnOnce(&MemberRecord, Option<&MemberRecord>) -> CoreResult<()> + Send + 'static,
    ) -> Self {
        Self {
            actor,
            check: Box::new(check),
        }
    }
}

/// Runs `auth` against the actor's current row: it must be active and in the target's
/// space (other spaces look like `NotFound`).
fn authorize_in(
    conn: &Connection,
    auth: Option<Authorize>,
    space: &SpaceId,
    target: Option<&MemberRecord>,
) -> CoreResult<()> {
    let Some(auth) = auth else {
        return Ok(());
    };
    let actor = get_member_in(conn, &auth.actor)
        .map_err(|_| CoreError::Forbidden(format!("member {} is not active", auth.actor)))?;
    if !actor.is_active() {
        return Err(CoreError::Forbidden(format!(
            "member {} is not active",
            auth.actor
        )));
    }
    if &actor.space_id != space {
        return Err(CoreError::NotFound(match target {
            Some(t) => format!("member {}", t.id),
            None => "space".into(),
        }));
    }
    (auth.check)(&actor, target)
}

impl Db {
    /// Creates the space's first member as `Owner` with one token, atomically.
    ///
    /// Returns `None` (and changes nothing) once the space has any member, so running
    /// bootstrap on every start is safe and can never mint a second owner.
    pub async fn bootstrap_owner(
        &self,
        space_id: &SpaceId,
        display_name: &str,
        token_hash: String,
        label: &str,
    ) -> CoreResult<Option<(MemberRecord, DeviceTokenRecord)>> {
        let (space_id, display_name, label) =
            (space_id.clone(), display_name.to_owned(), label.to_owned());
        self.write(move |conn| {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            let any: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM members WHERE space_id = ?1",
                    [space_id.as_str()],
                    |r| r.get(0),
                )
                .map_err(storage)?;
            if any > 0 {
                return Ok(None);
            }
            let member = insert_member(&tx, &space_id, &display_name, Role::Owner)?;
            let token = insert_token(&tx, &member, &token_hash, &label, None)?;
            audit::insert(
                &tx,
                None,
                "member.bootstrapped",
                member.id.as_str(),
                &serde_json::json!({"role": member.role, "token_id": token.id}),
            )?;
            tx.commit().map_err(storage)?;
            Ok(Some((member, token)))
        })
        .await
    }

    /// Adds a member, enforcing the per-space cap.
    pub async fn create_member(
        &self,
        space_id: &SpaceId,
        display_name: &str,
        role: Role,
        actor: Option<MemberId>,
    ) -> CoreResult<MemberRecord> {
        self.create_member_checked(space_id, display_name, role, actor, None)
            .await
    }

    /// [`Db::create_member`] with a permission check in the same transaction.
    pub async fn create_member_checked(
        &self,
        space_id: &SpaceId,
        display_name: &str,
        role: Role,
        actor: Option<MemberId>,
        auth: Option<Authorize>,
    ) -> CoreResult<MemberRecord> {
        let (space_id, display_name) = (space_id.clone(), display_name.to_owned());
        self.write(move |conn| {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            authorize_in(&tx, auth, &space_id, None)?;
            let member = insert_member(&tx, &space_id, &display_name, role)?;
            audit::insert(
                &tx,
                actor.as_ref(),
                "member.created",
                member.id.as_str(),
                &serde_json::json!({"display_name": member.display_name, "role": role}),
            )?;
            tx.commit().map_err(storage)?;
            Ok(member)
        })
        .await
    }

    pub async fn get_member(&self, id: &MemberId) -> CoreResult<MemberRecord> {
        let id = id.clone();
        self.read(move |conn| get_member_in(conn, &id)).await
    }

    /// All members of a space (disabled ones included), oldest first.
    pub async fn list_members(&self, space_id: &SpaceId) -> CoreResult<Vec<MemberRecord>> {
        let space_id = space_id.clone();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {MEMBER_COLS} FROM members WHERE space_id = ?1 ORDER BY created_at, id"
                ))
                .map_err(storage)?;
            stmt.query_map([space_id.as_str()], member_from_row)
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)
        })
        .await
    }

    /// Changes a member's role. Refuses to demote the last active owner.
    pub async fn set_member_role(
        &self,
        id: &MemberId,
        role: Role,
        actor: Option<MemberId>,
    ) -> CoreResult<MemberRecord> {
        self.set_member_role_checked(id, role, actor, None).await
    }

    /// [`Db::set_member_role`] with a permission check in the same transaction.
    pub async fn set_member_role_checked(
        &self,
        id: &MemberId,
        role: Role,
        actor: Option<MemberId>,
        auth: Option<Authorize>,
    ) -> CoreResult<MemberRecord> {
        let id = id.clone();
        self.write(move |conn| {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            let mut member = get_member_in(&tx, &id)?;
            authorize_in(&tx, auth, &member.space_id.clone(), Some(&member))?;
            if !member.is_active() {
                return Err(CoreError::Forbidden(format!("member {id} is disabled")));
            }
            if member.role == role {
                return Ok(member);
            }
            if role != Role::Owner {
                ensure_not_last_owner(&tx, &member)?;
            }
            tx.execute(
                "UPDATE members SET role = ?2 WHERE id = ?1",
                params![id.as_str(), enum_str(&role)],
            )
            .map_err(storage)?;
            audit::insert(
                &tx,
                actor.as_ref(),
                "member.role_changed",
                id.as_str(),
                &serde_json::json!({"from": member.role, "to": role}),
            )?;
            tx.commit().map_err(storage)?;
            member.role = role;
            Ok(member)
        })
        .await
    }

    /// Disables a member and revokes all of their tokens in the same transaction.
    /// Refuses to disable the last active owner. Disabling twice is a no-op.
    pub async fn disable_member(
        &self,
        id: &MemberId,
        actor: Option<MemberId>,
    ) -> CoreResult<MemberRecord> {
        self.disable_member_checked(id, actor, None).await
    }

    /// [`Db::disable_member`] with a permission check in the same transaction.
    pub async fn disable_member_checked(
        &self,
        id: &MemberId,
        actor: Option<MemberId>,
        auth: Option<Authorize>,
    ) -> CoreResult<MemberRecord> {
        let id = id.clone();
        self.write(move |conn| {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            let mut member = get_member_in(&tx, &id)?;
            authorize_in(&tx, auth, &member.space_id.clone(), Some(&member))?;
            if !member.is_active() {
                return Ok(member);
            }
            ensure_not_last_owner(&tx, &member)?;
            let now = now_ms();
            tx.execute(
                "UPDATE members SET disabled_at = ?2 WHERE id = ?1",
                params![id.as_str(), now],
            )
            .map_err(storage)?;
            let revoked = tx
                .execute(
                    "UPDATE device_tokens SET revoked_at = ?2
                     WHERE member_id = ?1 AND revoked_at IS NULL",
                    params![id.as_str(), now],
                )
                .map_err(storage)?;
            audit::insert(
                &tx,
                actor.as_ref(),
                "member.disabled",
                id.as_str(),
                &serde_json::json!({"tokens_revoked": revoked}),
            )?;
            tx.commit().map_err(storage)?;
            member.disabled_at = Some(now);
            Ok(member)
        })
        .await
    }

    /// Stores a new token for an active member. Only the hash of the secret is kept.
    pub async fn create_token(
        &self,
        member_id: &MemberId,
        token_hash: String,
        label: &str,
        expires_at: Option<i64>,
        actor: Option<MemberId>,
    ) -> CoreResult<DeviceTokenRecord> {
        self.create_token_checked(member_id, token_hash, label, expires_at, actor, None, None)
            .await
    }

    /// [`Db::create_token`] with a permission check in the same transaction and an
    /// optional cap on the member's live tokens.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_token_checked(
        &self,
        member_id: &MemberId,
        token_hash: String,
        label: &str,
        expires_at: Option<i64>,
        actor: Option<MemberId>,
        auth: Option<Authorize>,
        max_live_tokens: Option<u32>,
    ) -> CoreResult<DeviceTokenRecord> {
        let (member_id, label) = (member_id.clone(), label.to_owned());
        self.write(move |conn| {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            let member = get_member_in(&tx, &member_id)?;
            authorize_in(&tx, auth, &member.space_id.clone(), Some(&member))?;
            if let Some(max) = max_live_tokens {
                let live: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM device_tokens
                         WHERE member_id = ?1 AND revoked_at IS NULL
                           AND (expires_at IS NULL OR expires_at > ?2)",
                        params![member_id.as_str(), now_ms()],
                        |r| r.get(0),
                    )
                    .map_err(storage)?;
                if live >= i64::from(max) {
                    return Err(CoreError::LimitExceeded(format!(
                        "member already has {live} live tokens (max {max})"
                    )));
                }
            }
            let token = insert_token(&tx, &member, &token_hash, &label, expires_at)?;
            audit::insert(
                &tx,
                actor.as_ref(),
                "token.issued",
                member_id.as_str(),
                &token_detail(&token),
            )?;
            tx.commit().map_err(storage)?;
            Ok(token)
        })
        .await
    }

    pub async fn get_token(&self, id: &TokenId) -> CoreResult<DeviceTokenRecord> {
        let id = id.clone();
        self.read(move |conn| get_token_in(conn, &id)).await
    }

    /// A member's tokens, newest first, revoked ones included.
    pub async fn list_tokens(&self, member_id: &MemberId) -> CoreResult<Vec<DeviceTokenRecord>> {
        let member_id = member_id.clone();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {TOKEN_COLS} FROM device_tokens WHERE member_id = ?1
                     ORDER BY created_at DESC, id DESC"
                ))
                .map_err(storage)?;
            stmt.query_map([member_id.as_str()], token_from_row)
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)
        })
        .await
    }

    /// Revokes a token. Revoking an already revoked token is a no-op.
    pub async fn revoke_token(
        &self,
        id: &TokenId,
        actor: Option<MemberId>,
    ) -> CoreResult<DeviceTokenRecord> {
        self.revoke_token_checked(id, actor, None).await
    }

    /// [`Db::revoke_token`] with a permission check (against the token's owner) in the
    /// same transaction.
    pub async fn revoke_token_checked(
        &self,
        id: &TokenId,
        actor: Option<MemberId>,
        auth: Option<Authorize>,
    ) -> CoreResult<DeviceTokenRecord> {
        let id = id.clone();
        self.write(move |conn| {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            let mut token = get_token_in(&tx, &id)?;
            let owner = get_member_in(&tx, &token.member_id)?;
            authorize_in(&tx, auth, &owner.space_id.clone(), Some(&owner)).map_err(
                |e| match e {
                    CoreError::NotFound(_) => CoreError::NotFound(format!("token {id}")),
                    other => other,
                },
            )?;
            if token.revoked_at.is_some() {
                return Ok(token);
            }
            let now = now_ms();
            tx.execute(
                "UPDATE device_tokens SET revoked_at = ?2 WHERE id = ?1",
                params![id.as_str(), now],
            )
            .map_err(storage)?;
            audit::insert(
                &tx,
                actor.as_ref(),
                "token.revoked",
                token.member_id.as_str(),
                &token_detail(&token),
            )?;
            tx.commit().map_err(storage)?;
            token.revoked_at = Some(now);
            Ok(token)
        })
        .await
    }

    /// Resolves a token hash to the principal it authenticates, if the token is live and
    /// its member active at `now`. Any other outcome is `None`, never a reason.
    pub async fn principal_for_token_hash(
        &self,
        token_hash: String,
        now: i64,
    ) -> CoreResult<Option<(Principal, DeviceTokenRecord)>> {
        self.read(move |conn| {
            let row = conn
                .query_row(
                    "SELECT t.id, t.member_id, t.label, t.created_at, t.expires_at,
                            t.last_used_at, t.revoked_at,
                            m.space_id, m.display_name, m.role, m.disabled_at
                     FROM device_tokens t JOIN members m ON m.id = t.member_id
                     WHERE t.token_hash = ?1",
                    [&token_hash],
                    |r| {
                        let token = token_from_row(r)?;
                        let space_id = SpaceId(r.get(7)?);
                        let display_name: String = r.get(8)?;
                        let role: Role = parse_enum(9, &r.get::<_, String>(9)?)?;
                        let disabled_at: Option<i64> = r.get(10)?;
                        Ok((token, space_id, display_name, role, disabled_at))
                    },
                )
                .optional()
                .map_err(storage)?;
            Ok(
                row.and_then(|(token, space_id, display_name, role, disabled_at)| {
                    (disabled_at.is_none() && token.is_usable(now)).then(|| {
                        (
                            Principal {
                                member_id: token.member_id.clone(),
                                space_id,
                                display_name,
                                role,
                                token_id: token.id.clone(),
                            },
                            token,
                        )
                    })
                }),
            )
        })
        .await
    }

    /// Records that a token was used, at most once per [`TOKEN_TOUCH_INTERVAL_MS`].
    pub async fn touch_token(&self, id: &TokenId, now: i64) -> CoreResult<()> {
        let id = id.clone();
        self.write(move |conn| {
            conn.execute(
                "UPDATE device_tokens SET last_used_at = ?2
                 WHERE id = ?1 AND (last_used_at IS NULL OR last_used_at <= ?2 - ?3)",
                params![id.as_str(), now, TOKEN_TOUCH_INTERVAL_MS],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }
}
