//! Strongly typed identifiers.
//!
//! Every id is a UUIDv7 rendered as a string on the wire, so ids sort by creation time
//! and stay opaque to clients.

use serde::{Deserialize, Serialize};
use std::fmt;
use ts_rs::TS;

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
        #[ts(export, type = "string")]
        pub struct $name(pub String);

        impl $name {
            /// Generates a new time-ordered (UUIDv7) id.
            pub fn new() -> Self {
                Self(uuid::Uuid::now_v7().to_string())
            }

            /// Borrows the id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

define_id!(
    /// A team space: the collaboration and permission boundary.
    SpaceId
);
define_id!(
    /// A member of a space.
    MemberId
);
define_id!(
    /// A registered workspace (repository or directory on the server).
    WorkspaceId
);
define_id!(
    /// A long-lived agent session.
    SessionId
);
define_id!(
    /// A pending or resolved approval request.
    ApprovalId
);
define_id!(
    /// One prompt/response cycle inside a session.
    TurnId
);
define_id!(
    /// A device token issued to a member.
    TokenId
);
define_id!(
    /// A prompt waiting in a session's queue (W2-3).
    QueuedPromptId
);
define_id!(
    /// A schedule that sends prompts on a timetable (W4-1).
    ScheduleId
);
define_id!(
    /// One fire of a schedule (W4-1).
    ScheduleRunId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_time_ordered() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
        assert!(
            a < b,
            "UUIDv7 ids generated in sequence must sort ascending"
        );
    }

    #[test]
    fn ids_serialize_as_plain_strings() {
        let id = SessionId::from("abc");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"abc\"");
    }
}
