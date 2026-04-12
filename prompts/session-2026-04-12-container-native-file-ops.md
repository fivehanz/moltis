## Session Summary

- Added backend-aware container file access so the fs tools keep one contract while sandbox backends choose the transport.
- Docker and Podman now resolve known bind-mounted guest paths back to the host and use OCI `cp` for unmapped container reads, writes, and listings.
- Apple Container now resolves mounted home-persistence paths directly on the host and falls back to in-container commands only when the CLI does not expose a native file-copy primitive.
- Added coverage for guest-to-host mount resolution plus mounted-path read, write, and list behavior for Docker and Apple Container.

## Validation

- `cargo +nightly-2025-11-30 fmt --all -- --check`
- `cargo test -p moltis-tools docker_`
- `cargo test -p moltis-tools resolve_home_persistence_guest_path_on_host`
- `cargo test -p moltis-tools resolve_workspace_guest_path_on_host`
- `cargo test -p moltis-tools apple_container_home_`
- `cargo clippy -p moltis-tools --tests`

## Notes

- OCI container writes still use CLI `cp` semantics for unmapped container-root paths, with preflight checks to reject symlink targets and missing parents before copy-in.
- Apple Container still lacks an obvious copy primitive in the CLI surface, so unmapped paths continue to use backend-local command fallback.
- `bd dolt push` still fails because no Dolt remote is configured for this worktree.
