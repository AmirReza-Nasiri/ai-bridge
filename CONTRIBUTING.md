# Contributing to AI Bridge

Thank you for helping improve AI Bridge. Small, focused changes with a clear
reproduction or use case are easiest to review.

## Before opening a change

1. Search existing issues and pull requests.
2. For behavior or architecture changes, open an issue first so the intended
   safety boundary and cross-platform impact are clear.
3. Do not include API keys, authentication files, private prompts, customer code
   or `.ai-bridge/` runtime state in an issue or commit.

## Development checks

Use stable Rust with `rustfmt` and `clippy`:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --release --workspace
```

Windows and macOS are supported release targets. Changes to shared behavior
should account for both platforms; platform-specific logic belongs in
`aibridge-platform`.

## Pull requests

- Keep each pull request focused on one outcome.
- Explain the problem, approach, tests and any behavior intentionally left out.
- Add or update tests for behavior changes.
- Update documentation and `CHANGELOG.md` when users will notice the change.
- Keep commits understandable; maintainers may squash during merge when useful.

By contributing, you agree that your contribution is licensed under the
repository's MIT License.
