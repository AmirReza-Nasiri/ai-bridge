//! v0.32 read-only discovery carve-out — a pure, lexical classifier for Bash commands
//! that are safe to RUN *pre-approval*.
//!
//! Without this, the plan gate default-denies ALL Bash until a plan is approved, which
//! forces a `plan_gate` round just to inspect a repo with `git status` / `git diff` /
//! `find` (commands that have no non-Bash tool equivalent). This classifier lets a TIGHT
//! allowlist of proven read-only commands through, but ONLY when the opt-in
//! `planGate.readOnlyOrientation` config is enabled (default off).
//!
//! FAIL-CLOSED by construction: a positive safe-character gate bars every shell
//! metacharacter (so no expansion / redirection / compound / substitution / glob / tilde
//! can reach the allowlist), and only an explicit allowlist returns `true`.
//!
//! Confinement: every path operand must be RELATIVE and stay under the cwd — absolute paths
//! (`/etc/passwd`, `C:/Users`, `\\…`), any `..` escape, and option-embedded path reads
//! (`--files0-from=…`, `--exclude-from=…`, any `--flag=/abs`) are rejected. Blocking/stdin
//! forms are rejected too: a bare `-`, `tail -f`/`-F`/`--follow`, and a content reader
//! (`cat`/`head`/`tail`/`wc`) with no file operand (which would read stdin and hang the hook).
//! `git diff`/`show` are restricted to SUMMARY forms (`--stat`/`--name-only`/…); content
//! rendering (`-p`/`--patch`) is denied so no textconv/ext-diff/pager helper executes.
//!
//! OWNER-ACCEPTED RESIDUAL (this grants NO working-tree / source-content write capability
//! and bypasses no file scope; the Stop-gate still reviews the final diff):
//! - a lexical check cannot prove the resolved binary's identity or a clean exec env (a
//!   shadowed `git`/`ls` on PATH, or a shell function/alias if the runner sources one);
//! - Git read-only porcelain may refresh `.git/index` or take optional locks — i.e.
//!   `.git`-METADATA-only side effects, never tracked/working-tree content, never a route
//!   to author source;
//! - a relative path operand could be a symlink pointing outside the repo (would need a
//!   pre-existing in-repo symlink, itself a prior gate-blocked write).

/// `git log`/`diff`/`show` flags that only affect read-only summary/selection. CONTENT-diff
/// flags (`-p`/`--patch`/`--word-diff`) are deliberately EXCLUDED — rendering file content can
/// invoke configured textconv/ext-diff/pager helpers (exec). Anything outside this set
/// (`--output`, `--ext-diff`, `-O`, content flags, unknown) is denied for those subcommands.
const GIT_LOG_DIFF_FLAGS: &[&str] = &[
    "--oneline",
    "--stat",
    "--shortstat",
    "--numstat",
    "--name-only",
    "--name-status",
    "--graph",
    "--decorate",
    "--abbrev-commit",
    "--max-count",
    "--cached",
    "--staged",
    "--no-color",
    "--color",
    "--",
];

/// Summary flags that make a `git diff`/`show` print file NAMES/STATS rather than content —
/// so no textconv/ext-diff helper runs. `diff`/`show` are allowed ONLY with one of these
/// present (a bare content `git diff`/`git show` is denied).
const GIT_SUMMARY_FLAGS: &[&str] =
    &["--stat", "--shortstat", "--numstat", "--name-only", "--name-status"];

/// Option flags that read an arbitrary FILE given as their value (separately or via `=`).
/// They can read outside the cwd (e.g. `wc --files0-from=/etc/passwd`,
/// `git ls-files --exclude-from=…`) so they disqualify the command entirely.
const OPTION_PATH_READERS: &[&str] = &[
    "--files0-from",
    "-files0-from",
    "--exclude-from",
    "--exclude-per-directory",
];

/// `find` predicates/actions that execute a command or mutate the filesystem. Their presence
/// disqualifies an otherwise read-only `find` traversal.
const FIND_MUTATING_ACTIONS: &[&str] = &[
    "-delete",
    "-exec",
    "-execdir",
    "-ok",
    "-okdir",
    "-fprint",
    "-fprintf",
    "-fls",
    "-fprint0",
];

/// True iff `command` is a proven read-only discovery command (see module docs). Fail-closed:
/// any ambiguity returns `false` (the gate keeps denying it pre-approval).
pub fn is_read_only(command: &str) -> bool {
    if !has_only_safe_chars(command) {
        return false;
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let Some(&first) = tokens.first() else {
        return false; // empty / whitespace-only
    };
    // GLOBAL denials (apply to EVERY command, incl. git operands):
    // - `--help` opens a man page / configured `web.browser` / pager → arbitrary exec.
    //   (`-h` only prints usage and is a legit human-readable flag for `ls`/`du`/…, kept.)
    // - a bare `-` operand reads stdin and would BLOCK the hook.
    // - an ABSOLUTE path operand escapes the repo (`cat /etc/passwd`, `ls C:/Users`, `find /`).
    // - any `..` token escapes the cwd (`git status ../secret`, `git diff --stat -- ../x`, a
    //   `main..dev` rev-range); rejecting it confines every operand under the cwd (fail-closed).
    if tokens.contains(&"--help")
        || tokens.contains(&"-")
        || tokens.iter().any(|t| is_absolute_path(t) || t.contains(".."))
    {
        return false;
    }
    // Option-embedded paths: a known file-reading option flag, or any flag whose `=value` is
    // absolute or escapes the cwd, can read outside the cwd → deny.
    if tokens.iter().any(|t| {
        let base = t.split('=').next().unwrap_or(t);
        OPTION_PATH_READERS.contains(&base)
            || matches!(t.split_once('='), Some((_, v)) if is_absolute_path(v) || v.contains(".."))
    }) {
        return false;
    }
    match first {
        "ls" | "dir" | "pwd" | "cat" | "head" | "tail" | "wc" => file_reader_ok(first, &tokens),
        "git" => git_read_only(&tokens),
        // `..` already rejected globally; only the mutating/exec actions remain to bar.
        "find" => !tokens.iter().any(|t| FIND_MUTATING_ACTIONS.contains(t)),
        _ => false,
    }
}

/// True iff `tok` is an absolute path: a leading `/` or `\`, or a `X:` drive prefix.
fn is_absolute_path(tok: &str) -> bool {
    let b = tok.as_bytes();
    b.first() == Some(&b'/')
        || b.first() == Some(&b'\\')
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// File-reading inspection (`ls`/`dir`/`pwd`/`cat`/`head`/`tail`/`wc`) — absolute and `..`
/// escapes already rejected globally — must not BLOCK: a
/// `tail -f`/`-F`/`--follow` streams forever, and `cat`/`head`/`tail`/`wc` with no FILE
/// operand read stdin and hang the pre-approval hook.
fn file_reader_ok(first: &str, tokens: &[&str]) -> bool {
    // (`..` escapes are already rejected globally.)
    if first == "tail" && tokens.iter().any(|t| tail_follows(t)) {
        return false;
    }
    // `ls`/`dir`/`pwd` list the cwd and never block; the file-content readers must name a file.
    if matches!(first, "cat" | "head" | "tail" | "wc") && !has_file_operand(&tokens[1..]) {
        return false;
    }
    true
}

/// A `tail` "follow" form that streams indefinitely: `-f`, `-F`, `--follow[=…]`, or a short
/// flag CLUSTER containing `f`/`F` (e.g. `-fn`, `-nF`).
fn tail_follows(tok: &str) -> bool {
    if tok == "-F" || tok.starts_with("--follow") {
        return true;
    }
    tok.starts_with('-') && !tok.starts_with("--") && tok.contains(['f', 'F'])
}

/// True iff some arg is a FILE operand: a non-dash token that is not a pure number (a bare
/// number is typically the value of a preceding count flag like `-n 5`, not a filename).
fn has_file_operand(args: &[&str]) -> bool {
    args.iter()
        .any(|t| !t.starts_with('-') && !t.bytes().all(|b| b.is_ascii_digit()))
}

/// Positive character allowlist: ASCII letters/digits, space, and `- _ . / = , :`. Any other
/// byte (quotes, `$ { } [ ] ? * ~ \\ ( ) < > | ; &` backtick, control chars …) rejects the
/// whole command, so no shell metasyntax can survive to the token logic.
fn has_only_safe_chars(s: &str) -> bool {
    !s.trim().is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || c == ' '
                || matches!(c, '-' | '_' | '.' | '/' | '=' | ',' | ':')
        })
}

/// Read-only `git` subcommands. A global option BEFORE the subcommand (`git -c k=v status`,
/// `git -C dir status`, `git --exec-path=… status`) makes `tokens[1]` a dash form → not a
/// recognized subcommand → `false` (fail-closed; blocks `-c core.pager=…` config-injection).
fn git_read_only(tokens: &[&str]) -> bool {
    let Some(&sub) = tokens.get(1) else {
        return false; // bare `git`
    };
    let args = &tokens[2..];
    match sub {
        // No exec/source-write flag exists for these (only the `.git`-metadata residual).
        "status" | "rev-parse" | "ls-files" => true,
        // `log` prints commit metadata; a content diff needs `-p`/`--patch`, which are NOT in
        // the allowed set → rejected by `all` (so no textconv/ext-diff surface).
        "log" => args.iter().all(|t| git_log_diff_arg_ok(t)),
        // `diff`/`show` render CONTENT (textconv/ext-diff/pager exec) unless restricted to a
        // summary form: require a summary flag AND only allowed flags.
        "diff" | "show" => {
            args.iter().all(|t| git_log_diff_arg_ok(t))
                && args.iter().any(|t| GIT_SUMMARY_FLAGS.contains(t))
        }
        _ => false,
    }
}

/// A `git log|diff|show` argument is OK iff it is a non-dash operand (path/revision — the
/// char-gate already barred metasyntax), a `-<digits>` count shorthand (`-20`, `-1`), or a
/// dash-flag whose base (before `=`) is in [`GIT_LOG_DIFF_FLAGS`].
fn git_log_diff_arg_ok(tok: &str) -> bool {
    if !tok.starts_with('-') {
        return true;
    }
    if tok.len() >= 2 && tok[1..].chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    let base = tok.split('=').next().unwrap_or(tok);
    GIT_LOG_DIFF_FLAGS.contains(&base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_read_only_discovery() {
        for c in [
            "git status",
            "git status --porcelain",
            "git status -s -b",
            "git rev-parse HEAD",
            "git rev-parse --show-toplevel",
            "git ls-files",
            "git log",
            "git log --oneline -20",
            "git log -1",
            "git log --stat",
            "git diff --stat",
            "git diff --cached --stat",
            "git diff --name-only",
            "git show --name-only",
            "git diff --stat -- src/main.rs",
            "ls -la",
            "ls -lh",
            "dir",
            "cat f.txt",
            "head f.txt",
            "tail -n 5 f.txt",
            "wc -l f.txt",
            "pwd",
            "find apps/web -maxdepth 3 -type d",
            "find . -name foo.rs",
        ] {
            assert!(is_read_only(c), "{c} should be read-only");
        }
    }

    #[test]
    fn denies_help_exec_vector() {
        for c in ["git status --help", "git log --help", "git --help", "find --help"] {
            assert!(!is_read_only(c), "{c} (--help) must be denied");
        }
    }

    #[test]
    fn denies_git_global_option_before_subcommand() {
        for c in ["git -c core.pager=x status", "git -C dir status", "git --exec-path=p status"] {
            assert!(!is_read_only(c), "{c} (pre-subcommand global option) must be denied");
        }
    }

    #[test]
    fn denies_git_write_and_exec_flags() {
        for c in [
            "git diff --output=f",
            "git show --ext-diff",
            "git log -O orderfile",
            "git log --not-a-flag",
        ] {
            assert!(!is_read_only(c), "{c} (disallowed flag) must be denied");
        }
    }

    #[test]
    fn denies_content_rendering_diffs() {
        // Content rendering can invoke textconv/ext-diff/pager helpers → deny; only summary
        // forms are allowed.
        for c in [
            "git diff",
            "git diff -p",
            "git diff --patch",
            "git diff --cached",
            "git show",
            "git show HEAD",
            "git log -p",
            "git log --patch",
        ] {
            assert!(!is_read_only(c), "{c} (content render) must be denied");
        }
    }

    #[test]
    fn denies_absolute_paths_and_escapes() {
        for c in [
            "cat /etc/passwd",
            "find / -maxdepth 2 -type f",
            "ls C:/Users",
            "cat C:/secret.txt",
            "ls ../secret",
            "cat ../../etc/passwd",
            "find .. -type f",
            "git diff --stat -- /etc/x",
            "cat \\\\server\\share",
            // `..` escapes for GIT operands too (status/ls-files/diff path + rev-range).
            "git status ../secret",
            "git ls-files ../secret",
            "git diff --stat -- ../secret",
            "git log main..dev",
        ] {
            assert!(!is_read_only(c), "{c} (absolute/escape path) must be denied");
        }
    }

    #[test]
    fn denies_option_embedded_path_reads() {
        for c in [
            "git ls-files --exclude-from=/etc/passwd",
            "git ls-files --exclude-from=patterns",
            "git ls-files --exclude-per-directory=.gitignore",
            "wc --files0-from=/etc/passwd",
            "wc --files0-from=files.txt",
            "wc --files0-from=../list",
            "git diff --stat --foo=/etc/x",
        ] {
            assert!(!is_read_only(c), "{c} (option-embedded path read) must be denied");
        }
    }

    #[test]
    fn requires_a_file_operand_for_content_readers() {
        // No file operand → reads stdin → would block the hook.
        for c in ["cat", "wc", "wc -l", "head -n 5", "tail -n 5", "cat -", "head -"] {
            assert!(!is_read_only(c), "{c} (stdin / no file operand) must be denied");
        }
        // With a file operand they are allowed.
        for c in ["cat f", "wc -l f", "head -n 5 f", "tail -n 5 f.log"] {
            assert!(is_read_only(c), "{c} (has file operand) should be allowed");
        }
    }

    #[test]
    fn denies_all_blocking_tail_follow_variants() {
        for c in [
            "tail -f app.log",
            "tail -F app.log",
            "tail --follow app.log",
            "tail --follow=name app.log",
            "tail -fn 5 app.log",
            "tail -nF app.log",
        ] {
            assert!(!is_read_only(c), "{c} (tail follow) must be denied");
        }
        // a non-follow tail with an operand is fine.
        assert!(is_read_only("tail -n 20 app.log"));
    }

    #[test]
    fn denies_mutating_git_subcommands() {
        for c in [
            "git branch -D x",
            "git branch newb",
            "git tag -a v1",
            "git config user.name Bob",
            "git remote add o url",
            "git remote set-url o u",
            "git push",
            "git commit -m x",
            "git checkout main",
            "git",
        ] {
            assert!(!is_read_only(c), "{c} (mutating/unknown git) must be denied");
        }
    }

    #[test]
    fn denies_find_mutating_actions() {
        for c in ["find . -delete", "find . -exec ls", "find . -execdir rm", "find . -fls out"] {
            assert!(!is_read_only(c), "{c} (find mutating action) must be denied");
        }
    }

    #[test]
    fn denies_non_allowlisted_commands() {
        for c in [
            "rm -rf x",
            "python script.py",
            "node app.js",
            "sed -i s f",
            "cargo build",
            "npm install",
            "date -s 2026-01-01",
            "hostname newname",
            "tree -o out",
            "echo hi",
            "mv a b",
        ] {
            assert!(!is_read_only(c), "{c} (not allowlisted) must be denied");
        }
    }

    #[test]
    fn char_gate_rejects_shell_metasyntax() {
        for c in [
            "git status; rm -rf x",
            "ls > out",
            "cat $(x)",
            "ls `whoami`",
            "git log HEAD~1",
            "ls *.rs",
            "cat ~/x",
            "git status && rm x",
            "git diff | head",
            "cat 'quoted'",
            "find . -exec rm {} +",
            "",
            "   ",
        ] {
            assert!(!is_read_only(c), "{c:?} (shell metasyntax) must be denied by the char gate");
        }
    }
}
