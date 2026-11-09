### What's changed in v0.54.1

* chore(deps): update rust crate uuid to v1.29.1 (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate notify to v8.2.2 (#163) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update registry docker tag to v3 (#127) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* chore(deps): update rust crate uuid to v1.29.2 (#164) (by @renovate[bot])

  Co-authored-by: renovate[bot] <29139614+renovate[bot]@users.noreply.github.com>

* fix(local): address orbstack provider review feedback (#161) (by @patrickleet)

  fix(local): address orbstack provider review feedback

  Follow-ups to #159 from review:
  - Restore the kube current-context even when the OrbStack start or
    engine restart times out.
  - Drop the docker CLI preflight and the socket check at activation, so
    a fresh install can reach `orb start`.
  - A Docker-only provider flag keeps the recorded cluster provider.
  - Apply the k3s CNI paths and HTTPS listener after manifest overlays;
    the listener is added once.
  - Refresh Gateway TLS after Environments reconcile, and wait for the
    istio-ingress namespace before applying the Secret.
  - Harden the local CA: name constraints (localhost, 127.0.0.0/8) on new
    CAs, a 0700 TLS directory, reissue when the leaf no longer chains to
    the CA, and drop the old leaf before issuing a new one.

  [[tasks/orbstack-cluster-provider]]

  fix(local): harden orbstack patches and TLS waits

  Second round of review feedback on the orbstack follow-up:
  - Log the restore failure when both the operation and the kube
    context restore fail.
  - Patch the IstioStack inside multi-document istio.yaml overrides and
    merge the k3s CNI paths into spec.cni.values instead of replacing
    spec.cni.
  - Write the managed marker before overlays and patches, so a failed
    materialization stays retryable.
  - Only startup and Environment enable wait for istio-ingress; the
    watch loop retries next pass. Report kubectl's error on timeout.

  [[tasks/orbstack-cluster-provider]]


See full diff: [v0.54.0...v0.54.1](https://github.com/hops-ops/hops-cli/compare/v0.54.0...v0.54.1)
