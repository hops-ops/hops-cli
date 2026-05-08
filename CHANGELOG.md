### What's changed in v0.21.0

* chore(deps): update rust crate uuid to v1.23.1 (#41) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate clap to v4.6.1 (#40) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate openssl-sys to v0.9.114 (#39) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate tokio to v1.52.1 (#37) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* feat(vars): declarative sync of non-secret config to GitHub Actions (by @patrickleet)

  Parallel of `hops secrets` minus the SOPS round-trip — values are
  cleartext and committed to git. `hops vars sync github` shells out to
  `gh variable set`. Subcommands: init, list, sync. Layout mirrors
  secrets/github: `_shared/` per-key files synced to every repo in
  `vars.github.shared.repos`, plus optional `<repo>/` overrides.


See full diff: [v0.20.0...v0.21.0](https://github.com/hops-ops/hops-cli/compare/v0.20.0...v0.21.0)
