### What's changed in v0.23.0

* fix(install): produce single-manifest images by disabling buildx attestations (by @patrickleet)

  Modern Docker buildx (default since 23.x) wraps single-arch outputs in an
  OCI manifest list with an attestation entry. Crossplane's package fetcher
  uses go-containerregistry's `remote.Image` with no platform hint, which
  defaults to linux/amd64 when navigating an index — so an arm64-only list
  from a Mac dev build fails with "no child with platform linux/amd64".

  Passing `--provenance=false --sbom=false` makes buildx emit a single
  manifest with no manifest-list wrapper, which Crossplane fetches directly
  regardless of architecture. Applies to both `build_patched_configuration_image`
  (the configuration-image patcher) and `docker_build_from` (the render
  function rebuild).

  This bug surfaced after a recent local-build refactor; pre-Docker-23
  classic builder produced single manifests by default, so this was a
  silent regression as developer Docker installs caught up.

* feat(provider): preserve upstream URL in spec.package; auto-incrementing dev tag; --branch flag (by @patrickleet)

  Three related changes to make `hops provider install --path/--repo` work
  end-to-end with Crossplane's dependency manager and to support forked
  provider branches.

  1. Preserve the upstream URL in spec.package
  -------------------------------------------

  Crossplane's dep manager (v2.2.1) parses the Provider CR's spec.package
  into a Lock entry via `ParsePackageSourceFromReference` — yielding the
  URL prefix (no tag). Dependency declarations are matched against the
  Lock by exact string. ImageConfig's `rewriteImage` only affects fetching,
  not Lock matching, so patching spec.package to the local-registry URL
  breaks dependency resolution for any Configuration that declares
  `xpkg.crossplane.io/.../provider-foo`.

  `apply_provider_resources` now keeps the upstream URL prefix in
  spec.package and only swaps the tag for the local dev tag. The Provider
  controller continues to fetch from the local registry via the paired
  ImageConfig (composed by the install).

  `ResolvedProvider.existing_package` carries the full source ref;
  `recover_upstream_url_prefix` extracts the URL prefix on first install
  or recovers it from a pre-existing ImageConfig when re-running over an
  already-patched Provider.

  2. Auto-incrementing vN.999.<patch> dev tag
  -------------------------------------------

  Previous `--version-prefix v1` produced `v1-dev-<sha>` — not valid SemVer
  and not satisfying `>=v1` constraints. The new scheme reads the upstream
  Provider's tag (e.g. `v1.2.0`) to derive the major version, then queries
  the local registry's tags-list API for existing `v<MAJOR>.999.<N>` tags
  and increments past the highest one. Stable SemVer, monotonically newer
  each push, satisfies `>=vN` constraints in Masterminds/semver (which
  excludes prereleases by default). Falls back to `vN.999.999-dev-<sha>`
  when the registry can't be queried.

  3. --branch flag for source builds
  ----------------------------------

  `ensure_cached_repo_checkout` gains a sibling `_at` variant that accepts
  an optional branch. Initial clone uses `git clone --branch`; refresh does
  `fetch + checkout + reset --hard origin/<branch>`. Provider install
  surfaces this via `--branch` (requires `--repo`, conflicts with
  `--version`) so forks like `jonasz-lasut/provider-helm` on `helm-v4` can
  be installed directly without manually preparing a checkout.


See full diff: [v0.22.0...v0.23.0](https://github.com/hops-ops/hops-cli/compare/v0.22.0...v0.23.0)
