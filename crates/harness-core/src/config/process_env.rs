use std::env::{var as std_var, var_os as std_var_os, VarError};
use std::ffi::OsString;

/// Single funnel for process environment reads used by config resolution.
///
/// Runtime code that needs general process state may read it directly, but
/// config-driven environment overrides should go through this module so they
/// remain auditable in one place.
pub fn var(name: &str) -> Result<String, VarError> {
    std_var(name)
}

pub fn var_os(name: &str) -> Option<OsString> {
    std_var_os(name)
}

pub fn config_value(name: &str) -> Option<String> {
    var(name).ok().filter(|value| !value.is_empty())
}

pub fn trimmed_config_value(name: &str) -> Option<String> {
    var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn non_blank_config_value(name: &str) -> Option<String> {
    var(name).ok().filter(|value| !value.trim().is_empty())
}

pub fn first_non_blank_config_value(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| non_blank_config_value(name))
}

pub fn first_trimmed_config_value(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| trimmed_config_value(name))
}
