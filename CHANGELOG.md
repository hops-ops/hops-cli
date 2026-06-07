### What's changed in v0.32.0

* chore(deps): update rust crate log to v0.4.32 (#50) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate uuid to v1.23.2 (#52) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* feat: mount distributed_cli for hops service (drop local adapter) (#56) (by @patrickleet)

  * feat: mount distributed_cli for `hops service` instead of a local adapter

  `hops service` now re-exports distributed_cli's command surface rather than
  carrying its own scaffold/describe/schema adapter: the Service variant holds
  distributed_cli::ServiceArgs and dispatches with distributed_cli::run. Deletes
  src/commands/service (the former ~930-line adapter) and swaps the dependency from
  distributed_tooling to distributed_cli.

  This makes hops a thin, optional front-end: new flags/commands added in
  distributed_cli (e.g. `schema --format atlas`) reach `hops service` on a plain
  cargo update, with no code changes here.

  Temporary git dep on the distributed branch until distributed_cli is published;
  swap to a registry version once distributed PR #74 releases.

  * chore: depend on published distributed_cli 1.6 from crates.io

  distributed PR #74 merged and released distributed_cli 1.6.x, so replace the
  temporary git dependency with the registry version. No git source or branch
  tracking; `hops service` resolves the command surface from the published crate.

  Closes #58


See full diff: [v0.31.0...v0.32.0](https://github.com/hops-ops/hops-cli/compare/v0.31.0...v0.32.0)
