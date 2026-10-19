# Contributing to Handoff

I maintain Handoff alone. For anything bigger than a small fix, open an issue first so we can agree on the approach before you spend time on it.

## Setting up

You need 64-bit Windows 10 or 11, Rust from rustup with the Visual Studio C++ build tools, PowerShell and Git. `rust-toolchain.toml` picks the Rust version for you.

```powershell
git clone https://github.com/RealWhyKnot/Handoff
cd Handoff
git config core.hooksPath .githooks
cargo build --release
```

## Pull requests

1. Fork and branch off `main`.
2. Keep the change small. A cleanup you noticed on the way goes in its own PR.
3. Run `./scripts/lint.ps1`. It runs `cargo fmt --check`, `cargo clippy` with warnings as errors and `cargo test`, the same checks CI runs.
4. Open the PR and fill in the template.

Commit subjects look like `type(scope): summary`, where `type` is one of `feat`, `fix`, `chore`, `ci`, `docs`, `refactor`, `revert`, `test`, `perf`, `diag` or `style`. The commit-msg hook adds the version stamp at the end.

## New commands

A command is a capability in `src/capabilities/` plus its spec in `src/capabilities/specs/`. Anything that changes the PC has to ask the host first, and `--consent deny` has to refuse it.
