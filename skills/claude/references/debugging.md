# Debugging with kubectl

## Common Crossplane Debugging

### Check XR status
```bash
kubectl get <kind>.<group> -A
kubectl describe <kind>.<group> <name> -n <namespace>
```

### Check composed resources
```bash
kubectl get releases.helm.m.crossplane.io
kubectl get objects.kubernetes.m.crossplane.io
kubectl get repositories.repo.github.m.upbound.io
kubectl get podidentities.aws.hops.ops.com.ai
```

### Check provider health
```bash
kubectl get providers.pkg.crossplane.io
kubectl get configurations
kubectl get functions
```

## Common Issues

### Helm release stuck in `pending-install` with `failed: 1`

The Helm install failed mid-way (often due to expired credentials). The provider won't
retry automatically.

**Fix:** Delete the Release resource so the XR recreates it:
```bash
kubectl delete releases.helm.m.crossplane.io <release-name>
```

### Stale credentials after `hops local aws --refresh`

The Helm/Kubernetes providers cache connections. After refreshing the kubeconfig secret,
restart the provider pods:
```bash
kubectl delete pod -n crossplane-system -l pkg.crossplane.io/package=crossplane-contrib-provider-helm
kubectl delete pod -n crossplane-system -l pkg.crossplane.io/package=crossplane-contrib-provider-kubernetes
```

### Resource stuck deleting — `403` / insufficient permissions

External resources (GitHub repos, AWS resources) may fail to delete if the provider
credentials lack the required permissions (e.g., `delete_repo` scope for GitHub).

**Diagnosis:**
```bash
kubectl describe <resource-type> <name>
# Look for "async delete failed" or "CannotDeleteExternalResource" events
```

**Fix:** Remove the finalizer so Crossplane stops trying to delete and the XR can
recreate it. Note the `crossplane.io/external-name` annotation first:
```bash
# Get the external name for re-adoption
kubectl get <resource-type> <name> \
  -o jsonpath='{.metadata.annotations.crossplane\.io/external-name}'

# Remove finalizer
kubectl patch <resource-type> <name> \
  --type merge -p '{"metadata":{"finalizers":[]}}'
```

The XR will recreate the managed resource. If the external resource already exists,
the new resource will fail with `name already exists`. Set the external name annotation
to adopt the existing resource:
```bash
kubectl annotate <resource-type> <name> \
  crossplane.io/external-name=<value> --overwrite
```

### Resource not found after recreation

When an XR recreates a managed resource and the external resource already exists
(e.g., a GitHub repo), the provider tries to create it and gets `422 name already exists`.

**Fix:** Set the `crossplane.io/external-name` annotation to the existing resource's
name so the provider adopts it instead of creating.

### Configuration HEALTHY: False after switching local/published

When switching between `hops config install --path` (local) and `--version` (published),
stale Functions, ImageConfig rewrites, or ConfigurationRevisions can cause digest
conflicts.

**Fix:** The CLI handles this automatically, but if you see it:
```bash
# Check for stale functions
kubectl get functions | grep <stack-name>

# Check for local ImageConfig rewrites
kubectl get imageconfigs.pkg.crossplane.io | grep hops-local

# Check for inactive local revisions
kubectl get configurationrevisions | grep <stack-name>
```

Delete the stale resources and the Configuration will re-resolve.

### Provider pod not starting — ImagePullBackOff

The function image digest doesn't match what's in the registry.

**Fix:**
```bash
# Delete the stale function so dep resolution recreates it
kubectl delete function <function-name>
```

## Useful Contexts

When debugging, remember the control plane runs on Colima and target clusters are remote:

```bash
kubectl config use-context colima              # Control plane
kubectl config use-context arn:aws:eks:...     # Target cluster
```

- XRs, Configurations, Functions, Providers → colima
- Helm release workloads (ArgoCD pods, monitoring, etc.) → target cluster
