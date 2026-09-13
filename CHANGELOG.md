### What's changed in v0.51.1

* chore(deps): update unbounded-tech/workflow-vnext-tag action to v1.22.3 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate uuid to v1.26.1 (by @renovate[bot])

  Cargo.lock uuid 1.23.3→1.26.1; quality green; smokes skipped as expected.

* chore(deps): update rust crate serde_json to v1.0.151 (by @renovate[bot])

  Cargo.lock serde_json patch; quality green.

* chore(deps): update rust crate serde to v1.0.229 (by @renovate[bot])

  Cargo.lock serde patch; quality green; syn3 via serde_derive only.

* chore(deps): pin dependencies (#114) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate libc to v0.2.189 (by @renovate[bot])

  Cargo.lock libc patch after rebase; quality green.

* chore(deps): update rust crate clap to v4.6.6 (by @renovate[bot])

  Cargo.lock clap patch after rebase; quality green.

* chore(deps): update rust crate flate2 to v1.1.10 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update dependency railwayapp/railpack to v0.39.0 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate log to v0.4.34 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>
  Co-authored-by: Patrick Lee Scott <pat@patscott.io>

* chore(deps): update hops-ops/workflows-gitops action to v2.1.0 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>
  Co-authored-by: Patrick Lee Scott <pat@patscott.io>

* chore(deps): update actions/checkout action to v7 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* fix(deps): adopt ureq v3 API (supersedes Renovate #120) (by @patrickleet)

  * fix(deps): update rust crate ureq to v3

  * fix(deps): adopt ureq v3 API in vault secrets transport

  Migrate hops secrets sync vault HTTP calls from ureq 2 (.timeout/.set/Error::Status/into_json) to ureq 3 (config timeouts, header, http_status_as_error(false), body_mut().read_json).

  Enables Renovate #120 (ureq 2.12 -> 3.x) to pass quality.

  * fix(deps): adopt ureq v3 API in vault secrets transport

  Migrate hops secrets sync vault HTTP calls from ureq 2 (.timeout/.set/Error::Status/into_json) to ureq 3 (config timeouts, header, http_status_as_error(false), body_mut().read_json).

  Enables Renovate #120 (ureq 2.12 -> 3.x) to pass quality.

  ---------

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>


See full diff: [v0.51.0...v0.51.1](https://github.com/hops-ops/hops-cli/compare/v0.51.0...v0.51.1)
