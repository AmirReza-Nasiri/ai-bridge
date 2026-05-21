# MAINTAINERS — ai-bridge

Cross-platform discipline is the highest priority after correctness. Platform
ownership mirrors the codex-peer pattern: each maintainer owns one side, and the
shared trait/logic needs both-platform verification.

## Platform ownership

- **Windows (AmirReza):** `crates/aibridge-platform/src/windows.rs`,
  Windows-specific tests, `docs/install/windows.md`.
- **macOS (Mo):** `crates/aibridge-platform/src/unix.rs`, macOS-specific tests,
  `docs/install/macos.md`.
- **Shared (both review):** `crates/aibridge-platform/src/lib.rs` (the `Platform`
  trait), `crates/aibridge-core/`, `crates/aibridge/`, `adapters/*`, CI workflows.

The rtk-wiring + review-gate core is one shared, cross-platform code path; the
only genuinely platform-divergent code is executable discovery, install dir, PATH
mutation, and config-path handling — all confined to `aibridge-platform`.

## Release checklist

Before any version bump:

1. CI green: native (Windows, macOS Apple Silicon, Linux) + the Intel
   cross-compile check (`x86_64-apple-darwin` built/linted on the Apple Silicon
   runner). **Plus a manual runtime smoke on real Intel macOS** when one is
   available — CI only proves the Intel build/ABI, not Intel runtime.
2. `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.
3. `cargo test --workspace` green.
4. `aibridge selftest` green on both Windows and macOS.
5. CHANGELOG.md updated.
6. Tag on `main`.
