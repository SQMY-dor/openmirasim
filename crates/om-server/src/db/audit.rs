//! Audit log and remembered approval rules (W1-3).

use super::{Db, enum_str, parse_enum, storage};
use om_core::approval::ApprovalOptionKind;
use om_core::ids::{MemberId, SessionId, SpaceId};
use om_core::{CoreResult, now_ms};
use rusqlite::{Connection, params};

/// One row of `audit_logs`.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    pub id: i64,
    pub ts: i64,
    /// Member that acted; `None` for the server (policy, timeout, recovery).
    pub actor: Option<MemberId>,
    pub action: String,
    pub target: Option<String>,
    pub detail: serde_json::Value,
}

/// A remembered "allow always" / "reject always" answer for one session.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRule {
    pub rule_key: String,
    pub allow: bool,
    /// The option kind the member picked (`AllowAlways` / `RejectAlways`).
    pub option_kind: ApprovalOptionKind,
    pub created_by: Option<MemberId>,
    pub created_at: i64,
}

/// SQL expression resolving the space an audit entry belongs to from its target (a
/// session, member, token, goal, schedule, dispatch or workspace id) or, failing that,
/// its actor. Entries resolving to no space are visible to no space.
pub(super) fn space_of(target: &str, actor: &str) -> String {
    format!(
        "COALESCE(
            (SELECT space_id FROM sessions WHERE id = {target}),
            (SELECT s.space_id FROM approvals a JOIN sessions s ON s.id = a.session_id
              WHERE a.id = {target}),
            (SELECT space_id FROM members WHERE id = {target}),
            (SELECT m.space_id FROM device_tokens t JOIN members m ON m.id = t.member_id
              WHERE t.id = {target}),
            (SELECT space_id FROM goals WHERE id = {target}),
            (SELECT space_id FROM schedules WHERE id = {target}),
            (SELECT space_id FROM dispatches WHERE id = {target}),
            (SELECT space_id FROM workspaces WHERE id = {target}),
            (SELECT space_id FROM members WHERE id = {actor}))"
    )
}

pub(super) fn insert(
    conn: &Connection,
    actor: Option<&MemberId>,
    action: &str,
    target: &str,
    detail: &serde_json::Value,
) -> CoreResult<()> {
    conn.execute(
        &format!(
            "INSERT INTO audit_logs (ts, actor, action, target, detail, space_id)
             VALUES (?1, ?2, ?3, ?4, ?5, {})",
            space_of("?4", "?2")
        ),
        params![
            now_ms(),
            actor.map(|m| m.as_str()),
            action,
            target,
            detail.to_string()
        ],
    )
    .map_err(storage)?;
    Ok(())
}

impl Db {
    /// Appends an audit entry outside any other transaction.
    pub async fn audit(
        &self,
        actor: Option<MemberId>,
        action: impl Into<String>,
        target: impl Into<String>,
        detail: serde_json::Value,
    ) -> CoreResult<()> {
        let (action, target) = (action.into(), target.into());
        self.write(move |conn| insert(conn, actor.as_ref(), &action, &target, &detail))
            .await
    }

    /// Newest-first audit entries of one space, optionally only those about `target`.
    pub async fn list_audit(
        &self,
        space_id: &SpaceId,
        target: Option<String>,
        limit: u32,
    ) -> CoreResult<Vec<AuditEntry>> {
        let space_id = space_id.clone();
        self.read(move |conn| {
            // Two statements so each can use its own index.
            let sql = if target.is_some() {
                "SELECT id, ts, actor, action, target, detail FROM audit_logs
                 WHERE space_id = ?1 AND target = ?2 ORDER BY id DESC LIMIT ?3"
            } else {
                "SELECT id, ts, actor, action, target, detail FROM audit_logs
                 WHERE space_id = ?1 AND ?2 IS NULL ORDER BY id DESC LIMIT ?3"
            };
            let mut stmt = conn.prepare(sql).map_err(storage)?;
            let rows = stmt
                .query_map(params![space_id.as_str(), target, limit], |r| {
                    let detail: Option<String> = r.get(5)?;
                    Ok(AuditEntry {
                        id: r.get(0)?,
                        ts: r.get(1)?,
                        actor: r.get::<_, Option<String>>(2)?.map(MemberId),
                        action: r.get(3)?,
                        target: r.get(4)?,
                        detail: detail
                            .and_then(|d| serde_json::from_str(&d).ok())
                            .unwrap_or(serde_json::Value::Null),
                    })
                })
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?;
            Ok(rows)
        })
        .await
    }

    /// Stores (or replaces) a session's remembered rule and audits it.
    pub async fn remember_approval_rule(
        &self,
        session_id: &SessionId,
        rule: ApprovalRule,
    ) -> CoreResult<()> {
        let session_id = session_id.clone();
        self.write(move |conn| {
            let tx = conn.transaction().map_err(storage)?;
            tx.execute(
                "INSERT INTO approval_rules (session_id, rule_key, allow, option_kind, created_by,
                                             created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (session_id, rule_key) DO UPDATE SET
                     allow = excluded.allow, option_kind = excluded.option_kind,
                     created_by = excluded.created_by, created_at = excluded.created_at",
                params![
                    session_id.as_str(),
                    rule.rule_key,
                    rule.allow,
                    enum_str(&rule.option_kind),
                    rule.created_by.as_ref().map(|m| m.as_str()),
                    rule.created_at
                ],
            )
            .map_err(storage)?;
            insert(
                &tx,
                rule.created_by.as_ref(),
                "approval.rule_remembered",
                session_id.as_str(),
                &serde_json::json!({"rule_key": rule.rule_key, "allow": rule.allow}),
            )?;
            tx.commit().map_err(storage)?;
            Ok(())
        })
        .await
    }

    /// A session's remembered rules, oldest first.
    pub async fn approval_rules(&self, session_id: &SessionId) -> CoreResult<Vec<ApprovalRule>> {
        let session_id = session_id.clone();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT rule_key, allow, option_kind, created_by, created_at
                     FROM approval_rules WHERE session_id = ?1 ORDER BY created_at, rule_key",
                )
                .map_err(storage)?;
            let rows = stmt
                .query_map([session_id.as_str()], |r| {
                    let kind: String = r.get(2)?;
                    Ok(ApprovalRule {
                        rule_key: r.get(0)?,
                        allow: r.get(1)?,
                        option_kind: parse_enum(2, &kind)?,
                        created_by: r.get::<_, Option<String>>(3)?.map(MemberId),
                        created_at: r.get(4)?,
                    })
                })
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?;
            Ok(rows)
        })
        .await
    }

    /// Forgets every remembered rule of a session (e.g. a member revokes them).
    pub async fn clear_approval_rules(
        &self,
        session_id: &SessionId,
        actor: Option<MemberId>,
    ) -> CoreResult<usize> {
        let session_id = session_id.clone();
        self.write(move |conn| {
            let tx = conn.transaction().map_err(storage)?;
            let n = tx
                .execute(
                    "DELETE FROM approval_rules WHERE session_id = ?1",
                    [session_id.as_str()],
                )
                .map_err(storage)?;
            insert(
                &tx,
                actor.as_ref(),
                "approval.rules_cleared",
                session_id.as_str(),
                &serde_json::json!({"count": n}),
            )?;
            tx.commit().map_err(storage)?;
            Ok(n)
        })
        .await
    }
}
