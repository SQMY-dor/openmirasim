//! Approval policy (W1-3).
//!
//! Every permission request an agent raises goes through [`ApprovalPolicy::evaluate`]
//! before members see it. The policy can answer on the members' behalf (auto-allow
//! reads and edits that stay inside the session's sandboxed worktree, or repeat a
//! remembered "always" answer) or defer to members. Whatever it decides, the request
//! is still persisted and audited, so the event stream shows every gated call.
//!
//! Safety rules:
//! - Automatic **allow** only happens for sandboxed sessions ([`SandboxMode::Bwrap`]),
//!   where the OS already confines writes to the worktree, scratch dir and member
//!   HOME. Path checks are lexical defence in depth, not the boundary.
//! - `Execute`, `Fetch`, `Delete`, `Move`, `SwitchMode` and `Other` never auto-allow
//!   (except through a member's explicit "allow always" for that exact rule key).
//! - Paths inside `.git` never auto-allow: git internals are how a worktree escapes.
//! - A remembered "reject always" wins over everything else.
//! - A remembered "allow always" for a file tool (`Read`, `Search`, `Edit`, `Delete`,
//!   `Move`) only applies under the same conditions as the automatic allow: a
//!   sandboxed session and paths that stay inside the worktree and out of `.git`.
//! - Requests without a meaningful title (empty, or the harness's placeholder) are never
//!   remembered, so no rule can act as a wildcard.

#[cfg(test)]
mod tests;

use om_core::approval::{ApprovalOption, ApprovalOptionKind};
use om_core::event::ToolKind;
use om_core::sandbox::SandboxMode;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Title a harness uses when the agent gave none. Never part of a remembered rule.
pub const PLACEHOLDER_TITLE: &str = "Permission requested";

/// Tools whose scope is the paths they touch.
fn is_file_tool(kind: Option<ToolKind>) -> bool {
    matches!(
        kind,
        Some(
            ToolKind::Read | ToolKind::Search | ToolKind::Edit | ToolKind::Delete | ToolKind::Move
        )
    )
}

/// Server-wide approval settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalPolicy {
    /// Auto-allow `Read` / `Search` calls whose paths all stay inside the session cwd.
    pub auto_allow_read_in_cwd: bool,
    /// Auto-allow `Edit` calls whose paths all stay inside the session cwd.
    pub auto_allow_edit_in_cwd: bool,
    /// Auto-allow `Think` calls (no side effects).
    pub auto_allow_think: bool,
    /// Honour members' "allow always" / "reject always" answers for the rest of the
    /// session (persisted, survives resume).
    pub remember_always: bool,
    /// Reject a pending request nobody answered within this time. `None` waits forever.
    pub timeout: Option<Duration>,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            auto_allow_read_in_cwd: true,
            auto_allow_edit_in_cwd: true,
            auto_allow_think: true,
            remember_always: true,
            timeout: Some(Duration::from_secs(30 * 60)),
        }
    }
}

impl ApprovalPolicy {
    /// A policy that always asks members and never times out.
    pub fn ask_always() -> Self {
        Self {
            auto_allow_read_in_cwd: false,
            auto_allow_edit_in_cwd: false,
            auto_allow_think: false,
            remember_always: false,
            timeout: None,
        }
    }
}

/// What the policy needs to know about one request.
#[derive(Debug, Clone, Copy)]
pub struct PolicyRequest<'a> {
    pub sandbox: SandboxMode,
    /// The session's server-side working directory.
    pub cwd: &'a str,
    pub kind: Option<ToolKind>,
    pub locations: &'a [String],
    pub title: &'a str,
    pub options: &'a [ApprovalOption],
}

/// A member's remembered answer for one rule key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remembered {
    pub allow: bool,
}

/// The policy's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Answer with `option_id` without asking; `rule` names why.
    Auto { option_id: String, rule: String },
    /// Ask members.
    Ask,
}

impl ApprovalPolicy {
    pub fn evaluate(
        &self,
        req: &PolicyRequest<'_>,
        remembered: &HashMap<String, Remembered>,
    ) -> Verdict {
        if self.remember_always
            && let Some(key) = rule_key(req.kind, req.title)
            && let Some(r) = remembered.get(&key)
        {
            // Never forward `AllowAlways` or `RejectAlways` to the agent: the server
            // is the sole owner of remembered rules so `clear_approval_rules` and
            // worktree/`.git` bounds stay enforceable.
            let picked = if r.allow {
                pick(req.options, &[ApprovalOptionKind::AllowOnce])
            } else {
                pick(req.options, &[ApprovalOptionKind::RejectOnce])
            };
            // A remembered reject wins over everything else — if the agent omitted a
            // one-shot reject option, ask the member instead of falling through into
            // automatic allow rules.
            if !r.allow {
                return match picked {
                    Some(option_id) => Verdict::Auto {
                        option_id,
                        rule: format!("remembered:{key}"),
                    },
                    None => Verdict::Ask,
                };
            }
            // A remembered allow for a file tool stays inside the sandboxed worktree;
            // for non-file tools (e.g. Execute/Fetch), the rule key binds to the exact
            // command or URL explicitly approved by a member.
            let allow_in_scope = !is_file_tool(req.kind)
                || (req.sandbox == SandboxMode::Bwrap && all_within(req.cwd, req.locations));
            if allow_in_scope && let Some(option_id) = picked {
                return Verdict::Auto {
                    option_id,
                    rule: format!("remembered:{key}"),
                };
            }
        }

        if req.sandbox != SandboxMode::Bwrap {
            return Verdict::Ask;
        }
        let rule = match req.kind {
            Some(ToolKind::Think) if self.auto_allow_think => "think",
            Some(ToolKind::Read | ToolKind::Search)
                if self.auto_allow_read_in_cwd && all_within(req.cwd, req.locations) =>
            {
                "read_in_worktree"
            }
            Some(ToolKind::Edit)
                if self.auto_allow_edit_in_cwd && all_within(req.cwd, req.locations) =>
            {
                "edit_in_worktree"
            }
            _ => return Verdict::Ask,
        };
        match pick(req.options, &[ApprovalOptionKind::AllowOnce]) {
            Some(option_id) => Verdict::Auto {
                option_id,
                rule: rule.to_owned(),
            },
            None => Verdict::Ask,
        }
    }

    /// The option to answer with when a request times out: a one-off rejection when
    /// offered, else `None` (the request is cancelled instead).
    pub fn timeout_option(options: &[ApprovalOption]) -> Option<String> {
        pick(options, &[ApprovalOptionKind::RejectOnce])
    }
}

/// The key a remembered "always" answer is stored under.
///
/// Side-effect-free kinds remember per kind; commands, fetches and anything unknown
/// remember per exact title (which carries the command line or URL), so "always allow
/// `cargo test`" never allows `rm -rf`. Requests without a kind, and per-title kinds
/// without a real title, are never remembered.
pub fn rule_key(kind: Option<ToolKind>, title: &str) -> Option<String> {
    let kind = kind?;
    let title = title.trim();
    let name = serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "other".into());
    Some(match kind {
        ToolKind::Read | ToolKind::Search | ToolKind::Think | ToolKind::Edit => name,
        _ if title.is_empty() || title == PLACEHOLDER_TITLE => return None,
        _ => format!("{name}:{title}"),
    })
}

/// Whether choosing `kind` should be remembered, and as allow or reject.
pub fn remembers(kind: ApprovalOptionKind) -> Option<Remembered> {
    match kind {
        ApprovalOptionKind::AllowAlways => Some(Remembered { allow: true }),
        ApprovalOptionKind::RejectAlways => Some(Remembered { allow: false }),
        _ => None,
    }
}

fn pick(options: &[ApprovalOption], kinds: &[ApprovalOptionKind]) -> Option<String> {
    kinds.iter().find_map(|k| {
        options
            .iter()
            .find(|o| o.kind == *k)
            .map(|o| o.option_id.clone())
    })
}

/// `true` when `locations` is non-empty and every path stays inside `cwd` and out of
/// any `.git` directory, after lexical normalisation (relative paths resolve against
/// `cwd`). Unknown scope (no locations) is not "within".
pub fn all_within(cwd: &str, locations: &[String]) -> bool {
    let Some(root) = normalize(Path::new(cwd)) else {
        return false;
    };
    if !root.is_absolute()
        || root.parent().is_none()
        || root.components().any(|c| c.as_os_str() == ".git")
        || locations.is_empty()
    {
        return false;
    }
    locations.iter().all(|loc| {
        if loc.trim().is_empty() || loc.contains('\0') {
            return false;
        }
        let p = Path::new(loc);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        };
        match normalize(&joined) {
            Some(n) => {
                n.starts_with(&root)
                    && n != root
                    && !n
                        .strip_prefix(&root)
                        .map(|rel| rel.components().any(|c| c.as_os_str() == ".git"))
                        .unwrap_or(true)
            }
            None => false,
        }
    })
}

/// Lexically resolves `.` and `..`; `None` if `..` climbs above the root.
fn normalize(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || out.as_os_str().is_empty() {
                    return None;
                }
            }
            Component::Normal(s) => out.push(s),
        }
    }
    Some(out)
}
