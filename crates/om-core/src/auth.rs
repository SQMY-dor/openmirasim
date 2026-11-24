//! Team membership, roles and device tokens (W2-1).
//!
//! A space has at most [`MAX_MEMBERS_PER_SPACE`] active members, each with one
//! [`Role`]. Members sign in with device tokens: opaque bearer secrets that the server
//! shows exactly once and stores only as a hash. Every request runs as a [`Principal`];
//! [`Role::can`] is the single place that decides what a role may do.

use crate::ids::{MemberId, SpaceId, TokenId};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Maximum number of active (not disabled) members in one space.
pub const MAX_MEMBERS_PER_SPACE: usize = 30;

/// Prefix of every device token secret, so leaked tokens are easy to grep for.
pub const TOKEN_PREFIX: &str = "omt_";

/// A member's role, ordered from least to most privileged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Role {
    /// Read-only: sees sessions, events and diffs.
    Viewer,
    /// Works with agents: creates and prompts sessions, answers approvals.
    Member,
    /// Runs the team: manages workspaces, members below admin, and reads the audit log.
    Admin,
    /// Owns the space: everything, including other admins and owners.
    Owner,
}

/// Something a principal may try to do. Checked with [`Role::can`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Action {
    /// List sessions, read events, diffs and approvals.
    ViewSessions,
    /// Create, resume or close a session.
    CreateSession,
    /// Send prompts, cancel turns, queue or steer.
    PromptSession,
    /// Answer an approval request.
    ResolveApproval,
    /// List or clear a session's remembered "always" approval rules.
    ManageApprovalRules,
    /// Register or remove repositories / workspaces.
    ManageWorkspaces,
    /// Add members, change roles, disable members, manage other members' tokens.
    ManageMembers,
    /// Read the audit log.
    ViewAudit,
    /// Change space-wide settings (approval policy, limits, keys).
    ManageSpace,
}

impl Role {
    /// Every role, least privileged first.
    pub const ALL: [Role; 4] = [Role::Viewer, Role::Member, Role::Admin, Role::Owner];

    /// Whether this role may perform `action`.
    pub fn can(self, action: Action) -> bool {
        use Action::*;
        match action {
            ViewSessions => true,
            CreateSession | PromptSession | ResolveApproval | ManageApprovalRules => {
                self >= Role::Member
            }
            ManageWorkspaces | ManageMembers | ViewAudit => self >= Role::Admin,
            ManageSpace => self == Role::Owner,
        }
    }

    /// Whether a member with this role may act on (change, disable, revoke tokens of)
    /// another member whose role is `target`. Owners manage everyone; admins manage only
    /// roles strictly below their own; nobody else manages anyone.
    pub fn can_manage(self, target: Role) -> bool {
        self.can(Action::ManageMembers) && (self == Role::Owner || target < self)
    }

    /// Whether a member with this role may give someone the role `to`.
    pub fn can_assign(self, to: Role) -> bool {
        self.can_manage(to)
    }
}

/// A member of a space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MemberRecord {
    pub id: MemberId,
    pub space_id: SpaceId,
    pub display_name: String,
    pub role: Role,
    #[ts(type = "number")]
    pub created_at: i64,
    /// Set once the member is disabled; their tokens stop working at the same moment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub disabled_at: Option<i64>,
}

impl MemberRecord {
    pub fn is_active(&self) -> bool {
        self.disabled_at.is_none()
    }
}

/// A device token's metadata. The secret itself is never part of this record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DeviceTokenRecord {
    pub id: TokenId,
    pub member_id: MemberId,
    /// Where the token is used ("MacBook", "om-cli on build box").
    pub label: String,
    #[ts(type = "number")]
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub last_used_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub revoked_at: Option<i64>,
}

impl DeviceTokenRecord {
    /// Whether the token still authenticates at time `now` (Unix ms).
    pub fn is_usable(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|exp| now < exp)
    }
}

/// A freshly issued token. `secret` is returned exactly once and never stored.
#[derive(Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IssuedToken {
    pub token: DeviceTokenRecord,
    pub secret: String,
}

impl std::fmt::Debug for IssuedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedToken")
            .field("token", &self.token)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

/// Who a request runs as, resolved from its bearer token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Principal {
    pub member_id: MemberId,
    pub space_id: SpaceId,
    pub display_name: String,
    pub role: Role,
    /// The token the request authenticated with.
    pub token_id: TokenId,
}

impl Principal {
    /// Whether the principal's role allows `action`.
    pub fn can(&self, action: Action) -> bool {
        self.role.can(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_matrix() {
        use Action::*;
        let expect = |role: Role, allowed: &[Action]| {
            for action in [
                ViewSessions,
                CreateSession,
                PromptSession,
                ResolveApproval,
                ManageApprovalRules,
                ManageWorkspaces,
                ManageMembers,
                ViewAudit,
                ManageSpace,
            ] {
                assert_eq!(
                    role.can(action),
                    allowed.contains(&action),
                    "{role:?} / {action:?}"
                );
            }
        };
        expect(Role::Viewer, &[ViewSessions]);
        let member = [
            ViewSessions,
            CreateSession,
            PromptSession,
            ResolveApproval,
            ManageApprovalRules,
        ];
        expect(Role::Member, &member);
        let admin = [&member[..], &[ManageWorkspaces, ManageMembers, ViewAudit]].concat();
        expect(Role::Admin, &admin);
        expect(Role::Owner, &[&admin[..], &[ManageSpace]].concat());
    }

    #[test]
    fn management_is_strictly_downward_except_for_owners() {
        assert!(Role::Owner.can_manage(Role::Owner));
        assert!(Role::Owner.can_assign(Role::Admin));
        assert!(Role::Admin.can_manage(Role::Member));
        assert!(Role::Admin.can_assign(Role::Viewer));
        assert!(!Role::Admin.can_manage(Role::Admin));
        assert!(!Role::Admin.can_assign(Role::Admin));
        assert!(!Role::Admin.can_assign(Role::Owner));
        assert!(!Role::Member.can_manage(Role::Viewer));
        assert!(!Role::Viewer.can_manage(Role::Viewer));
    }

    #[test]
    fn role_wire_names() {
        assert_eq!(serde_json::to_string(&Role::Owner).unwrap(), "\"owner\"");
        assert_eq!(
            serde_json::to_string(&Action::ResolveApproval).unwrap(),
            "\"resolve_approval\""
        );
    }

    #[test]
    fn token_usability() {
        let mut t = DeviceTokenRecord {
            id: TokenId::new(),
            member_id: MemberId::new(),
            label: "x".into(),
            created_at: 0,
            expires_at: Some(100),
            last_used_at: None,
            revoked_at: None,
        };
        assert!(t.is_usable(99));
        assert!(!t.is_usable(100));
        t.expires_at = None;
        assert!(t.is_usable(i64::MAX));
        t.revoked_at = Some(1);
        assert!(!t.is_usable(0));
    }
}
