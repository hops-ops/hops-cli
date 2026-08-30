### What's changed in v0.44.0

* feat(local): support explicit GitOps renderers (#102) (by @patrickleet)

  * feat: reconcile local gitops clusters

  Implement the Cluster-owned local GitOps seed, inventory, Environment reconcile, and exact cleanup paths while preserving standalone local start installers.\n\nImplements [[tasks/lwb-cluster-environment-definition]] [[tasks/lwb-gitops-bootstrap-ownership]] [[tasks/lwb-cluster-controller]]

  * feat(local): support explicit GitOps renderers

  * fix: address local gitops review comments

  * refactor: remove legacy local application path

  * docs: update local gitops manifest path


See full diff: [v0.43.2...v0.44.0](https://github.com/hops-ops/hops-cli/compare/v0.43.2...v0.44.0)
