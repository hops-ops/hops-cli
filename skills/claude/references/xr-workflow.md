# XR Lifecycle Workflow

## Overview

The `hops xr` commands support reclaiming existing infrastructure into Crossplane-managed
XRs. The flow is: **observe → adopt → manage** (and optionally **orphan** to release).

## Commands

### 1. Observe

Generate an observe-only XR manifest for existing infrastructure.

```bash
hops xr observe \
  --kind AutoEKSCluster \
  --name pat-local \
  --namespace default \
  --aws-region us-east-2 \
  --output observed.yaml
```

- Loads the live XR from the cluster if present
- Enriches with live AWS discovery for supported kinds (AutoEKSCluster, Network)
- Produces a manifest with `managementPolicies: ["Observe"]`

### 2. Adopt

Render metadata patches needed for Crossplane to adopt existing managed resources.

```bash
# Adopt next batch of resources
hops xr adopt \
  --kind AutoEKSCluster \
  --name pat-local \
  --namespace default \
  --apply

# Adopt all resources recursively
hops xr adopt \
  --kind AutoEKSCluster \
  --name pat-local \
  --namespace default \
  --recursive \
  --apply
```

- Lists managed resources belonging to the XR
- For AutoEKSCluster, uses label `hops.ops.com.ai/autoekscluster=<name>`
- Only patches resources with missing/blank `crossplane.io/external-name`
- Resolves external names for supported managed kinds (IAM attachments, KMS keys, etc.)

### 3. Manage

Convert the observed/adopted XR into a fully managed manifest.

```bash
hops xr manage \
  --kind AutoEKSCluster \
  --name pat-local \
  --namespace default \
  --output managed.yaml
```

- Generates the final managed XR manifest
- Changes `managementPolicies` from `["Observe"]` to `["*"]`

### 4. Orphan

Release managed resources by removing `Delete` from management policies.

```bash
hops xr orphan \
  --kind AutoEKSCluster \
  --name pat-local \
  --namespace default \
  --apply
```

- Renders patches that remove `Delete` from management policies
- Resources remain but Crossplane won't delete them if the XR is removed

## Typical Reclaim Flow

```bash
# 1. Observe existing infrastructure
hops xr observe --kind AutoEKSCluster --name pat-local --namespace default \
  --aws-region us-east-2 --output observed.yaml

# 2. Apply the observe-only XR
kubectl apply -f observed.yaml

# 3. Adopt all managed resources recursively
hops xr adopt --kind AutoEKSCluster --name pat-local --namespace default \
  --recursive --apply

# 4. Convert to managed
hops xr manage --kind AutoEKSCluster --name pat-local --namespace default \
  --output managed.yaml

# 5. Apply managed manifest
kubectl apply -f managed.yaml
```

## Flags

| Flag | Commands | Purpose |
|------|----------|---------|
| `--kind` | All | XR kind (e.g. AutoEKSCluster, Network) |
| `--name` | All | XR name |
| `--namespace` | All | XR namespace |
| `--aws-region` | observe | AWS region for discovery |
| `--output` | observe, manage | Write manifest to file |
| `--apply` | adopt, manage, orphan | Apply directly to cluster |
| `--recursive` | adopt | Keep adopting until no more patches needed |
