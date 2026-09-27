// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Provision a raw, unformatted disk into a new ZFS pool or a new Ceph OSD (via Rook). A rare,
// high-consequence admin action (wipes a disk) — deliberately its own destination rather than a
// button on Backends/Cluster, and deliberately kept thin: one form, one recent-jobs table.
import { useState } from "react";
import { submitJob } from "../api/client";
import { useInvalidate, useJobs, useNodes } from "../api/hooks";
import { Badge, FormModal, type FormField } from "../ui/kit";
import { Table } from "../ui/Table";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { stateKind, timeAgo } from "../lib/format";

const JOB_TYPES = new Set(["zfs.pool.create_from_device", "ceph.osd.add_device"]);

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export default function Disks() {
  const [open, setOpen] = useState(false);
  const { data: nodes } = useNodes();
  const { data: jobs } = useJobs();
  const inv = useInvalidate();

  const nodeOptions = (nodes || []).map((n) => ({ value: n.host, label: n.host }));
  const recentJobs = (jobs || []).filter((j) => JOB_TYPES.has(j.job_type));

  const fields = (vals: Record<string, string>): FormField[] => {
    const backend = vals.backend || "zfs";
    const out: FormField[] = [
      {
        name: "backend",
        label: "Provision as",
        options: [
          { value: "zfs", label: "ZFS pool" },
          { value: "ceph", label: "Ceph OSD (Rook)" },
        ],
      },
    ];
    if (backend === "ceph") {
      out.push({
        name: "node_name",
        label: "Node",
        options: nodeOptions,
        hint: nodeOptions.length ? undefined : "No nodes discovered yet.",
      });
    } else {
      out.push({
        name: "pool_name",
        label: "Pool name",
        placeholder: "tank2",
        pattern: /^[a-z][a-z0-9_-]*$/,
        hint: "zpool name — lowercase, no spaces.",
      });
    }
    out.push({
      name: "device_path",
      label: "Device path",
      placeholder: "/dev/sdb",
      pattern: /^\/dev\/[a-zA-Z0-9/_-]+$/,
      hint:
        backend === "zfs"
          ? "Exact path on the host the gateway runs on. This will be wiped."
          : "Exact path as seen on the selected node. This will be wiped.",
    });
    if (backend === "zfs") {
      out.push({
        name: "wipe_existing",
        label: "If the device already has data",
        options: [
          { value: "refuse", label: "Refuse (default, safest)" },
          { value: "wipe", label: "Wipe residual signatures first (destructive)" },
        ],
        hint:
          "Clears a stale partition table or filesystem signature (e.g. a decommissioned Ceph " +
          "OSD) before creating the pool. Never overrides the root/boot-disk or mounted-device " +
          "refusal.",
      });
    }
    // Cross-field "type it again to confirm" gate: FormModal's own per-field `pattern` check is
    // reused here rather than a bespoke modal — the pattern is just built from the *other* field's
    // current value. "\u0000" as the fallback can never be typed into a text input, so an empty
    // device_path makes this field impossible to satisfy (it's also still gated by the plain
    // required-field check below that point).
    out.push({
      name: "confirm_path",
      label: `Type "${vals.device_path || "the device path above"}" to confirm`,
      placeholder: vals.device_path || "/dev/sdX",
      pattern: new RegExp(`^${escapeRegExp(vals.device_path || "\u0000")}$`),
    });
    return out;
  };

  return (
    <ListPage
      crumbs={navCrumbs("disks")}
      eyebrow="INFRASTRUCTURE · OPS"
      title="Disks"
      state="Provision a raw, unformatted disk into a new ZFS pool or Ceph OSD. Irreversible — wipes the target device."
      actions={
        <button type="button" className="at-btn danger" onClick={() => setOpen(true)}>
          Provision device…
        </button>
      }
    >
      <Table
        soundings
        panelTitle="Recent provisioning jobs"
        rows={recentJobs}
        rowKey={(j) => j.id}
        empty="No disk-provisioning jobs yet."
        cols={[
          { h: "Job", f: (j) => j.id, mono: true },
          {
            h: "Kind",
            f: (j) => (j.job_type === "ceph.osd.add_device" ? "Ceph OSD" : "ZFS pool"),
          },
          { h: "State", f: (j) => <Badge kind={stateKind(j.state)} dot>{j.state}</Badge> },
          { h: "Requested", f: (j) => timeAgo(j.created_at) },
        ]}
      />

      <FormModal
        open={open}
        onClose={() => setOpen(false)}
        title="Provision a raw device"
        fields={fields}
        submitLabel="Provision (destructive)"
        danger
        onSubmit={async (vals) => {
          if (vals.backend === "ceph") {
            await submitJob(
              "post",
              "/ceph/devices",
              { node_name: vals.node_name, device_path: vals.device_path, confirm: true },
              `provision ${vals.device_path} → Ceph OSD (${vals.node_name})`,
              () => inv("osds", "pools", "clusters", "jobs"),
            );
          } else {
            await submitJob(
              "post",
              "/zfs/pools/from-device",
              {
                pool_name: vals.pool_name,
                device_path: vals.device_path,
                confirm: true,
                wipe_existing: vals.wipe_existing === "wipe",
              },
              `provision ${vals.device_path} → ZFS pool ${vals.pool_name}`,
              () => inv("pools", "nodes", "jobs"),
            );
          }
        }}
      />
    </ListPage>
  );
}
