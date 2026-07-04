use super::*;

#[test]
fn intake_trust_classifies_github_author_association() -> anyhow::Result<()> {
    let parsed = parse_gh_output(
        br##"[
            {"number":1,"title":"Owner","body":null,"url":"u1","labels":[],"author_association":"OWNER","createdAt":null},
            {"number":2,"title":"Member","body":null,"url":"u2","labels":[],"author_association":"MEMBER","createdAt":null},
            {"number":3,"title":"Collaborator","body":null,"url":"u3","labels":[],"author_association":"COLLABORATOR","createdAt":null},
            {"number":4,"title":"First timer","body":null,"url":"u4","labels":[],"author_association":"FIRST_TIME_CONTRIBUTOR","createdAt":null},
            {"number":5,"title":"Missing association","body":null,"url":"u5","labels":[],"createdAt":null}
        ]"##,
        "owner/repo",
        &DashMap::new(),
        None,
    )?;

    let classes: Vec<_> = parsed
        .new_issues
        .into_iter()
        .map(|issue| issue.author_trust_class)
        .collect();

    assert_eq!(
        classes,
        vec![
            IsolationTrustClass::Trusted,
            IsolationTrustClass::Trusted,
            IsolationTrustClass::Trusted,
            IsolationTrustClass::NonCollaborator,
            IsolationTrustClass::NonCollaborator,
        ]
    );
    Ok(())
}
