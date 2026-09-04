### What's changed in v0.48.0

* feat: Make local GitOps restarts and Vault bootstrap reusable (#107) (by @patrickleet)

  * fix(local): derive environments from worktrees [[deleted-local-cluster-cannot-restart-from-clean-st]]

  * feat(local): complete GitOps restart prerequisites [[tasks/hops-cli-local-gitops-prerequisites]]

  * fix(local): make PID liveness portable [[tasks/hops-cli-local-gitops-prerequisites]]

  * test(local): avoid stalled lock race failures [[tasks/hops-cli-local-gitops-prerequisites]]

  * ci: make kind backend smoke opt-in [[tasks/hops-cli-local-gitops-prerequisites]]

  * fix(local): publish controller leases atomically [[tasks/hops-cli-local-gitops-prerequisites]]

  * test(ci): preserve opt-in kind smoke [[tasks/hops-cli-local-gitops-prerequisites]]

  * fix: address local GitOps review findings [[tasks/hops-cli-local-gitops-prerequisites]]


See full diff: [v0.47.0...v0.48.0](https://github.com/hops-ops/hops-cli/compare/v0.47.0...v0.48.0)
