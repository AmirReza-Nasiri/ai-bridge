//! AI Bridge core: warm peer engine, review strategies, profile translation.
//!
//! Foundation only exposes [`version`]; engine modules land in later phases.

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
