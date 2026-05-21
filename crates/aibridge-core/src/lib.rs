//! AI Bridge core: warm peer engine, review strategies, profile translation.
//!
//! Foundation increment: exposes [`version`], a [`health`] check, and a minimal
//! [`mcp`] stdio server. The warm Codex child + review strategies land next.

pub mod health;
pub mod mcp;

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
