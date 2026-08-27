use super::RuntimeDispatchPolicy;

pub(super) fn validate_reserved_runtime_profiles(
    policy: &RuntimeDispatchPolicy,
) -> anyhow::Result<()> {
    let validate = |location: &str, profile: Option<&str>| -> anyhow::Result<()> {
        if profile.is_some_and(|profile| profile.starts_with("server-owned-")) {
            anyhow::bail!("{location} uses reserved runtime profile namespace `server-owned-*`");
        }
        Ok(())
    };

    validate(
        "runtime_dispatch.runtime_profile",
        policy.runtime_profile.as_deref(),
    )?;
    for (workflow, profile) in &policy.workflow_profiles {
        validate(
            &format!("runtime_dispatch.workflow_profiles.{workflow}.runtime_profile"),
            profile.runtime_profile.as_deref(),
        )?;
    }
    for (activity, profile) in &policy.activity_profiles {
        validate(
            &format!("runtime_dispatch.activity_profiles.{activity}.runtime_profile"),
            profile.runtime_profile.as_deref(),
        )?;
    }
    for (workflow, activities) in &policy.workflow_activity_profiles {
        for (activity, profile) in activities {
            validate(
                &format!(
                    "runtime_dispatch.workflow_activity_profiles.{workflow}.{activity}.runtime_profile"
                ),
                profile.runtime_profile.as_deref(),
            )?;
        }
    }
    Ok(())
}
