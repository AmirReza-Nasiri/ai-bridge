//! AI Bridge core: the warm Codex peer engine ([`codex`]), the [`mcp`] stdio
//! server (consult / plan_gate / review_diff / implement / run / review_stop /
//! health / capability_status / budget_status), the two review gates ([`gate`],
//! [`plan_gate`]), [`git`] diffing, persisted consult [`topics`], the rtk
//! [`optimizer`] hook, [`install`] wiring, [`doctor`] checks, and [`update`].

pub mod claude_mcp;
pub mod codex;
pub mod doctor;
pub mod gate;
pub mod git;
pub mod health;
pub mod install;
pub mod managed_skills;
pub mod mcp;
pub mod optimizer;
pub mod plan_gate;
pub mod plan_receipt;
pub mod progress;
pub mod review_frontier;
pub mod review_mcp;
pub mod skills;
pub mod tool_discovery;
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

/// Full version string for `--version` / `doctor`, e.g. `0.4.0 (git 8f2c9da, 2026-05-23)`.
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
