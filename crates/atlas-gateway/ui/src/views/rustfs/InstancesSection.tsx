// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// RustFS servers in the gateway's namespace: install one from RustFS's own Helm chart (optionally
// on a drive prepared from the Disks page), point Atlas at it, or remove it. The heavy lifting is
// server-side (an installer Job / a patch of the gateway's own Deployment).
import { useState } from "react";
import { submit, submitJob } from "../../api/client";
import { useInvalidate, useRustfsDrives, useRustfsInstances } from "../../api/hooks";
import { Badge, Button, FormModal, type FormField } from "../../ui/kit";
import { Table } from "../../ui/Table";
import { confirmThen, del } from "../../ui/confirm";
import { Section } from "./common";

const NO_DRIVE = "";

export default function InstancesSection() {
  const { data: instances } = useRustfsInstances();
  const { data: drives } = useRustfsDrives();
  const inv = useInvalidate();
  const refresh = () => inv("rustfs-instances", "jobs", "rustfs-drives");
  const [deploy, setDeploy] = useState(false);

  const usedClaims = new Set((instances || []).map((i) => i.claim).filter(Boolean));
  const claimOptions = [
    { value: NO_DRIVE, label: "Chart-managed volume (local-path, root disk)" },
    ...(drives || [])
      .filter((d) => d.claim && !usedClaims.has(d.claim))
      .map((d) => ({ value: d.claim as string, label: `Drive ${d.name} · ${d.claim}` })),
  ];
  const usedPorts = (instances || []).flatMap((i) => [i.s3_node_port, i.console_node_port]).filter(Boolean) as number[];
  const nextPort = (from: number) => {
    let p = from;
    while (usedPorts.includes(p)) p += 1;
    return String(p);
  };

  const fields = (): FormField[] => [
    { name: "name", label: "Instance name", pattern: /^[a-z][a-z0-9-]{1,30}$/, placeholder: "rustfs-sdb", hint: "Lowercase letters, digits and hyphens." },
    {
      name: "pvc",
      label: "Data volume",
      options: claimOptions,
      hint: "A drive prepared from the Disks page keeps the data on that disk. RustFS runs single-drive (no erasure coding) — it cannot be expanded in place; move data by migration.",
    },
    { name: "s3_node_port", label: "S3 API node port", type: "number", value: nextPort(30930), min: 30000 },
    { name: "console_node_port", label: "Web console node port", type: "number", value: nextPort(30931), min: 30000 },
  ];

  return (
    <Section
      title="RustFS servers"
      actions={<Button size="sm" variant="primary" onClick={() => setDeploy(true)}>Deploy RustFS…</Button>}
    >
      <Table
        soundings
        panelTitle="RustFS instance index"
        rows={instances}
        rowKey={(i) => i.name}
        empty="No RustFS servers found."
        cols={[
          { h: "Instance", f: (i) => i.name, mono: true },
          { h: "Installed by", f: (i) => (i.managed_by === "helm" ? "official chart" : "lab manifest") },
          { h: "State", f: (i) => <Badge kind={i.ready ? "success" : "warning"} dot>{i.ready ? "ready" : "starting"}</Badge> },
          { h: "S3 / console", f: (i) => `${i.s3_node_port ?? "—"} / ${i.console_node_port ?? "—"}`, mono: true },
          { h: "Data", f: (i) => i.claim ?? "—", mono: true },
          { h: "Atlas", f: (i) => (i.active ? <Badge kind="info" dot>in use</Badge> : "—") },
        ]}
        actions={(i) => (
          <>
            {!i.active && (
              <Button
                size="sm"
                onClick={() =>
                  confirmThen(
                    {
                      title: `Point Atlas at ${i.name}?`,
                      message:
                        "Atlas's RustFS endpoint, credentials and (if enabled) state backup switch to this server and the gateway restarts. Copy your data first (DataBridge → Object Migrations), then re-adopt buckets with “Import from RustFS” on the Buckets page.",
                      confirmLabel: "Switch and restart",
                    },
                    () => submit("post", `/rustfs/instances/${i.name}/activate`, null, `switch Atlas to ${i.name}`, refresh).catch(() => {}),
                  )
                }
              >
                Use for Atlas
              </Button>
            )}
            {i.managed_by === "helm" && !i.active && (
              <Button
                size="sm"
                variant="danger"
                onClick={() =>
                  del(`RustFS server ${i.name} (its data volume is kept)`, () =>
                    submitJob("delete", `/rustfs/instances/${i.name}`, null, `uninstall ${i.name}`, refresh).catch(() => {}),
                  )
                }
              >
                Uninstall
              </Button>
            )}
          </>
        )}
      />
      <FormModal
        open={deploy}
        onClose={() => setDeploy(false)}
        title="Deploy RustFS"
        submitLabel="Deploy"
        fields={fields}
        onSubmit={async (v) => {
          await submitJob(
            "post",
            "/rustfs/instances",
            {
              name: v.name,
              pvc: v.pvc ?? "",
              s3_node_port: Number(v.s3_node_port),
              console_node_port: Number(v.console_node_port),
            },
            `deploy RustFS ${v.name}`,
            refresh,
          );
        }}
      />
    </Section>
  );
}
