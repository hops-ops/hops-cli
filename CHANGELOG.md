### What's changed in v0.53.2

* chore: change localDomain to 'hops.localhost' (by @patrickleet)

  Updated localDomain to 'hops.localhost' in configuration and examples.

* chore(deps): update rust crate libc to v0.2.190 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate uuid to v1.27.0 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate tokio to v1.53.2 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>
  Co-authored-by: Patrick Lee Scott <pat@patscott.io>

* chore(deps): update local-start-crossplane-bootstrap (by @renovate[bot])

  TEST - should fail if we cancel... wait we should NOT actually merge this without review complete

* chore(deps): update dependency crossplane-contrib/provider-kubernetes to v1.4.1 (#135) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate uuid to v1.28.0 (#136) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate serde_json to v1.0.152 (#137) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate serde_json to v1.0.154 (by @renovate[bot])

  Usage-reviewed: Cargo.lock only. Patch notes (UTF-8 error locus, no_std preserve_order) do not affect hops-cli's json!/from_str/to_string usage. quality CI green; smoke workflows label-gated/skipped.

* chore(deps): update rust crate tokio to v1.53.3 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update dependency railwayapp/railpack to v0.41.0 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>
  Co-authored-by: Patrick Lee Scott <pat@patscott.io>

* chore(deps): update dependency railwayapp/railpack to v0.42.0 (#142) (by @renovate[bot])

  hops import publish-image-railpack template pin only. 0.42.0 adds turbo cache/Zig support and Node fixes; release asset with sha256 digest present, matching the template's verify step. quality green.

* chore(deps): update dependency crossplane-contrib/provider-helm to v1.5.0 (#141) (by @renovate[bot])

  hops local start bootstrap pin. v1.5.0 moves chart cache to /tmp/content-cache (local-dev-helm DRC has no read-only rootfs), reworks takeOwnership/OCI URL handling (no hops-ops Release uses takeOwnership or chart.url). kind-smoke (test-kind) green: provider Healthy, doctor, config install. quality green. aws-crossplane-stack already runs v1.5.0 with e2e green.

* chore(deps): update rust crate libc to v0.2.191 (#143) (by @renovate[bot])

  Cargo.lock-only bump. hops-cli uses libc on unix only (fstatat/AT_SYMLINK_NOFOLLOW, mkdirat, kill/SIGKILL, flock/LOCK_UN); 0.2.191 is additive for Linux/macOS (Apple F_*NOSIGPIPE, ifaliasreq; Mach API re-exposed on non-macOS Apple). quality green; smoke jobs label-gated (skipped).

* chore(deps): update dependency railwayapp/railpack to v0.43.0 (#144) (by @renovate[bot])

  Pin in the hops import publish-image-railpack workflow template only. The template downloads the release asset and checks its SHA-256 digest.
  0.43.0 'breaking': default excludes for node_modules and venv when no .dockerignore exists. No effect in the generated workflow because it builds from a fresh actions/checkout. Ruby lockfile fallback is now only a deprecation warning.
  quality green. Smokes are skipped on PRs and do not exercise the template.

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate tokio to v1.54.0 (#145) (by @renovate[bot])

  hops-cli only uses tokio::runtime::Runtime::new() + block_on for rusoto (secrets list/sync). The 1.54 behavior changes (zero-capacity panics, process/mio pipes, timer internals) do not touch that path. MSRV is now 1.85, and CI stable builds it.
  The lockfile also re-resolved windows-sys for Windows-only deps (errno/rustix/tempfile/socket2 0.5/winapi-util 0.59->0.52, socket2 0.6 0.61->0.60), all within those crates' declared ranges. I verified locally with cargo check --locked --target x86_64-pc-windows-msvc: all of these crates compiled; only ring's C build failed, because the box has no MSVC.
  quality green. The Windows check is opt-in by label and did not run.

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update swatinem/rust-cache digest to d1527c8 (#146) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* fix(local): delete Crossplane resources before env namespace (#148) (by @patrickleet)

  * fix(local): delete Crossplane resources before env namespace

  A terminating namespace rejects new objects, so a managed resource
  still finalizing there cannot recreate its ProviderConfigUsage and
  blocks the namespace forever. Delete and wait for every composite and
  managed resource in the owned namespace first.

  * test(kind): retry exec of just-written proxy scripts on ETXTBSY

  A concurrently forked test child can briefly hold the script's write
  fd between its fork and exec, so exec fails with Text file busy.


See full diff: [v0.53.1...v0.53.2](https://github.com/hops-ops/hops-cli/compare/v0.53.1...v0.53.2)
