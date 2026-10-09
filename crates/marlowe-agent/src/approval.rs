//! The approval port. The daemon implements it as an ApprovalRequested/Approve round trip.

use marlowe_proto::{ApprovalDecision, SessionId};

/// One approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalReq {
    pub session: SessionId,
    pub call_id: String,
    pub tool: String,
    /// One line, from `Consequence::Ask`.
    pub summary: String,
    /// The full arguments, pretty-printed.
    pub detail: String,
}

/// Asks the user. Blocks the calling tool thread; must return `Deny` on timeout or cancel.
pub trait ApprovalGate: Send + Sync {
    fn ask(&self, req: ApprovalReq) -> ApprovalDecision;
}

/// Allows everything. Tests and headless trusted runs only.
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoApprove;

impl ApprovalGate for AutoApprove {
    fn ask(&self, _req: ApprovalReq) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
}

/// Denies everything that asks.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

impl ApprovalGate for DenyAll {
    fn ask(&self, _req: ApprovalReq) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}
