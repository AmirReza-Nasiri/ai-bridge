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

/// Collect the comma-separated glob payloads of every line whose label (case-insensitive)
/// is `label_lower` (which MUST include the trailing colon, lowercase). The glob payloads
/// are taken VERBATIM (case preserved — globs are case-sensitive), trimmed, and deduped
/// (first-seen kept). Shared by [`parse_scope_approved`] and [`declared_globs`].
fn parse_label_globs(text: &str, label_lower: &str, split_commas: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        // Match only the LABEL case-insensitively; never lowercase the glob payload.
        let Some(head) = trimmed.get(..label_lower.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(label_lower) {
            continue;
        }
        let payload = &trimmed[label_lower.len()..];
        // `declared_globs` is a single comma-separated line; a `SCOPE-APPROVED:` reviewer
        // marker is exactly ONE glob per line (no comma splitting — narrower semantics).
        let parts: Vec<&str> = if split_commas {
            payload.split(',').collect()
        } else {
            vec![payload]
        };
        for tok in parts {
            let g = tok.trim();
            if !g.is_empty() && !out.iter().any(|e| e == g) {
                out.push(g.to_string());
            }
        }
    }
    out
}

/// Parse reviewer-owned `SCOPE-APPROVED: <glob>` markers from Codex's review FINDINGS.
/// Label matched case-insensitively; glob payload verbatim. Reviewer-owned — never scanned
/// from plan prose (so a plan merely mentioning a path can't self-authorize scope).
pub fn parse_scope_approved(findings: &str) -> Vec<String> {
    parse_label_globs(findings, "scope-approved:", false) // one glob per marker line
}

/// Parse Claude's machine-readable scope declaration — a standalone `ALLOWED-GLOBS: a, b, c`
/// line in the plan. Label case-insensitive; comma-separated globs verbatim. Absent → empty.
pub fn declared_globs(plan: &str) -> Vec<String> {
    parse_label_globs(plan, "allowed-globs:", true) // a single comma-separated declaration line
}

/// The fail-closed approved scope = globs that are BOTH declared by Claude (`ALLOWED-GLOBS:`)
/// AND echoed by the reviewer (`SCOPE-APPROVED:`), then kept only if they pass [`validate_glob`]
/// and the breadth policy: `RepoWide` is ALWAYS dropped (no repo-wide scope in v0.32 even if
/// echoed); `Broad` (recursive) is kept ONLY when `broad_scope_granted`; `Narrow`/`Moderate`
/// are kept. Intersection is case-SENSITIVE (globs are case-sensitive paths).
///
/// `broad_scope_granted` is supplied by the caller; deriving it from a reviewer
/// `RISK-APPROVED: broad-scope` grant (plus the `RISK_CLASSES`/prompt change) is DEFERRED to
/// the storage/wiring unit. Nothing here is stored or enforced yet — this is pure policy.
pub fn approved_scope(plan: &str, findings: &str, broad_scope_granted: bool) -> Vec<String> {
    let echoed = parse_scope_approved(findings);
    declared_globs(plan)
        .into_iter()
        .filter(|g| echoed.iter().any(|e| e == g)) // case-sensitive intersection
        .filter(|g| validate_glob(g).is_ok())
        .filter(|g| match classify_glob(g) {
            ScopeRisk::Narrow | ScopeRisk::Moderate => true,
            ScopeRisk::Broad => broad_scope_granted,
            ScopeRisk::RepoWide => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn validate_glob_accepts_supported_grammar() {
        for g in [
            "src/main.rs",
            "dir/*",
            "*.rs",
            "dir/**",
            "src/**/*.rs",
            "a/*/b.rs",
        ] {
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
        for g in [
            "*",
            "**",
            "**/*",
            "**/foo.rs",
            "",
            "src/[ab].rs",
            "f?.rs",
            "a**b",
            r"src\x",
        ] {
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
        assert_eq!(
            scope_risk_of(&[]),
            ScopeRisk::Narrow,
            "empty scope = deny-all = Narrow"
        );
        assert_eq!(scope_risk_of(&s(&["src/main.rs"])), ScopeRisk::Narrow);
        assert_eq!(
            scope_risk_of(&s(&["src/main.rs", "dir/*"])),
            ScopeRisk::Moderate
        );
        assert_eq!(
            scope_risk_of(&s(&["src/main.rs", "dir/**"])),
            ScopeRisk::Broad
        );
        assert_eq!(
            scope_risk_of(&s(&["src/main.rs", "**"])),
            ScopeRisk::RepoWide
        );
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
        assert!(path_in_allowed(
            "src/main.rs",
            &s(&[r"src\*", "src/main.rs"])
        ));
        // malformed `**` and bracket syntax are rejected through the public API too —
        // a list of ONLY invalid globs admits nothing.
        assert!(!path_in_allowed(
            "src/main.rs",
            &s(&["**.rs", "src/[ab].rs"])
        ));
        // ...but a valid glob alongside the invalid ones still admits a match.
        assert!(path_in_allowed("src/main.rs", &s(&["**.rs", "src/**"])));
    }

    #[test]
    fn parse_scope_approved_reads_markers_case_insensitively_preserving_glob_case() {
        let findings = "Some prose.\nSCOPE-APPROVED: src/foo.rs\nscope-approved: Src/Bar.rs\n\
                        Scope-Approved: tests/x.rs\nnot a marker: nope\n\
                        SCOPE-APPROVED: src/foo.rs"; // duplicate
        let got = parse_scope_approved(findings);
        assert_eq!(
            got,
            vec![
                "src/foo.rs".to_string(),
                "Src/Bar.rs".to_string(), // glob case PRESERVED
                "tests/x.rs".to_string(),
            ],
            "labels case-insensitive, glob payloads verbatim + deduped"
        );
    }

    #[test]
    fn scope_approved_is_exactly_one_glob_per_marker_line() {
        // A `SCOPE-APPROVED:` line is ONE glob — commas are NOT a separator here (narrower
        // reviewer-approval semantics than the comma-separated `ALLOWED-GLOBS:` declaration).
        assert_eq!(
            parse_scope_approved("SCOPE-APPROVED: a.rs, b.rs"),
            vec!["a.rs, b.rs".to_string()]
        );
        // ...whereas the Claude declaration DOES split on commas.
        assert_eq!(
            declared_globs("ALLOWED-GLOBS: a.rs, b.rs"),
            vec!["a.rs".to_string(), "b.rs".to_string()]
        );
    }

    #[test]
    fn declared_globs_parses_the_line_case_preserving() {
        assert_eq!(
            declared_globs("plan...\nALLOWED-GLOBS: src/Foo.rs, tests/*\nmore"),
            vec!["src/Foo.rs".to_string(), "tests/*".to_string()]
        );
        assert_eq!(
            declared_globs("allowed-globs: a.rs"),
            vec!["a.rs".to_string()]
        );
        assert!(declared_globs("no declaration here").is_empty());
    }

    #[test]
    fn approved_scope_is_the_policy_filtered_intersection() {
        // declared ∩ echoed, then breadth policy.
        let plan = "ALLOWED-GLOBS: src/a.rs, src/b.rs, src/**, only_declared.rs, big/**";
        let findings = "SCOPE-APPROVED: src/a.rs\nSCOPE-APPROVED: src/b.rs\n\
                        SCOPE-APPROVED: src/**\nSCOPE-APPROVED: only_echoed.rs\n\
                        SCOPE-APPROVED: big/**";
        // Without a broad-scope grant: Narrow kept, recursive (Broad) dropped, declared-only
        // and echoed-only dropped.
        assert_eq!(
            approved_scope(plan, findings, false),
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );
        // With a broad-scope grant: the recursive globs (in BOTH) are kept too.
        assert_eq!(
            approved_scope(plan, findings, true),
            vec![
                "src/a.rs".to_string(),
                "src/b.rs".to_string(),
                "src/**".to_string(),
                "big/**".to_string(),
            ]
        );
    }

    #[test]
    fn approved_scope_intersection_is_case_sensitive_and_drops_repo_wide() {
        // case mismatch → not in the intersection.
        assert!(
            approved_scope("ALLOWED-GLOBS: src/x.rs", "SCOPE-APPROVED: Src/X.rs", true).is_empty()
        );
        // a repo-wide glob declared AND echoed AND broad-granted is STILL dropped.
        assert!(approved_scope("ALLOWED-GLOBS: **", "SCOPE-APPROVED: **", true).is_empty());
        // an invalid glob in both is dropped.
        assert!(approved_scope(
            "ALLOWED-GLOBS: src/[ab].rs",
            "SCOPE-APPROVED: src/[ab].rs",
            true
        )
        .is_empty());
    }

    #[test]
    fn approved_scope_keeps_moderate_regardless_of_broad_grant() {
        // A `Moderate` (single-level wildcard) glob is its own policy tier — it must be kept
        // whether or not a broad-scope grant is present (it must NOT ride the Broad tier).
        let plan = "ALLOWED-GLOBS: tests/*";
        let findings = "SCOPE-APPROVED: tests/*";
        assert_eq!(
            approved_scope(plan, findings, false),
            vec!["tests/*".to_string()]
        );
        assert_eq!(
            approved_scope(plan, findings, true),
            vec!["tests/*".to_string()]
        );
    }
}
