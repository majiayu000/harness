//! Prompts produced by the intake pipeline (issue → agent submission).

/// Borrowed view of the fields needed to render an "implement this issue" prompt.
///
/// Lives in `harness-core` so callers in `harness-server::intake` (which owns the
/// `IncomingIssue` struct) do not need to share their type with this crate.
pub struct IncomingIssueView<'a> {
    pub source: &'a str,
    pub identifier: &'a str,
    pub title: &'a str,
    pub url: Option<&'a str>,
    pub description: Option<&'a str>,
}

/// Prompt the agent receives for an unattended issue submission.
pub fn from_incoming_issue(view: &IncomingIssueView<'_>) -> String {
    format!(
        "You are working on {source} issue {id}: {title}\n\n\
         URL: {url}\n\n\
         Description:\n{desc}\n\n\
         Instructions:\n\
         1. This is an unattended session. Do not ask humans for help.\n\
         2. Implement changes, run validation, create PR, push.\n\
         3. Only stop for true blockers (missing auth/permissions).",
        source = view.source,
        id = view.identifier,
        title = view.title,
        url = view.url.unwrap_or("N/A"),
        desc = view.description.unwrap_or("No description."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_fields() {
        let prompt = from_incoming_issue(&IncomingIssueView {
            source: "github",
            identifier: "42",
            title: "Fix the bug",
            url: Some("https://example/issues/42"),
            description: Some("Bug body."),
        });
        assert!(prompt.contains("github issue 42: Fix the bug"));
        assert!(prompt.contains("URL: https://example/issues/42"));
        assert!(prompt.contains("Description:\nBug body."));
        assert!(prompt.contains("unattended session"));
    }

    #[test]
    fn fills_defaults_when_missing() {
        let prompt = from_incoming_issue(&IncomingIssueView {
            source: "feishu",
            identifier: "FE-1",
            title: "no body",
            url: None,
            description: None,
        });
        assert!(prompt.contains("URL: N/A"));
        assert!(prompt.contains("Description:\nNo description."));
    }
}
