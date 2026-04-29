pub(crate) fn resolve_github_token(config_token: Option<&str>) -> Option<String> {
    let github_token = std::env::var("GITHUB_TOKEN").ok();
    let gh_token = std::env::var("GH_TOKEN").ok();
    resolve_github_token_from_sources(config_token, github_token.as_deref(), gh_token.as_deref())
}

pub(crate) fn resolve_github_token_from_sources(
    config_token: Option<&str>,
    github_token: Option<&str>,
    gh_token: Option<&str>,
) -> Option<String> {
    normalize_token(config_token)
        .or_else(|| normalize_token(github_token))
        .or_else(|| normalize_token(gh_token))
}

fn normalize_token(token: Option<&str>) -> Option<String> {
    token
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::resolve_github_token_from_sources;

    #[test]
    fn configured_token_wins() {
        assert_eq!(
            resolve_github_token_from_sources(
                Some(" config-token "),
                Some("github-token"),
                Some("gh-token"),
            )
            .as_deref(),
            Some("config-token")
        );
    }

    #[test]
    fn github_token_precedes_gh_token() {
        assert_eq!(
            resolve_github_token_from_sources(None, Some("github-token"), Some("gh-token"))
                .as_deref(),
            Some("github-token")
        );
    }

    #[test]
    fn blank_values_fall_back() {
        assert_eq!(
            resolve_github_token_from_sources(Some(""), Some(" "), Some("gh-token")).as_deref(),
            Some("gh-token")
        );
    }
}
