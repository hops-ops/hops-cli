### What's changed in v0.52.1

* chore(deps): update railwayapp/railpack to v0.40.0 (by @renovate[bot])

  Import template RAILPACK_VERSION pin only. 0.40.0 fixes Bun store/PHP apt/planner excludes — no workflow with: change. quality green; smokes skipped as usual.

* fix(local): wait for first boot and clean up disabled Environment namespaces (#130) (by @patrickleet)

  * fix(local): wait for fresh Cluster readiness

  Refs [[incidents/local-up-first-boot-readiness]]

  * fix(local): delete exclusively owned Environment namespaces

  Refs [[incidents/local-env-disable-retains-namespace]]

  * test(local): mock Crossplane package inventory in cluster contract [[incidents/local-up-first-boot-readiness]]


See full diff: [v0.52.0...v0.52.1](https://github.com/hops-ops/hops-cli/compare/v0.52.0...v0.52.1)
