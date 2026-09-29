### What's changed in v0.53.1

* chore(deps): update dependency railwayapp/railpack to v0.40.1 (#132) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* fix(local): converge auth routes and preserve package overrides (#131) (by @patrickleet)

  * fix(local): converge auth routes and preserve package overrides

  Reconcile asynchronous ingress, pass Environment identity to setup hooks, and qualify resource groups during cleanup.

  [[tasks/harmony-1847]]

  * fix(local): bound watcher waits and isolate ingress failures

  Address PR #131 review findings. Regression tests cover failed targets, debounce bounds, quiet periods, and disconnected watchers. [[tasks/harmony-1847]]

  * test(local): exercise a nonzero debounce ceiling

  [[tasks/harmony-1847]]


See full diff: [v0.53.0...v0.53.1](https://github.com/hops-ops/hops-cli/compare/v0.53.0...v0.53.1)
