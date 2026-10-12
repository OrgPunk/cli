//! Environments, protect and CI mode.

pub mod protect;

/// CI mode: `--ci`, or `CI=true` as set by CI services.
pub fn ci_mode(flag: bool, ci_env: Option<&str>) -> bool {
    flag || ci_env.is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
}
