# Crossplane development registry

One Distribution chart for local and remote control planes. This is artifact
storage, not an application deployment framework or a Crossplane XR.

The single pod has two Distribution processes over one content store:

- `registry-upload:5001` selects a **loopback-only** HTTP writer. Reach it using
  `kubectl --context CONTEXT -n NAMESPACE port-forward --address 127.0.0.1 service/registry-upload :5001`.
  A host-side OCI client uses the printed local port. Docker's remote daemon
  must not be used to push to this workstation loopback address.
- `registry:5000` serves HTTPS in Distribution read-only mode. Both Crossplane
  and node container runtimes must resolve this name and trust its certificate.
  The reader disables redirects, deletion, upload purging, and proxy caching.

The upload Service deliberately has no reachable pod-IP write listener.
Kubelet port-forward connects inside the selected pod network namespace.
NetworkPolicy additionally permits only declared read peers on port 5000.
No NodePort, LoadBalancer, Ingress, DNS, or Gateway resource is created.
Do not add a public route as a workaround for node DNS/trust failures.

A StatefulSet keeps the upload RBAC bound to one stable pod name. Upload
subjects can read pod metadata in this namespace but can port-forward only
`registry-0`; they receive no exec, Secret, or package-manager write rights.
Use a dedicated namespace. Existing cluster-wide RBAC grants are additive and
cannot be revoked by this chart. NetworkPolicy needs an enforcing CNI and does
not isolate a compromised node or cluster administrator.

## Inputs and rendering

Provide an existing TLS Secret, explicit allowed read peers (including runtime
node CIDRs), and encrypted storage. The chart creates no certificates or cloud
infrastructure and accepts no static S3 credentials.

```sh
helm lint charts/oci-registry -f charts/oci-registry/ci/local.yaml
helm template registry charts/oci-registry -n crossplane-dev \
  -f charts/oci-registry/ci/remote.yaml
cargo test --test oci_registry_chart
```

The optional native protocol fixture uses the same rendered Distribution
configuration, with temporary filesystem paths, loopback ports, and a test CA:

```sh
HOPS_DISTRIBUTION_BINARY=/absolute/path/to/registry \
  cargo test --test distribution_protocol -- --ignored
```

Build that binary from the pinned Distribution v2.8.3 source. This verifies
upload/digest readback, TLS trust failure/success, read-side mutation rejection,
and process-restart durability without Docker or a local control plane. It does
not substitute for Kubernetes RBAC, NetworkPolicy, node pulls, PVC replacement,
or S3/workload-identity testing.

The files in `ci/` are **render fixtures**, not deployment-ready values: replace
documentation CIDRs, sample bucket/IAM names, and TLS Secret names. PVC
encryption is an operator attestation, not something Helm can verify. S3 uses
server-side encryption and HTTPS with existing workload identity; bucket
policies, isolation, and node trust remain infrastructure responsibilities.
A cloud credential plugin already used by kubeconfig is not a new Hops AWS
authentication requirement.

An Argo Application can select `charts/oci-registry` from this repository at a
pinned revision. The chart can also be rendered by a local controller or Helm;
Argo itself is not required. The chart and its runtime image must remain
fetchable without this development registry.

## Durability and cleanup

There is no automatic garbage collector or raw object TTL. Deletion and upload
purging are disabled pending the session-aware reachability/retention workflow.
Do not enable TTL on the bucket prefix. Retain every active and restorable
manifest and its transitive blob closure. Offline Distribution GC must only run
after stopping uploads and establishing that complete retention set; this chart
does not yet automate or claim that proof. PVCs have Helm and Argo retention
annotations, but other inventory controllers must separately honor retention.

## Local adoption boundary

This chart does **not yet replace** the embedded local NodePort installer.
Migrating a live local registry also needs host-side upload transport and node
trust integration. Do not run both writers on the existing claim or apply the
new Service over the old Deployment.

The intended migration reuses `registry-pvc`, the existing TLS Secret, and the
pull hostname, with the old writer stopped first and its manifests retained
for rollback. `storage.pvc.existingClaim` avoids creating/adopting the PVC.
Snapshot/backup the data and verify digest pulls before and after migration.
Neither that migration nor a shared-cluster deployment is performed by render
tests. Registry replacement, strict-TLS node pulls, RBAC denial, and
NetworkPolicy enforcement need the disposable Kubernetes integration fixture.

Configuration references: [Distribution configuration](https://distribution.github.io/distribution/about/configuration/),
[S3 driver](https://distribution.github.io/distribution/storage-drivers/s3/).
