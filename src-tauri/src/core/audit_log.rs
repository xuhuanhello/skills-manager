//! Append-only audit log of user/system actions.
//!
//! Stored in the existing SQLite database (table `audit_log`). Writes are
//! best-effort: failures are swallowed so they never block the user action
//! they accompany. Reads return newest-first.
//!
//! The log is auto-pruned to `MAX_ENTRIES` rows on each write so the table
//! cannot grow unbounded.

use serde::Serialize;

/// Hard cap on log size. Older rows are dropped on insert.
pub const MAX_ENTRIES: i64 = 10_000;

/// One audit log entry as exposed to the frontend / exports.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub id: i64,
    /// Unix timestamp in seconds.
    pub ts: i64,
    /// Short action verb, e.g. "install", "remove", "enable", "sync".
    pub action: String,
    pub skill_id: Option<String>,
    pub skill_name: Option<String>,
    /// Affected tool/agent key when the action targets one, e.g. "claude_code".
    pub tool: Option<String>,
    pub success: bool,
    /// Free-form detail. Error message on failure, optional context otherwise.
    pub detail: Option<String>,
}

/// Payload used when recording a new entry.
#[derive(Debug, Default, Clone)]
pub struct AuditDraft {
    pub action: String,
    pub skill_id: Option<String>,
    pub skill_name: Option<String>,
    pub tool: Option<String>,
    pub success: bool,
    pub detail: Option<String>,
}

impl AuditDraft {
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            ..Default::default()
        }
    }

    pub fn ok(mut self) -> Self {
        self.success = true;
        self
    }

    pub fn fail(mut self, error: impl Into<String>) -> Self {
        self.success = false;
        // Error details often embed the failing git remote URL, which may carry
        // an inline `user:token@` credential. Sanitize here — the single sink
        // for every audit detail — so nothing secret is ever persisted to the
        // `audit_log` table (which, unlike `settings`, is not encrypted at rest).
        self.detail = Some(crate::core::log_sanitize::sanitize(&error.into()));
        self
    }

    pub fn skill(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.skill_id = Some(id.into());
        self.skill_name = Some(name.into());
        self
    }

    pub fn tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        // Same rationale as `fail`: scrub any credential-bearing text before it
        // reaches the unencrypted audit_log table.
        self.detail = Some(crate::core::log_sanitize::sanitize(&detail.into()));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_redacts_inline_git_credentials() {
        let draft = AuditDraft::new("install")
            .fail("clone failed: https://alice:ghp_secretsecretsecret@github.com/foo/bar.git");
        let detail = draft.detail.expect("detail set");
        assert!(!detail.contains("ghp_secretsecretsecret"), "token leaked: {detail}");
        assert!(detail.contains("<redacted>"));
        // Host is preserved so the log is still diagnostically useful.
        assert!(detail.contains("github.com/foo/bar.git"));
    }

    #[test]
    fn detail_redacts_credentials_too() {
        let draft = AuditDraft::new("sync").detail("remote https://u:tok@host/x.git");
        assert!(!draft.detail.unwrap().contains("tok@"));
    }
}
