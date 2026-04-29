pub(crate) mod engines;
pub(crate) mod intake;
pub(crate) mod registry;
pub(crate) mod services;
pub(crate) mod storage;

#[cfg(test)]
tokio::task_local! {
    static FORCED_STARTUP_FAILURES: std::collections::HashMap<&'static str, String>;
}

#[cfg(test)]
pub(crate) async fn with_forced_startup_failures<F, T>(
    failures: &[(&'static str, &str)],
    future: F,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let mut forced = std::collections::HashMap::new();
    for (name, error) in failures {
        forced.insert(*name, (*error).to_string());
    }
    FORCED_STARTUP_FAILURES.scope(forced, future).await
}

pub(crate) fn forced_startup_error(name: &'static str) -> Option<String> {
    #[cfg(test)]
    {
        return FORCED_STARTUP_FAILURES
            .try_with(|failures| failures.get(name).cloned())
            .ok()
            .flatten();
    }

    #[cfg(not(test))]
    {
        let _ = name;
        None
    }
}
