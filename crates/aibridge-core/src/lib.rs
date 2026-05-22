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

/// The crate (and product) version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_nonempty() {
        assert!(!super::version().is_empty());
    }
}
