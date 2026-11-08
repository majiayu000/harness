//! Which approval requests the backend answers with "allow once".
//!
//! The harness request says which tools a turn may use (`allowed_tools`, or
//! unrestricted when `None`). agent-runtime passes each vendor's request
//! through with its raw payload, so the tool is recognised from that payload.
//! Anything not recognised is rejected: an unknown request must never be
//! approved by default.

use agent_runtime::core::PendingRequest;
use agent_runtime::Reply;
use harness_core::agent::AgentRequest;

pub(super) struct Grants {
    all: bool,
    tools: Vec<String>,
}

/// Harness tool names a request maps to.
fn tools_of(request: &PendingRequest) -> Vec<&'static str> {
    let text = format!("{} {}", request.title.as_deref().unwrap_or(""), request.raw).to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    let mut tools = Vec::new();
    if has(&[
        "\"write\"",
        "\"edit\"",
        "multiedit",
        "notebookedit",
        "filechange",
        "apply_patch",
        "\"permission\":\"edit\"",
        "\"action\":\"edit\"",
    ]) {
        tools.push("Edit");
        tools.push("Write");
    }
    if has(&[
        "\"bash\"",
        "commandexecution",
        "\"permission\":\"bash\"",
        "exec_command",
        "\"shell\"",
    ]) {
        tools.push("Bash");
    }
    if has(&["\"read\"", "\"grep\"", "\"glob\""]) {
        tools.push("Read");
    }
    tools
}

impl Grants {
    pub(super) fn from_request(req: &AgentRequest) -> Self {
        Self {
            all: req.allowed_tools.is_none(),
            tools: req.allowed_tools.clone().unwrap_or_default(),
        }
    }

    /// `Some(allow once)` when every tool the request touches is granted.
    pub(super) fn decide(&self, request: &PendingRequest) -> Option<Reply> {
        let allow = request.options.iter().find(|o| o.kind == "allow_once")?;
        let needed = tools_of(request);
        let granted = self.all
            || (!needed.is_empty() && needed.iter().any(|t| self.tools.iter().any(|g| g == t)));
        granted.then(|| Reply::Select(allow.id.clone()))
    }
}

/// Reject once if the agent offers it, otherwise cancel. Never "always".
pub(super) fn reject(request: &PendingRequest) -> Reply {
    match request.options.iter().find(|o| o.kind == "reject_once") {
        Some(option) => Reply::Select(option.id.clone()),
        None => Reply::Cancel,
    }
}
