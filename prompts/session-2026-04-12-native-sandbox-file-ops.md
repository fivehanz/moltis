## Session Summary

- Added native host-backed `read_file`, `write_file`, and `list_files` helpers for sandbox backends whose file paths already resolve on the host.
- Wired `NoSandbox` and `RestrictedHostSandbox` to use those native file operations instead of shelling back through the command bridge.
- Hardened the native write path with atomic temp-file persistence, parent-directory validation, and typed symlink rejection payloads.
- Added backend-level tests covering native read, write, list, and symlink rejection for both `NoSandbox` and `RestrictedHostSandbox`.

## Validation

- `cargo +nightly-2025-11-30 fmt --all -- --check`
- `cargo test -p moltis-tools no_sandbox_`
- `cargo test -p moltis-tools restricted_host_sandbox_`
- `cargo clippy -p moltis-tools --tests`
- `just lint` (fails in unrelated `llama-cpp-sys-2` build path in this environment)

## Notes

- `just lint` still fails outside the touched crate because `llama-cpp-sys-2` hits a broken CMake build path on this macOS environment.
- `bd dolt push` still fails because no Dolt remote is configured for this worktree.
