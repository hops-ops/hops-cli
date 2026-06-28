### What's changed in v0.37.0

* feat(local): make registry volume persist across computer restarts with PVC (#69) (by @patrickleet)

  The local in-cluster OCI registry (used for dev-* builds and provider snapshots) now uses a PersistentVolumeClaim.

  This ensures /var/lib/registry data lives on the colima VM disk (via local-path), surviving host reboots and colima restarts.

  This is the primary solution for the "registry wipe on restart" problem.

  See updated spec [[specs/hops-cli-local-registry-recover]] and task for details. Recover command remains as safety net for full resets.


See full diff: [v0.36.0...v0.37.0](https://github.com/hops-ops/hops-cli/compare/v0.36.0...v0.37.0)
