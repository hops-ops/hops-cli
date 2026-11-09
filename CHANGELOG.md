### What's changed in v0.55.1

* fix(local): preserve source mounts through probe failures (#166) (by @patrickleet)

  * fix(local): preserve source mounts through probe failures

  Defer inconclusive observations, guard accepted host mounts, and initialize source sync before accepting delivery state.

  [[tasks/fix-source-delivery-fallback-mount-loss]]

  * test(local): drain manifest input in the kubectl fixture

  [[tasks/fix-source-delivery-fallback-mount-loss]]

  * fix(local): inspect full watcher command during cleanup

  Use unlimited ps width so Linux COLUMNS cannot hide the ownership marker. Exercise a real tar watcher under narrow columns in an isolated subprocess, retaining unrelated-PID protection.

  Refs [[tasks/fix-source-delivery-fallback-mount-loss]]


See full diff: [v0.55.0...v0.55.1](https://github.com/hops-ops/hops-cli/compare/v0.55.0...v0.55.1)
