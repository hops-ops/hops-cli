# Hops Stacks and XRs

## Platform Stacks (AWS-specific)

These compose multiple resources for a specific platform concern. Group: `aws.hops.ops.com.ai`.

| Kind | Package | Description |
|------|---------|-------------|
| `BaseStack` | `aws-base-stack` | AWS Load Balancer Controller with Pod Identity |
| `CrossplaneStack` | `aws-crossplane-stack` | Crossplane + AWS/K8s/Helm/GitHub providers with toggles |
| `DNSStack` | `aws-dns-stack` | ExternalDNS, CertManager, ClusterIssuer for DNS/TLS |
| `ObserveStack` | `aws-observe-stack` | Full observability: KPS, Loki, Tempo, k8s-monitoring, Grafana Operator, VPA, Goldilocks. Optional dedicated NodePool |
| `SecretStack` | `aws-secret-stack` | External Secrets Operator with Pod Identity for Secrets Manager/SSM. ClusterSecretStore for Pod Identity auth |

## Platform Stacks (Cloud-agnostic)

Group: `hops.ops.com.ai`. These install Helm charts and wire them together.

| Kind | Package | Description |
|------|---------|-------------|
| `GitopsStack` | `gitops-stack` | ArgoCD + GitHub repo creation + optional ESO for repo credentials |
| `IstioStack` | `istio-stack` | istio-base, istiod, gateway |
| `KnativeStack` | `knative-stack` | Knative Operator, Serving/Eventing, optional NATS |
| `ObserveStack` | `observe-stack` | Base observability (KPS, Loki, Tempo, k8s-monitoring, Grafana Operator) — no AWS features |
| `PSQLStack` | `psql-stack` | StackGres (with Citus) + Atlas Operator for PostgreSQL |

## AWS Infrastructure XRs

Group: `aws.hops.ops.com.ai`. These manage AWS resources directly.

| Kind | Package | Description |
|------|---------|-------------|
| `AutoEKSCluster` | `aws-auto-eks-cluster` | EKS cluster with Auto Mode, IAM, KMS, optional Karpenter |
| `Network` | `aws-network` | VPC, subnets, routing, IPAM support |
| `Foundation` | `aws-foundation` | Organization + Identity Center + IPAM |
| `Organization` | `aws-organization` | AWS Organizations with OUs, accounts, delegated admins |
| `Account` | `aws-account` | AWS account within an organization |
| `IdentityCenter` | `aws-identity-center` | SSO users, permission sets, account assignments |
| `PodIdentity` | `aws-pod-identity` | IAM role + EKS Pod Identity association |
| `IRSA` | `aws-irsa` | IAM role + EKS Pod Identity (alias) |
| `ActionsConnector` | `aws-actions-connector` | GitHub Actions → AWS OIDC trust |
| `RAMShare` | `aws-ram-share` | Cross-account resource sharing |

## Common Patterns

All XRs share these conventions:
- `spec.clusterName` — target cluster name, used for provider config defaults
- `spec.managementPolicies` — defaults to `["*"]`
- `spec.labels` — custom labels merged with defaults
- Provider config refs: `helmProviderConfigRef`, `kubernetesProviderConfigRef`, `awsProviderConfigRef`
- AWS stacks require `spec.aws.region`

## Package Installation

```bash
# Published version
hops config install --repo hops-ops/<package> --version <tag>

# Local source build
hops config install --path xrs/aws/_stacks/observe
```

All packages are published to `ghcr.io/hops-ops/<package>`.
