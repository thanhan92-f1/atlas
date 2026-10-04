<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Two-site RBD mirroring lab

What the 2026-10-04 drill in [`docs/DR.md`](../../docs/DR.md) ran on: a second, host-networked
Rook cluster (`rook-ceph-dr`) as the mirroring **primary**, beside whatever Rook cluster the host
already runs, and an existing pod-networked cluster on another host as the **secondary**.

Only the secondary runs `rbd-mirror`, and it has to reach the primary's mons and OSDs. Pod IPs
aren't routable between hosts, so the primary uses `network.provider: host`, and its Ceph ports
(3300 and 6800–7300) are firewalled to the peer's IP. The secondary can stay on the pod network
because nothing dials into it. That makes replication one-way, and so failback is a forced
promote followed by a resync (see `docs/DR.md`).

## Primary host

```bash
sudo truncate -s 40G /var/lib/ceph-dr-osd.img
sudo install -m0644 ceph-dr-osd-loop.service ceph-dr-firewall.service /etc/systemd/system/
sudo install -m0755 ceph-dr-firewall.sh /usr/local/sbin/
# Edit PEER= in ceph-dr-firewall.service to the secondary's public IP first.
sudo systemctl daemon-reload && sudo systemctl enable --now ceph-dr-osd-loop ceph-dr-firewall
kubectl -n rook-ceph patch cm rook-ceph-operator-config --type merge \
  -p '{"data":{"ROOK_CEPH_ALLOW_LOOP_DEVICES":"true"}}'
kubectl create -f 00-rbac.yaml
kubectl apply -f 01-cephcluster.yaml      # edit the node name first
NAMESPACE=rook-ceph-dr bash ../../scripts/ensure-atlas-auth-secret.sh
kubectl apply -f 02-atlas-gateway-dr.yaml # NodePort 30521
```

Allowing loop devices is operator-wide. That's safe only if every other CephCluster on the
operator names its devices (`useAllDevices: false`). Otherwise those clusters would claim
`/dev/loop30` too. The mons run msgr2 only (`requireMsgr2`) because the lab host already had
something on 6789.

## Secondary cluster

```bash
kubectl apply -f 10-secondary-pool-and-mirror.yaml
# Copy the primary's bootstrap token (keys `token` and `pool`) into the secondary's namespace:
kubectl -n rook-ceph-dr get secret pool-peer-token-atlas-dr-mirror-test -o json   # on the primary
#   ...re-create it as Secret `atlas-dr-site-peer` in `rook-ceph` on the secondary, then:
kubectl -n rook-ceph patch cephblockpool atlas-dr-mirror-test --type merge \
  -p '{"spec":{"mirroring":{"peers":{"secretNames":["atlas-dr-site-peer"]}}}}'
```

Rook imports the token with its own admin credentials, which avoids the `(13) Permission denied`
that a manual `rbd mirror pool peer bootstrap import` hits under a scoped identity. Because the
primary is host-networked, the token's `mon_host` is already the routable `v2:<host-ip>:3300`.

## Teardown

```bash
kubectl -n rook-ceph-dr delete -f 02-atlas-gateway-dr.yaml
kubectl -n rook-ceph-dr patch cephcluster rook-ceph-dr --type merge \
  -p '{"spec":{"cleanupPolicy":{"confirmation":"yes-really-destroy-data"}}}'
kubectl delete -f 01-cephcluster.yaml && kubectl delete -f 00-rbac.yaml
sudo systemctl disable --now ceph-dr-firewall ceph-dr-osd-loop
sudo iptables -D INPUT -p tcp --dport 3300 -j ATLAS-DR; sudo iptables -D INPUT -p tcp --dport 6800:7300 -j ATLAS-DR
sudo iptables -F ATLAS-DR; sudo iptables -X ATLAS-DR
sudo rm -rf /var/lib/rook-dr /var/lib/ceph-dr-osd.img
```
