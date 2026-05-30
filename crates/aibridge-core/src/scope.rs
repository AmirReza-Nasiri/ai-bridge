//! v0.32 scoped-approval — UNIT 2: glob scope-risk classification + matching.
//!
//! Builds on [`crate::path_scope`]: given a canonical repo-relative path (its output)
//! and a set of declared `allowed_globs`, decide membership FAIL-CLOSED, and classify a
//! glob's breadth so an over-broad scope can be refused by policy.
//!
//! Restricted grammar — the classifier AND the matcher share it, so they can never
//! diverge: NORMALIZED repo-relative forward-slash patterns over
//!   { literal chars, `/`, `*` (a SINGLE path segment — never crosses `/`), `**` (a
//!     WHOLE segment, spanning directories) }.
//! Every other construct (`?` `[` `]` `{` `}` `!` `\`, a rooted / `..` / `.` / `//`
//! form, a drive-qualified segment, or a malformed `**`) is UNSUPPORTED → rejected by
//! [`validate_glob`], classified [`ScopeRisk::RepoWide`], and never matched.
//!
//! Inert/additive: nothing wires this into the gate yet (a later unit does), so it
//! changes NO gate behavior.

use crate::path_scope::ScopeReject;
use globset::GlobBuilder;

/// How broad a declared scope glob is. Policy may refuse anything wider than `Narrow`
/// without an explicit broad-scope approval. Ordered least→most permissive so a `.max()`
/// over a set yields the broadest member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeRisk {
    /// A pure literal path — exactly one file.
    Narrow,
    /// A single-level wildcard (`dir/*`, `*.rs`) — one directory level, no recursion.
    Moderate,
    /// A non-leading recursive `**` (`dir/**`, `src/**/*.rs`) — many directories.
    Broad,
    /// `*` / `**` / `**/*` / a leading `**`, or any unsupported/invalid form — effectively
    /// the whole repo.
    RepoWide,
}

/// The single grammar gate. `Ok(())` iff `glob` is in the restricted grammar; otherwise
/// [`ScopeReject::UnsupportedGlob`]. Fail-closed: anything ambiguous is rejected.
pub fn validate_glob(glob: &str) -> Result<(), ScopeReject> {
    if glob.is_empty() {
        return Err(ScopeReject::UnsupportedGlob);
    }
    // Forward-slash grammar ONLY — never rely on globset's platform-dependent backslash /
    // escape behavior.
    if glob.contains('\\') {
        return Err(ScopeReject::UnsupportedGlob);
    }
    // Glob metasyntax outside { `*`, `**` } is unsupported.
    if glob.contains(['?', '[', ']', '{', '}', '!']) {
        return Err(ScopeReject::UnsupportedGlob);
    }
    // A run of three or more `*` is malformed.
    if glob.contains("***") {
        return Err(ScopeReject::UnsupportedGlob);
    }
    // No rooted / absolute form.
    if glob.starts_with('/') {
        return Err(ScopeReject::UnsupportedGlob);
    }
    for seg in glob.split('/') {
        // Empty segment (`//`, leading or trailing `/`).
        if seg.is_empty() {
            return Err(ScopeReject::UnsupportedGlob);
        }
        // `.` / `..` path segments — globs must be already-normalized.
        if seg == "." || seg == ".." {
            return Err(ScopeReject::UnsupportedGlob);
        }
        // A `:` (drive-qualified or ADS-like) is foreign to a repo-relative glob.
        if seg.contains(':') {
            return Err(ScopeReject::UnsupportedGlob);
        }
        // `**` is only valid as a WHOLE segment; `a**b` / `**.rs` / `x**` are malformed.
        if seg != "**" && seg.contains("**") {
            return Err(ScopeReject::UnsupportedGlob);
        }
    }
    Ok(())
}

/// Classify a glob's breadth. An unsupported/invalid glob is conservatively `RepoWide`
/// (policy flags it; the matcher also refuses it), so the classifier never under-reports.
pub fn classify_glob(glob: &str) -> ScopeRisk {
    if validate_glob(glob).is_err() {
        return ScopeRisk::RepoWide;
    }
    let segs: Vec<&str> = glob.split('/').collect();
    // RepoWide: a bare `*`, a bare `**`, `**/*`, or ANY leading `**` (matches from root).
    if glob == "*" || glob == "**" || glob == "**/*" || segs.first() == Some(&"**") {
        return ScopeRisk::RepoWide;
    }
    // Broad: any remaining (non-leading) `**` segment — spans multiple directories.
    if segs.contains(&"**") {
        return ScopeRisk::Broad;
    }
    // Moderate: a single-segment `*` somewhere (no `**` — handled above).
    if glob.contains('*') {
        return ScopeRisk::Moderate;
    }
    // Narrow: a pure literal path.
    ScopeRisk::Narrow
}

/// The breadth of a whole declared scope = its MOST permissive (max) glob. An EMPTY scope
/// authorizes NO writes ([`path_in_allowed`] returns `false`), so it is the least-permissive
/// case and reports `Narrow` — it can never over-authorize.
pub fn scope_risk_of(globs: &[String]) -> ScopeRisk {
    globs
        .iter()
        .map(|g| classify_glob(g))
        .max()
        .unwrap_or(ScopeRisk::Narrow)
}

/// Match a single canonical repo-relative path against ONE glob, fail-closed. An
/// unsupported glob, or one globset cannot build, never matches.
fn glob_matches(glob: &str, rel: &str) -> bool {
    if validate_glob(glob).is_err() {
        return false;
    }
    match GlobBuilder::new(glob)
        .literal_separator(true) // `*` does NOT cross `/`
        .backslash_escape(false) // no escape syntax (and `\` is already rejected)
        .build()
    {
        Ok(g) => g.compile_matcher().is_match(rel),
        Err(_) => false,
    }
}

/// True iff the canonical repo-relative `rel` matches ANY valid glob in `allowed_globs`.
/// An empty list, or only invalid globs, yields `false` (FAIL CLOSED — nothing is in scope
/// unless an approved glob explicitly admits it).
pub fn path_in_allowed(rel: &str, allowed_globs: &[String]) -> bool {
    allowed_globs.iter().any(|g| glob_matches(g, rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn validate_glob_accepts_supported_grammar() {
        for g in ["src/main.rs", "dir/*", "*.rs", "dir/**", "src/**/*.rs", "a/*/b.rs"] {
            assert!(validate_glob(g).is_ok(), "{g} should be accepted");
        }
    }

    #[test]
    fn validate_glob_rejects_everything_else() {
        for g in [
            "",
            r"src\x",
            r"src\*",
            r"src\*.rs",
            r"src/\*.rs",
            "src/[ab].rs",
            "f?.rs",
            "{a,b}.rs",
            "!x.rs",
            "***",
            "a**b",
            "**.rs",
            "x**",
            "/rooted.rs",
            "src//x.rs",
            "src/./x.rs",
            "src/../x.rs",
            "C:/x.rs",
            "src/x:y.rs",
        ] {
            assert_eq!(
                validate_glob(g),
                Err(ScopeReject::UnsupportedGlob),
                "{g} should be rejected"
            );
        }
    }

    #[test]
    fn classify_glob_tiers() {
        // RepoWide
        for g in ["*", "**", "**/*", "**/foo.rs", "", "src/[ab].rs", "f?.rs", "a**b", r"src\x"] {
            assert_eq!(classify_glob(g), ScopeRisk::RepoWide, "{g} → RepoWide");
        }
        // Broad
        for g in ["src/**/*.rs", "foo/**/bar/*", "dir/**"] {
            assert_eq!(classify_glob(g), ScopeRisk::Broad, "{g} → Broad");
        }
        // Moderate
        for g in ["dir/*", "*.rs", "a/*/b.rs"] {
            assert_eq!(classify_glob(g), ScopeRisk::Moderate, "{g} → Moderate");
        }
        // Narrow
        for g in ["src/main.rs", "a/b/c.txt"] {
            assert_eq!(classify_glob(g), ScopeRisk::Narrow, "{g} → Narrow");
        }
    }

    #[test]
    fn scope_risk_of_takes_the_max_and_defines_empty() {
        assert_eq!(scope_risk_of(&[]), ScopeRisk::Narrow, "empty scope = deny-all = Narrow");
        assert_eq!(scope_risk_of(&s(&["src/main.rs"])), ScopeRisk::Narrow);
        assert_eq!(
            scope_risk_of(&s(&["src/main.rs", "dir/*"])),
            ScopeRisk::Moderate
        );
        assert_eq!(
            scope_risk_of(&s(&["src/main.rs", "dir/**"])),
            ScopeRisk::Broad
        );
        assert_eq!(scope_risk_of(&s(&["src/main.rs", "**"])), ScopeRisk::RepoWide);
    }

    #[test]
    fn star_does_not_cross_slash() {
        // `*.rs` is a single top-level segment — it must NOT match a nested path.
        assert!(!glob_matches("*.rs", "src/main.rs"));
        assert!(glob_matches("*.rs", "main.rs"));
        // `dir/*` is one level under dir, not deeper.
        assert!(glob_matches("dir/*", "dir/a.rs"));
        assert!(!glob_matches("dir/*", "dir/a/b.rs"));
    }

    #[test]
    fn doublestar_spans_directories() {
        assert!(glob_matches("dir/**", "dir/a/b.rs"));
        assert!(glob_matches("dir/**", "dir/a.rs"));
        assert!(glob_matches("src/**/*.rs", "src/a/b/c.rs"));
        // unrelated prefix never matches.
        assert!(!glob_matches("dir/**", "other/a.rs"));
    }

    #[test]
    fn literal_matches_only_itself() {
        assert!(glob_matches("src/main.rs", "src/main.rs"));
        assert!(!glob_matches("src/main.rs", "src/other.rs"));
        assert!(!glob_matches("src/main.rs", "src/main.rs.bak"));
        // Anchored at the repo root — a matching SUFFIX under another prefix must NOT match.
        assert!(!glob_matches("src/main.rs", "x/src/main.rs"));
    }

    #[test]
    fn path_in_allowed_any_match_fail_closed() {
        // empty list → nothing in scope.
        assert!(!path_in_allowed("src/main.rs", &[]));
        // any valid glob admits it.
        assert!(path_in_allowed("src/main.rs", &s(&["docs/*", "src/**"])));
        assert!(!path_in_allowed("lib/x.rs", &s(&["docs/*", "src/**"])));
        // an invalid glob in the list contributes NO match (but a valid sibling still works).
        assert!(!path_in_allowed("src/main.rs", &s(&[r"src\*"])));
        assert!(path_in_allowed("src/main.rs", &s(&[r"src\*", "src/main.rs"])));
        // malformed `**` and bracket syntax are rejected through the public API too —
        // a list of ONLY invalid globs admits nothing.
        assert!(!path_in_allowed("src/main.rs", &s(&["**.rs", "src/[ab].rs"])));
        // ...but a valid glob alongside the invalid ones still admits a match.
        assert!(path_in_allowed("src/main.rs", &s(&["**.rs", "src/**"])));
    }
}
