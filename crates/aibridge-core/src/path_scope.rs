//! v0.32 scoped-approval — UNIT 1: hardened path canonicalization + repo-root confinement.
//!
//! Pure, std-only primitive. Given a repo root and a write target (relative or
//! absolute, existing or new), produce a normalized repo-RELATIVE path ONLY when
//! the target provably resolves to a location strictly UNDER the repo root by path
//! **component** semantics (not string-prefix — `…/repo` must reject `…/repo2/x`).
//! Every ambiguity fails CLOSED (returns `Err`).
//!
//! This is the foundation later scoped-approval units build on (glob matching,
//! scope_risk classification, `enforce_tool` wiring). On its own it changes NO gate
//! behavior — nothing calls it yet, so it is additive and inert until a later unit
//! does. See the Codex-validated spec (consult topic `v032-workflow-guardrail`).

use std::path::{Component, Path, PathBuf};

/// Why a target path was refused. Every variant means "do NOT treat as in-scope".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeReject {
    /// Empty / `.` / whitespace-only input — no concrete target.
    Empty,
    /// The target canonicalizes exactly to the repo root (no relative remainder).
    EqualsRoot,
    /// The target resolves outside the repo root (incl. a sibling-prefix like `repo2`).
    OutsideRoot,
    /// A `..` segment anywhere in the target.
    Traversal,
    /// A UNC path (`\\server\share\…`).
    UncPath,
    /// A user-supplied verbatim / device path (`\\?\`, `\\?\UNC\`, `\\.\`).
    RawDevicePath,
    /// An alternate-data-stream marker (a `:` other than the drive-letter colon).
    AdsStream,
    /// A Windows reserved device-name segment (`CON`, `NUL`, `COM1`, …), incl. `NUL.txt`.
    ReservedDeviceName,
    /// A segment ending in `.` or ` ` (Windows silently retargets these).
    TrailingDotOrSpace,
    /// An empty or drive-qualified (`C:`) path segment.
    EmptyOrDriveSegment,
    /// A single leading separator (`\foo` / `/foo`) on Windows — drive-ROOT-relative,
    /// resolves against the current drive rather than being fully qualified.
    DriveRootRelative,
    /// A backslash on a non-Windows platform, where `\` is a legal filename char: the
    /// separator semantics this module enforces would be inconsistent, so refuse it.
    ForeignSeparator,
    /// The target (or its nearest existing ancestor, or the repo root) could not be canonicalized.
    NonCanonicalizable,
    /// A declared scope GLOB used syntax outside this module's restricted grammar
    /// (literals, `/`, single-segment `*`, whole-segment `**`) — e.g. `?`/`[`/`{`/`\`,
    /// a rooted/`..`/`.`/`//` form, or a malformed `**`. Used by [`crate::scope`].
    UnsupportedGlob,
}

/// Canonicalize `raw_target` and confine it under `repo_root`, returning a
/// normalized repo-RELATIVE path (forward-slash separators, canonicalized casing
/// for the existing portion) on success. Never returns an empty string.
///
/// `repo_root` MUST exist (it is the `git rev-parse --show-toplevel` of the repo).
/// `raw_target` may be relative (resolved against `repo_root`) or absolute, and may
/// name a not-yet-existing file (its nearest existing ancestor is canonicalized and
/// the new trailing segments are validated and appended).
pub fn canonicalize_under_root(repo_root: &str, raw_target: &str) -> Result<String, ScopeReject> {
    // 1. UP-FRONT rejects on the RAW input, before any filesystem call. Do NOT trim:
    //    a trailing space is itself a Windows hazard (the FS silently retargets `foo ` → `foo`),
    //    so stripping it here would defeat the trailing-space check below.
    let raw = raw_target;
    if raw.trim().is_empty() || raw == "." {
        return Err(ScopeReject::Empty);
    }
    // Verbatim / device forms are NEVER accepted as input, in EITHER separator style
    // (`\\?\`, `\\.\`, `//?/`, `//./`). Windows/Rust path parsing treats `/` as a
    // separator, so a forward-slash device form is just as dangerous. (We only ever
    // strip a `\\?\` prefix from `fs::canonicalize` OUTPUT, for internal comparison.)
    let prefix4: String = raw
        .chars()
        .take(4)
        .map(|c| if c == '/' { '\\' } else { c })
        .collect();
    if prefix4.starts_with(r"\\?\") || prefix4.starts_with(r"\\.\") {
        return Err(ScopeReject::RawDevicePath);
    }
    let b = raw.as_bytes();
    // UNC (`\\server`, `//server`, or a mixed `/\server` / `\/server`) — ANY two leading
    // separators of either kind.
    if b.len() >= 2 && is_sep(b[0]) && is_sep(b[1]) {
        return Err(ScopeReject::UncPath);
    }
    // On Windows a SINGLE leading separator (`\foo` / `/foo`) is drive-root-relative
    // (current-drive dependent), not fully qualified — reject. (Two-leading is UNC, above.)
    // On Unix a leading `/` is a legitimate absolute root, so this is Windows-only.
    #[cfg(windows)]
    if is_sep(b[0]) {
        return Err(ScopeReject::DriveRootRelative);
    }
    // On non-Windows, `\` is a normal filename char — but this module's raw-segment
    // validator treats it as a separator, so accepting it would canonicalize inconsistently
    // with `Path` semantics. Refuse any backslash as a foreign separator (fail closed).
    #[cfg(not(windows))]
    if raw.contains('\\') {
        return Err(ScopeReject::ForeignSeparator);
    }
    // Leading drive-letter syntax (`X:…`). On Windows only a full drive-ROOT `C:\…` / `C:/…`
    // is allowed; a bare `C:` or drive-RELATIVE `C:foo` resolves against the per-drive cwd →
    // reject. On non-Windows a leading `X:` is never a real path here (it would be read as a
    // directory literally named `C:`, and `Ok("C:/…")` looks drive-qualified to later
    // Windows-aware scope logic) → reject EVERY leading drive-letter form.
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        #[cfg(windows)]
        {
            if !matches!(b.get(2), Some(&c) if is_sep(c)) {
                return Err(ScopeReject::EmptyOrDriveSegment);
            }
        }
        #[cfg(not(windows))]
        {
            return Err(ScopeReject::EmptyOrDriveSegment);
        }
    }
    // Interior empty segment — ANY two ADJACENT separators of either kind (`a//b`, `a\\b`,
    // `a/\b`, `a\/b`). Leading double-separators were already rejected as UNC above.
    if b.windows(2).any(|w| is_sep(w[0]) && is_sep(w[1])) {
        return Err(ScopeReject::EmptyOrDriveSegment);
    }
    // Alternate data stream: any `:` that is not the drive-letter colon at index 1.
    if has_extra_colon(raw) {
        return Err(ScopeReject::AdsStream);
    }
    // Validate each segment of the RAW input directly — robust against later `Path`
    // normalization that can silently drop a trailing dot/space. Reject `..` traversal,
    // and on Windows reserved device names + trailing dot/space.
    for seg in raw.split(['/', '\\']).filter(|s| !s.is_empty() && *s != ".") {
        if seg == ".." {
            return Err(ScopeReject::Traversal);
        }
        #[cfg(windows)]
        {
            if is_reserved_device(seg) {
                return Err(ScopeReject::ReservedDeviceName);
            }
            if seg.ends_with('.') || seg.ends_with(' ') {
                return Err(ScopeReject::TrailingDotOrSpace);
            }
        }
    }

    // 2. Build the absolute target (relative inputs resolve against the repo root).
    let target_path = {
        let p = Path::new(raw);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(repo_root).join(p)
        }
    };

    // 3. Lexical component scan: reject `..` / empty / mid-path drive segments outright.
    for comp in target_path.components() {
        match comp {
            Component::ParentDir => return Err(ScopeReject::Traversal),
            Component::Normal(s) => {
                if s.is_empty() {
                    return Err(ScopeReject::EmptyOrDriveSegment);
                }
            }
            // A drive/UNC prefix only ever appears first; `RootDir`/`CurDir` are harmless.
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
        }
    }

    // 4. Canonicalize the repo root once, by components.
    let root_canon = fs_canonicalize_stripped(Path::new(repo_root))
        .ok_or(ScopeReject::NonCanonicalizable)?;
    let root_comps = components_str(&root_canon)?;

    // 5. Walk up to the NEAREST EXISTING DIRECTORY ENTRY, collecting the trailing (new)
    //    segments. Uses `symlink_metadata` (does NOT follow links) rather than `exists()`,
    //    because `exists()` reports `false` for a DANGLING symlink — which would let the
    //    walk treat a symlink-to-outside as a brand-new in-root segment and accept it. With
    //    `symlink_metadata`, a dangling symlink stops the walk and step 6's canonicalize then
    //    fails it closed (`NonCanonicalizable`); a live symlink is resolved and confined.
    let mut existing = target_path.clone();
    let mut trailing: Vec<String> = Vec::new();
    loop {
        match std::fs::symlink_metadata(&existing) {
            Ok(_) => break,
            // Keep walking up ONLY for a genuine "not found". Any OTHER error — a component
            // that is a file (NotADirectory), permission denied, etc. — fails CLOSED rather
            // than being mistaken for a not-yet-existing segment.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or(ScopeReject::NonCanonicalizable)?
                    .to_string_lossy()
                    .into_owned();
                let parent = existing
                    .parent()
                    .ok_or(ScopeReject::NonCanonicalizable)?
                    .to_path_buf();
                trailing.push(name);
                existing = parent;
            }
            Err(_) => return Err(ScopeReject::NonCanonicalizable),
        }
    }
    trailing.reverse(); // collected leaf-first; restore root→leaf order.

    // 6. Canonicalize the existing ancestor (resolves symlinks/junctions → catches escapes).
    let anc_canon =
        fs_canonicalize_stripped(&existing).ok_or(ScopeReject::NonCanonicalizable)?;
    let anc_comps = components_str(&anc_canon)?;

    // 7. CONTAINMENT by component boundary (case-folded on Windows). The ancestor's
    //    component sequence must START WITH the repo root's FULL sequence.
    if anc_comps.len() < root_comps.len() {
        return Err(ScopeReject::OutsideRoot);
    }
    for (r, a) in root_comps.iter().zip(anc_comps.iter()) {
        if casefold(r) != casefold(a) {
            return Err(ScopeReject::OutsideRoot);
        }
    }

    // 8. A NEW file can only be created under a DIRECTORY: if there are trailing (new)
    //    segments, the nearest existing ancestor must itself be a directory (rejects
    //    `existing_file.txt/new.rs`). Defense-in-depth: a trailing segment is never empty
    //    (step 1 already rejected `..` / reserved names / trailing dot|space).
    if !trailing.is_empty() && !anc_canon.is_dir() {
        return Err(ScopeReject::NonCanonicalizable);
    }
    for seg in &trailing {
        if seg.is_empty() {
            return Err(ScopeReject::EmptyOrDriveSegment);
        }
    }

    // 9. Build the repo-relative path: (ancestor components beyond root) + trailing.
    let mut rel: Vec<String> = anc_comps[root_comps.len()..].to_vec();
    rel.extend(trailing);
    if rel.is_empty() {
        return Err(ScopeReject::EqualsRoot);
    }
    Ok(rel.join("/"))
}

/// A path separator of either platform style.
fn is_sep(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

/// True if `raw` contains a `:` that is not the drive-letter colon at index 1
/// (e.g. `file.rs:stream`, or a mid-path `C:`), which signals an ADS / device form.
fn has_extra_colon(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let start = if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        2 // skip a single leading drive-letter colon (`C:`)
    } else {
        0
    };
    raw[start..].contains(':')
}

/// `fs::canonicalize` then strip a Windows verbatim prefix (`\\?\`, `\\?\UNC\`) so
/// component comparison sees the plain form. Returns `None` if it cannot canonicalize.
fn fs_canonicalize_stripped(p: &Path) -> Option<PathBuf> {
    let canon = std::fs::canonicalize(p).ok()?;
    Some(strip_verbatim(&canon))
}

/// Strip a leading `\\?\` / `\\?\UNC\` verbatim prefix from a canonicalized path.
/// No-op on non-Windows (and on paths that lack the prefix). Uses a NON-lossy `to_str`
/// check: a non-UTF-8 path is left untouched so the fail-closed [`components_str`]
/// rejects it downstream rather than this silently substituting `�`.
fn strip_verbatim(p: &Path) -> PathBuf {
    if let Some(s) = p.as_os_str().to_str() {
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    p.to_path_buf()
}

/// The component strings of a path, original casing preserved (a drive prefix like
/// `C:` and the root are included so comparison is anchored at the volume). FAIL-CLOSED:
/// a non-UTF-8 component (possible on Unix) yields `NonCanonicalizable` rather than a
/// lossy `�`-substituted string that could alias two distinct byte-paths to one.
fn components_str(p: &Path) -> Result<Vec<String>, ScopeReject> {
    p.components()
        .map(|c| {
            c.as_os_str()
                .to_str()
                .map(str::to_owned)
                .ok_or(ScopeReject::NonCanonicalizable)
        })
        .collect()
}

/// Case-fold for comparison: lowercase on Windows (case-insensitive volumes), identity elsewhere.
fn casefold(s: &str) -> String {
    #[cfg(windows)]
    {
        s.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s.to_string()
    }
}

/// A Windows reserved device-name segment: `CON`, `PRN`, `AUX`, `NUL`,
/// `COM1..=COM9`, `LPT1..=LPT9` — case-insensitive, matching the stem before the
/// first `.` (so `NUL.txt` is reserved too).
#[cfg(windows)]
fn is_reserved_device(seg: &str) -> bool {
    let stem = seg.split('.').next().unwrap_or(seg).to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let numbered = |prefix: &str| {
                stem.strip_prefix(prefix)
                    .and_then(|n| n.parse::<u8>().ok())
                    .map(|n| (1..=9).contains(&n))
                    .unwrap_or(false)
            };
            numbered("COM") || numbered("LPT")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A throwaway directory under the OS temp dir (B1: never the real ~/.ai-bridge),
    /// removed on drop. Canonicalized so callers compare against the real root form.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir =
                std::env::temp_dir().join(format!("aibridge-pathscope-{pid}-{n}"));
            std::fs::create_dir_all(&dir).unwrap();
            // Strip the Windows `\\?\` verbatim prefix that `fs::canonicalize` adds, so
            // the root mimics a real `git rev-parse --show-toplevel` (a clean path) —
            // not a verbatim/device form (which the function rejects as input).
            TempDir(strip_verbatim(&std::fs::canonicalize(&dir).unwrap()))
        }
        fn root(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
        fn join(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn accepts_existing_in_root_file() {
        let repo = TempDir::new();
        std::fs::write(repo.join("a.txt"), b"x").unwrap();
        assert_eq!(
            canonicalize_under_root(&repo.root(), &repo.join("a.txt").to_string_lossy()),
            Ok("a.txt".to_string())
        );
    }

    #[test]
    fn accepts_new_file_under_existing_parent() {
        let repo = TempDir::new();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        // src/new.rs does not exist yet; src/ does.
        let rel = canonicalize_under_root(
            &repo.root(),
            &repo.join("src/new.rs").to_string_lossy(),
        )
        .unwrap();
        assert_eq!(rel, "src/new.rs");
    }

    #[test]
    fn relative_input_resolves_against_repo_root() {
        let repo = TempDir::new();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert_eq!(
            canonicalize_under_root(&repo.root(), "src/new.rs"),
            Ok("src/new.rs".to_string())
        );
    }

    #[test]
    fn rejects_empty_and_dot() {
        let repo = TempDir::new();
        assert_eq!(
            canonicalize_under_root(&repo.root(), ""),
            Err(ScopeReject::Empty)
        );
        assert_eq!(
            canonicalize_under_root(&repo.root(), "   "),
            Err(ScopeReject::Empty)
        );
        assert_eq!(
            canonicalize_under_root(&repo.root(), "."),
            Err(ScopeReject::Empty)
        );
    }

    #[test]
    fn rejects_target_equal_to_root() {
        let repo = TempDir::new();
        assert_eq!(
            canonicalize_under_root(&repo.root(), &repo.root()),
            Err(ScopeReject::EqualsRoot)
        );
    }

    #[test]
    fn rejects_parent_traversal_escape() {
        let repo = TempDir::new();
        assert_eq!(
            canonicalize_under_root(&repo.root(), "../escape.txt"),
            Err(ScopeReject::Traversal)
        );
    }

    #[test]
    fn rejects_sibling_prefix_directory() {
        // `…/repo` must NOT contain `…/repo2/file` — component boundary, not string prefix.
        let parent = TempDir::new();
        let repo = parent.join("repo");
        let sibling = parent.join("repo2");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("file.txt"), b"x").unwrap();
        let repo_root = strip_verbatim(&std::fs::canonicalize(&repo).unwrap())
            .to_string_lossy()
            .into_owned();
        let target = sibling.join("file.txt").to_string_lossy().into_owned();
        assert_eq!(
            canonicalize_under_root(&repo_root, &target),
            Err(ScopeReject::OutsideRoot)
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_unc_path() {
        let repo = TempDir::new();
        assert_eq!(
            canonicalize_under_root(&repo.root(), r"\\server\share\file.txt"),
            Err(ScopeReject::UncPath)
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_raw_verbatim_even_in_root() {
        let repo = TempDir::new();
        // A `\\?\`-prefixed form of an in-root path is still rejected as raw device input.
        let verbatim = format!(r"\\?\{}\a.txt", repo.root().replace('/', "\\"));
        assert_eq!(
            canonicalize_under_root(&repo.root(), &verbatim),
            Err(ScopeReject::RawDevicePath)
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_dos_device_path() {
        let repo = TempDir::new();
        assert_eq!(
            canonicalize_under_root(&repo.root(), r"\\.\PhysicalDrive0"),
            Err(ScopeReject::RawDevicePath)
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_ads_stream() {
        let repo = TempDir::new();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert_eq!(
            canonicalize_under_root(&repo.root(), "src/file.rs:hidden"),
            Err(ScopeReject::AdsStream)
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_reserved_device_names() {
        let repo = TempDir::new();
        for name in ["NUL", "nul.txt", "CON", "COM1", "LPT9"] {
            assert_eq!(
                canonicalize_under_root(&repo.root(), name),
                Err(ScopeReject::ReservedDeviceName),
                "expected {name} to be rejected as a reserved device name"
            );
        }
        // A name that merely STARTS with a device prefix is fine.
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert!(canonicalize_under_root(&repo.root(), "src/computer.rs").is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_trailing_dot_or_space() {
        let repo = TempDir::new();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert_eq!(
            canonicalize_under_root(&repo.root(), "src/foo."),
            Err(ScopeReject::TrailingDotOrSpace)
        );
        assert_eq!(
            canonicalize_under_root(&repo.root(), "src/foo "),
            Err(ScopeReject::TrailingDotOrSpace)
        );
    }

    #[test]
    fn rejects_forward_slash_unc() {
        let repo = TempDir::new();
        assert_eq!(
            canonicalize_under_root(&repo.root(), "//server/share/file.txt"),
            Err(ScopeReject::UncPath)
        );
    }

    #[test]
    fn rejects_forward_slash_device_forms() {
        let repo = TempDir::new();
        assert_eq!(
            canonicalize_under_root(&repo.root(), "//?/C:/repo/file.txt"),
            Err(ScopeReject::RawDevicePath)
        );
        assert_eq!(
            canonicalize_under_root(&repo.root(), "//./PhysicalDrive0"),
            Err(ScopeReject::RawDevicePath)
        );
    }

    #[test]
    fn rejects_interior_empty_segment() {
        let repo = TempDir::new();
        assert_eq!(
            canonicalize_under_root(&repo.root(), "src//file.rs"),
            Err(ScopeReject::EmptyOrDriveSegment)
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_drive_relative_and_bare_drive() {
        let repo = TempDir::new();
        for t in ["C:foo", "C:", "C:foo:stream"] {
            assert_eq!(
                canonicalize_under_root(&repo.root(), t),
                Err(ScopeReject::EmptyOrDriveSegment),
                "expected {t} to be rejected as drive-qualified / drive-relative"
            );
        }
    }

    // On Windows both `/` and `\` are separators, so adjacent mixed ones are an empty
    // interior segment. (On Unix a `\` is rejected earlier as a foreign separator — see
    // `rejects_backslash_on_unix` — so this case is Windows-specific.)
    #[cfg(windows)]
    #[test]
    fn rejects_mixed_adjacent_separators() {
        let repo = TempDir::new();
        for t in [r"src/\file.rs", r"src\/file.rs"] {
            assert_eq!(
                canonicalize_under_root(&repo.root(), t),
                Err(ScopeReject::EmptyOrDriveSegment),
                "expected {t} (mixed adjacent separators) to be rejected"
            );
        }
    }

    #[test]
    fn rejects_new_file_under_existing_file() {
        let repo = TempDir::new();
        std::fs::write(repo.join("file.txt"), b"x").unwrap();
        assert_eq!(
            canonicalize_under_root(&repo.root(), "file.txt/new.rs"),
            Err(ScopeReject::NonCanonicalizable)
        );
    }

    #[test]
    fn rejects_mixed_leading_separators_as_unc() {
        let repo = TempDir::new();
        for t in [r"/\server/share/file.txt", r"\/server/share/file.txt"] {
            assert_eq!(
                canonicalize_under_root(&repo.root(), t),
                Err(ScopeReject::UncPath),
                "expected {t} (mixed leading separators) to be rejected as UNC"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_drive_root_relative() {
        let repo = TempDir::new();
        for t in [r"\repo\file.rs", "/repo/file.rs", r"\", "/"] {
            assert_eq!(
                canonicalize_under_root(&repo.root(), t),
                Err(ScopeReject::DriveRootRelative),
                "expected {t} (single leading separator) to be rejected as drive-root-relative"
            );
        }
    }

    // On non-Windows a leading drive-letter form (`C:/foo`) is not a real absolute path —
    // it would be a dir literally named `C:`; returning `Ok("C:/…")` would mislead later
    // Windows-aware scope logic, so reject it.
    #[cfg(not(windows))]
    #[test]
    fn rejects_drive_letter_forms_on_unix() {
        let repo = TempDir::new();
        for t in ["C:/foo", "C:/nested/file.rs", "Z:/repo/file.rs"] {
            assert_eq!(
                canonicalize_under_root(&repo.root(), t),
                Err(ScopeReject::EmptyOrDriveSegment),
                "expected {t} (drive-letter form on unix) to be rejected"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn rejects_backslash_on_unix() {
        let repo = TempDir::new();
        for t in [r"src\file.rs", r"src\nested\file.rs"] {
            assert_eq!(
                canonicalize_under_root(&repo.root(), t),
                Err(ScopeReject::ForeignSeparator),
                "expected {t} (backslash on unix) to be rejected as a foreign separator"
            );
        }
    }

    // A canonical path with non-UTF-8 bytes (reachable on Unix via a symlink to a
    // non-UTF-8-named target) must fail CLOSED, not be lossily normalized to `�`.
    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_canonical_path() {
        use std::os::unix::ffi::OsStrExt;
        let repo = TempDir::new();
        // an in-root file whose name is not valid UTF-8
        let bad_path = repo.0.join(std::ffi::OsStr::from_bytes(b"bad\xFFname"));
        std::fs::write(&bad_path, b"x").unwrap();
        // a UTF-8-named symlink pointing at it (so raw_target is a valid &str)
        let link = repo.join("link");
        std::os::unix::fs::symlink(&bad_path, &link).unwrap();
        assert_eq!(
            canonicalize_under_root(&repo.root(), &link.to_string_lossy()),
            Err(ScopeReject::NonCanonicalizable)
        );
    }

    // A dangling symlink inside the repo must NOT be accepted as a new in-root segment —
    // a later write would follow it outside the root. (Unix: symlinks need no privilege.)
    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlink() {
        let parent = TempDir::new();
        let repo = parent.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let link = repo.join("out");
        // points outside the repo, to a target that does not exist (dangling).
        std::os::unix::fs::symlink("../outside/new.txt", &link).unwrap();
        let repo_root = strip_verbatim(&std::fs::canonicalize(&repo).unwrap())
            .to_string_lossy()
            .into_owned();
        let res = canonicalize_under_root(&repo_root, &link.to_string_lossy());
        assert!(
            matches!(
                res,
                Err(ScopeReject::NonCanonicalizable) | Err(ScopeReject::OutsideRoot)
            ),
            "dangling symlink must be rejected, got {res:?}"
        );
    }

    // A LIVE symlink whose target exists but is OUTSIDE the repo must be confined out:
    // `fs::canonicalize` resolves it and component-containment then rejects it.
    #[cfg(unix)]
    #[test]
    fn rejects_live_symlink_escape() {
        let parent = TempDir::new();
        let repo = parent.join("repo");
        let outside = parent.join("outside");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("real.txt"), b"x").unwrap();
        // repo/link -> ../outside (a live dir symlink escaping the repo)
        std::os::unix::fs::symlink(&outside, repo.join("link")).unwrap();
        let repo_root = strip_verbatim(&std::fs::canonicalize(&repo).unwrap())
            .to_string_lossy()
            .into_owned();
        let target = repo.join("link/real.txt").to_string_lossy().into_owned();
        assert_eq!(
            canonicalize_under_root(&repo_root, &target),
            Err(ScopeReject::OutsideRoot)
        );
    }
}
