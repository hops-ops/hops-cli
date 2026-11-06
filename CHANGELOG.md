### What's changed in v0.54.0

* chore(deps): update rust crate ureq to v3.5.0 (#147) (by @renovate[bot])

  Cargo.lock only. Pulls rustls 0.23.46 (GHSA-2mjx-qc3c-rqvc) and webpki-roots 1.0.10 (eMudhra distrust). hops-cli uses ureq for vault health/lookup-self GETs and the package_dev registry agent (content-digest headers); no proxy/SOCKS config, so 3.5.0 proxy/timeout-budget/EINTR changes don't affect usage. windows-sys re-resolution is Windows-only and in declared ranges. quality green; smokes label-gated (skipped).

* chore(deps): update rust crate tokio to v1.54.1 (#149) (by @renovate[bot])

  Cargo.lock only. 1.54.1 reverts the timer runtime-handle change from 1.54.0 (#8692). hops-cli uses tokio only via Runtime::new()+block_on for rusoto secrets paths. quality green; smokes label-gated (skipped).

* chore(deps): update rust crate uuid to v1.29.0 (#150) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update docker/login-action digest to 81c6f93 (#151) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate log to v0.4.35 (#153) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate openssl-sys to v0.9.118 (#154) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update unbounded-tech/workflow-vnext-tag action to v1.22.4 (#157) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update unbounded-tech/workflow-simple-release action to v2.1.4 (#156) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update docker/login-action digest to 8aa5831 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate ureq to v3.5.1 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate flate2 to v1.1.11 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate notify to v8.2.1 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>
  Co-authored-by: Patrick Lee Scott <pat@patscott.io>

* feat(local): add orbstack cluster provider (#159) (by @patrickleet)

  feat(local): add orbstack cluster provider

  OrbStack's built-in k3s as `--cluster-provider orbstack` (implies
  `--docker-provider orbstack`): `orb start k8s` / `orb stop k8s` /
  `orb delete k8s` for lifecycle, context `orbstack`, and the user's
  kubeconfig current-context restored after OrbStack switches it.

  k3s runs on OrbStack's dockerd, which resolves *.svc.cluster.local, so
  packages push to and pull from the in-cluster registry Service name; the
  only wiring is an insecure-registries entry in OrbStack's docker.json.

  `hops local up` now records the provider pair on the machine record and
  writes it into the Cluster document instead of hard-coding kind/dory;
  for orbstack it points istio-cni at the k3s CNI paths for ambient.

  feat(local): serve orbstack gateway on host :80/:443 with local CA

  OrbStack binds LoadBalancer Services on the Mac, so the orbstack
  template makes the shared Gateway a LoadBalancer with http 80 and an
  https 443 listener terminating platform-tls. hops keeps a local CA in
  ~/.hops/local/tls and reissues the Gateway leaf with every .localhost
  HTTPRoute hostname (browsers reject *.localhost wildcards).

  The auth template stops pinning sectionName http so Zitadel
  (externalSecure, port 443) attaches to HTTPS where it exists. Kind/Dory
  keeps one HTTP listener, so nothing changes there.

  Refs [[tasks/orbstack-cluster-provider]]

  fix(local): deploy environments to the machine cluster

  `hops local env enable` resolved the project's sibling cluster.yaml,
  so a project still declaring kind/dory sent its Environment to the
  retired kind-hops context. The machine record's Cluster now wins; the
  sibling definition is only a fallback before any machine exists.

  Refs [[tasks/orbstack-cluster-provider]]


See full diff: [v0.53.2...v0.54.0](https://github.com/hops-ops/hops-cli/compare/v0.53.2...v0.54.0)
