# hops local Cluster template

This tree is embedded in hops-cli and materialized by `hops local up` to
`$HOME/.gitops/local/cluster`.

It is the machine Cluster desired state: Crossplane packages (helm/k8s/zitadel
providers and ProviderConfigs), platform stacks (auth, gateway, Istio, PSQL,
secrets/Vault), and local identity.

Project extras overlay from `<repo>/.gitops/local/cluster/` (last write wins by
relative path). Do not put shared app workloads here — those belong on a
cluster-scoped Environment (for Harmony: MinIO/Mailpit/Redis as
`harmony-system`).

Edit the files in this CLI template, not the materialized copy under `$HOME`.
The materialized directory is marked `.hops-managed` and is rewritten on each
`up`.
