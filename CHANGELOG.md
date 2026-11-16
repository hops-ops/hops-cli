### What's changed in v0.56.2

* fix(provider): install new providers on a GitOps-owned dev cluster (#170) (by @patrickleet)

  fix(provider): install new providers on a GitOps-owned dev cluster

  hops provider install refused whenever a controller.lock existed for the
  Cluster, even when its pid was no longer running, and refused to create a
  Provider that did not exist yet. Local development of a new provider was
  impossible on any Cluster that had ever run hops local gitops cluster.

  - A controller lock whose pid is not running owns nothing. The imperative
    guards now check liveness the same way acquire_controller does; the lock
    is only read, never removed or adopted.
  - provider install refuses only a Provider in a live controller's inventory:
    the controller prunes only what it recorded, and re-applies (reverts) only
    what it owns. A live controller with no inventory fails closed.
  - A source build (--path, or --repo without --version) that matches no
    existing Provider creates one whose spec.package names the local registry.
    Published installs (--repo --version) still require an existing Provider.

  config install and local aws keep the whole-Cluster owner check; they gain
  only the stale-lock fix.


See full diff: [v0.56.1...v0.56.2](https://github.com/hops-ops/hops-cli/compare/v0.56.1...v0.56.2)
