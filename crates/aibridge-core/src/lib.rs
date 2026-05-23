//! AI Bridge core: warm peer engine, review strategies, profile translation.
//!
//! Increment: exposes [`version`], a [`health`] check, a [`codex`] warm-peer
//! client, and the [`mcp`] stdio server (with a live `consult` tool).

pub mod codex;
pub mod doctor;
pub mod gate;
pub mod git;
pub mod health;
pub mod install;
pub mod mcp;
pub mod optimizer;
pub mod plan_gate;
pub mod topics;
pub mod update;

/// The crate (and product) semver version (from `Cargo.toml`).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Git short SHA of the build (with a `-dirty` suffix for uncommitted builds), or
/// "unknown" if built without git. Embedded by `build.rs`.
pub const GIT_SHA: &str = env!("AIBRIDGE_GIT_SHA");

/// Commit date (YYYY-MM-DD) of the build, or "unknown". Embedded by `build.rs`.
pub const BUILD_DATE: &str = env!("AIBRIDGE_BUILD_DATE");

/// Full version string for `--version` / `doctor`: `0.2.0 (git 8f2c9da, 2026-05-23)`.
pub const VERSION_FULL: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (git ",
    env!("AIBRIDGE_GIT_SHA"),
    ", ",
    env!("AIBRIDGE_BUILD_DATE"),
    ")"
);

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_nonempty() {
        assert!(!super::version().is_empty());
    }
}
