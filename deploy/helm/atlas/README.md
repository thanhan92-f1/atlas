<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Atlas Helm chart

Packages what `deploy/k8s/atlas-gateway.yaml` (fake/k8s driver) and
`deploy/k8s/atlas-gateway-ceph.yaml` (real Ceph driver) apply by hand into one versioned,
upgradeable chart. See `docs/DEPLOYMENT.md` for the full deployment picture (this chart replaces
the "apply these YAML files directly" step, not the Rook/Ceph prerequisites).

## Quickstart (fake driver, no Ceph needed)

```bash
helm install atlas ./deploy/helm/atlas \
  --create-namespace \
  --set auth.createSecret=true
```

This generates a random JWT secret and admin password in a Secret the chart creates — fine for a
local/lab install, but read `values.yaml`'s `auth.createSecret` comment before doing this for
anything beyond that: the generated values land in `helm get values` and Helm's release history.
For a real deployment, create the `atlas-gateway-auth` Secret yourself first (see
`deploy/k8s/atlas-auth-secret.example.yaml` or `deploy/vault-lab/` for an externally-managed
alternative) and leave `auth.createSecret` at its default `false`.

## Real Ceph deployment

```bash
helm install atlas-ceph ./deploy/helm/atlas \
  --set ceph.enabled=true \
  --set image.repository=localhost/atlas-gateway \
  --set image.tag=ceph \
  --set service.grpc.enabled=true
```

Expects to run in `ceph.rookNamespace` (default `rook-ceph`) with Rook's mon endpoints ConfigMap
and admin keyring Secret already present — this chart doesn't stand up Ceph itself, see
`deploy/rook-ceph-lab/`.

## RustFS object storage (server + gateway wiring)

```bash
helm install atlas ./deploy/helm/atlas --create-namespace --set auth.createSecret=true \
  --set rustfs.enabled=true --set rustfs.driverMode=real --set rustfs.server.enabled=true \
  --set stateBackup.enabled=true --set stateBackup.useRustfs=true
```

`rustfs.server.enabled` renders a single-replica RustFS (`templates/rustfs.yaml`, a templated mirror of
`deploy/rustfs-lab/deployment.yaml`, image pinned to `rustfs/rustfs:1.0.0`) with NodePorts
`rustfs.server.s3NodePort` (30900) / `consoleNodePort` (30901) and a CORS origin for the console's
NodePort (browser uploads/downloads go straight to RustFS via presigned URLs, so the S3 port must be
browser-reachable). `rustfs.server.generateCredentials` creates the `rustfs.credentialsSecret`
Secret once (`lookup` + `randAlphaNum`, kept across upgrades, `helm.sh/resource-policy: keep`, never
printed); set it false to supply your own. The gateway gets the endpoint, the credentials Secret name
and namespace, and the access/secret key env, so bucket create/delete, signed discovery, the console
self-test and DataBridge object migrations all work. `stateBackup.useRustfs` points the self-state
backup at it (bucket created on first run). This is a **lab topology** (one volume, not HA) — for
production point `rustfs.endpoint` at a real multi-drive/multi-node RustFS and leave the server off.
`helm template` cannot `lookup`, so it renders throwaway credentials; only a real install persists them.

## Raw-disk formatting from the console (`disks.enabled`)

`disks.enabled=true` makes the Disks page able to wipe and format raw disks (ZFS pool create/destroy):
the gateway pod runs **privileged** with hostPath mounts of the node's `/dev`, `/run/udev` and
`/proc/1/mountinfo`, the ZFS backend is forced on in `real` mode with `ATLAS_ZFS_HOST=$(NODE_IP)`, and
`ATLAS_HOST_MOUNTINFO_PATH` is set (see `docs/DISKS.md` for why each is needed). ZFS is node-local:
pin the pod with `nodeSelector`, keep `replicaCount: 1` (the template fails otherwise), and run one
disk-enabled gateway per node. The default image has `zfsutils-linux`; `Dockerfile.ceph` now does too.
The Ceph-OSD path additionally needs Rook with `ROOK_ENABLE_DISCOVERY_DAEMON`.

### Install-time RustFS-on-a-disk

`--set disks.enabled=true --set disks.autoRustfsDevice=/dev/sdb [--set disks.autoRustfsActivate=true]`
makes the gateway, on first start, format that one disk as a RustFS drive **only if it is empty** (never a
wipe), install RustFS from the official chart on it and (optionally) point Atlas at it. The console does
the same steps on demand: Storage → RustFS → Drives & pools. The chart also renders the
`atlas-rustfs-installer` ServiceAccount/Role the console's installer Job runs under, and grants the
gateway read access to Deployments/Services plus `patch` on its **own** Deployment only.

## Lab side-by-side install

`scripts/helm-lab-remote.sh <host> <user> --set auth.createSecret=true` installs the chart as release
`atlas-helm` in namespace `atlas-helm` from `values-lab.yaml` (own PVC, console NodePort 30520,
RustFS 30920/30921, image `localhost/atlas-gateway:dev` already imported by `deploy-remote.sh`) next
to the raw-manifest gateway. `namespace.create: false` is used there because `--create-namespace`
already creates the namespace. Cluster-scoped RBAC names include the release namespace, so two
installs (or the raw manifest's `atlas-gateway-readonly`) do not collide.

## Published images (0.4.0)

Lab quickstart above keeps `localhost/atlas-gateway:dev`. A tagged release is
`ghcr.io/zyvorai/atlas:0.4.0` (fake/k8s image) and `ghcr.io/zyvorai/atlas-ceph:0.4.0` (real Ceph
image). Pin the immutable version tag; do not deploy `:latest`.

```bash
helm install atlas ./deploy/helm/atlas \
  -f ./deploy/helm/atlas/values-release.yaml \
  --create-namespace \
  --set auth.createSecret=true

helm install atlas-ceph ./deploy/helm/atlas \
  -f ./deploy/helm/atlas/values-release.yaml \
  --set ceph.enabled=true \
  --set image.repository=ghcr.io/zyvorai/atlas-ceph \
  --set service.grpc.enabled=true
```

## What's parameterized

See `values.yaml` for the full set — image, replicas/resources, database backend, service
type/ports, ingress, PVC size/class, OIDC/SSO, self-state backup, native alerting sinks
(webhook/PagerDuty/Slack/Opsgenie — `docs/ALERTING.md`), the secrets backend (env vars or Vault —
`docs/SECRETS.md`), OpenTelemetry tracing (`docs/TRACING.md`), and the NFS/ZFS demo backends.

## Multi-replica (Postgres-backed) deployment

By default (`database.kind: sqlite`) Atlas is single-replica: the DB lives on a `ReadWriteOnce`
PVC only one pod can mount, so `templates/deployment.yaml` uses a `Recreate` rollout regardless of
`replicaCount`. The query layer itself is backend-agnostic (`sqlx::AnyPool` — see `docs/HA.md`),
so switching is a values change, not a code change:

```bash
helm install atlas ./deploy/helm/atlas \
  --set database.kind=postgres \
  --set database.existingSecret=atlas-postgres-auth \
  --set replicaCount=3
```

`database.existingSecret` must hold the full `postgres://user:pass@host:port/db` connection string
under `database.secretKey` (default `database-url`) — this chart does not stand up Postgres
itself; point it at a real managed/HA instance (RDS, CloudNativePG, Patroni-managed) or
`deploy/postgres-lab/` for a throwaway lab one. With `database.kind: postgres` the PVC is not
rendered at all and the rollout strategy becomes `RollingUpdate`.

`rateLimitRpm` is cluster-wide-aware when `database.kind: postgres` and `replicaCount > 1`: each
pod's rate limiter stays fast and in-process for the actual allow/deny decision, but a background
task periodically syncs counts through the database so replicas converge on one shared budget
(eventually consistent within `ATLAS_RATE_LIMIT_SYNC_SECS`, default `2`s — see `docs/HA.md`). Not a
separate chart value; it rides `database.kind` automatically.

## Verifying a render before installing

```bash
helm lint ./deploy/helm/atlas
helm template atlas ./deploy/helm/atlas --set ceph.enabled=true | kubectl apply --dry-run=client -f -
```
