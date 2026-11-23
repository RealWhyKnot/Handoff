# Contributing to Handoff

I built Handoff for my own use first and I maintain it alone. I'm picky about how the code is structured, how it's styled and how features fit together. If you hit a bug or want a change, please open an [issue](https://github.com/RealWhyKnot/Handoff/issues/new/choose) instead of sending a surprise pull request. A description of the problem, the steps to reproduce it or what you're trying to do is more useful to me than an unannounced PR.

If you'd like to help maintain Handoff over time, especially if I'm committing to it less often, or you have a substantial change you want to build, open an issue or reach out first. I'm happy to talk through the approach and agree on code style and commit conventions before you write any code.

## If we agree on a PR

Link the issue where we settled the approach in the PR description.

### Setting up

You need 64-bit Windows 10 or 11, Rust from rustup with the Visual Studio C++ build tools, PowerShell and Git. `rust-toolchain.toml` picks the Rust version for you.

```powershell
git clone https://github.com/RealWhyKnot/Handoff
cd Handoff
git config core.hooksPath .githooks
cargo build --release
```

### Checks

Keep the change small. A cleanup you noticed on the way goes in its own PR.

Run `./.github/lint/lint.ps1`. It runs the style check, `cargo fmt --check`, `cargo clippy` with warnings as errors, `cargo test` and the PowerShell and shell lint, the same checks CI runs. On Windows the shell lint runs in WSL: set `LINT_WSL_DISTRO` to a distro that has shellcheck and shfmt, or it is skipped.

### Commits

Commit subjects look like `type(scope): summary`, where `type` is one of `feat`, `fix`, `chore`, `ci`, `docs`, `refactor`, `revert`, `test`, `perf`, `diag` or `style`. The prepare-commit-msg hook adds the version stamp at the end, and the commit-msg hook checks the format.

### New commands

A command is a capability in `src/capabilities/` plus its spec in `src/capabilities/specs/`. Anything that changes the PC has to ask the host first, and `--consent deny` has to refuse it.
