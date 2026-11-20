# Contributing to TimelapseMaker

Köszönöm, hogy hozzá szeretnél járulni! This document describes the workflow
for submitting changes.

## Ground rules

- Be respectful and constructive in issues, pull requests and discussions.
- Search [existing issues](https://github.com/vamtic/makerr/issues) and
  [pull requests](https://github.com/vamtic/makerr/pulls) before opening a new one.
- Use [Discussions](https://github.com/vamtic/makerr/discussions) for questions
  and open-ended ideas; use Issues for actionable bugs and feature requests.

## Reporting a bug

Use the **Bug report** issue form and include:

- the version/commit you are running,
- your OS and Rust version,
- minimal reproduction steps,
- the console log (GUI log panel or CLI stdout/stderr),
- whether `ffmpeg` is on `PATH`.

For security-sensitive problems, follow [SECURITY.md](SECURITY.md) instead of
opening a public issue.

## Suggesting a feature

Use the **Feature request** issue form. Describe the problem, the proposed
solution and any alternatives you considered. Larger ideas are best discussed
in GitHub Discussions first.

## Development setup

```sh
git clone https://github.com/vamtic/makerr.git
cd makerr
cargo build --workspace
cargo test --workspace
```

Requirements: stable Rust, `ffmpeg` at runtime, and on Linux the GL/X11/Wayland
dev packages listed in the [README](README.md#installation).

For GUI work with the Slint MCP server, use `./slintmcpstart.sh`.

## Making a change

1. Fork the repository and create a topic branch from `master`.
2. Keep commits focused; use conventional prefixes (`feat:`, `fix:`, `perf:`,
   `docs:`, `refactor:`, `test:`, `chore:`).
3. Before opening a pull request, make sure all checks pass locally:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets
   cargo test --workspace
   cargo build --release --workspace
   ```

4. Fill in the pull request template: describe the change, link the related
   issue (`Closes #123`), and note how you tested it.

## Pull request checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets` passes without new warnings
- [ ] `cargo test --workspace` passes
- [ ] New behaviour is covered by tests where practical
- [ ] README/docs updated if the change affects usage or configuration
- [ ] No generated files (`target/`, caches, `Cargo.lock` churn unless intentional) committed

CI runs formatting on every push and PR, clippy and the test suite on Linux,
and a Windows `cargo check` for the `cfg(windows)` code paths. Every push to
`master` also builds the release artifacts (Windows installer and portable zip,
Linux tarball and zip) in parallel with those checks, and publishes them to the
`latest` release once everything has passed.

## Compatibility notes

The project keeps V1's `settings.json` and template JSON readable. When touching
those formats, the tile cache or snapshot semantics, preserve the existing
formats and document any deliberate migration in the PR description.

The vendored `image_parity` crate under `crates/tlm-core/vendor/` is a test-only
copy of the exact `image` version in use. If you bump `image`, keep it in sync.

## License

By contributing you agree that your contributions are licensed under the
[MIT License](LICENSE).
