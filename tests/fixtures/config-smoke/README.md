# config-smoke

Minimal Crossplane configuration fixture for **path-based** hops integration tests.

```bash
hops local start --backend dory   # or kind/colima
hops config install --path tests/fixtures/config-smoke --backend dory
kubectl apply -f tests/fixtures/config-smoke/local/ci-xr.yaml
kubectl -n hops-ci wait --for=condition=Ready configsmoke/hops-ci-smoke --timeout=300s
kubectl -n hops-ci get configmap hops-ci-smoke
```

Creates only namespaced resources under `hops-ci` (ConfigMap via provider-kubernetes).
Not a real platform stack — safe on a shared local control plane.
