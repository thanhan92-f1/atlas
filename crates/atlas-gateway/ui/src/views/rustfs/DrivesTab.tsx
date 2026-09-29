// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
import { useState } from "react";
import { Badge, Button, FormModal, Modal } from "../../ui/kit";
import { Table } from "../../ui/Table";
import { confirmThen } from "../../ui/confirm";
import { fmtBytes } from "../../lib/format";
import Disks from "../Disks";
import { http } from "../../api/client";
import { asArr, asNum, asRec, asStr, rfsError, rfsWrite, useRefreshRustfs, useRustfs, type Rec } from "./api";
import InstancesSection from "./InstancesSection";
import { Dynamic, LoadError, Muted, RawJson, Section, escapeRegExp } from "./common";

/** Read-only: tier add/edit/remove need a remote target's credentials, deferred (see docs/RUSTFS.md). */
function TiersSection() {
  const tiers = useRustfs("tier");
  const stats = useRustfs("tier-stats");
  const rows = asArr(tiers.data);
  return (
    <Section title="Tiers">
      {tiers.error ? (
        <Muted>{rfsError(tiers.error)}</Muted>
      ) : rows.length === 0 && tiers.data != null ? (
        <Muted>No tiers configured.</Muted>
      ) : (
        tiers.data != null && <Dynamic title="Tier index" data={tiers.data} />
      )}
      {stats.data != null && !stats.error && <RawJson title="Tier stats" value={stats.data} />}
    </Section>
  );
}

const UsageBar = ({ used, total }: { used: number; total: number }) => {
  const pct = total > 0 ? Math.min(100, (used / total) * 100) : 0;
  return (
    <div style={{ minWidth: 160 }}>
      <div style={{ height: 6, borderRadius: 3, background: "rgba(128,128,128,.25)" }}>
        <div style={{ width: `${pct}%`, height: 6, borderRadius: 3, background: "var(--at-accent, #3b82f6)" }} />
      </div>
      <div className="at-caption">{fmtBytes(used)} / {fmtBytes(total)}</div>
    </div>
  );
};

export default function DrivesTab() {
  const refresh = useRefreshRustfs();
  const storage = useRustfs("storageinfo");
  const poolsList = useRustfs("pools/list");
  const rebalance = useRustfs("rebalance/status");
  const [decommission, setDecommission] = useState(false);
  const [cancel, setCancel] = useState(false);
  const [healStatus, setHealStatus] = useState<unknown>(null);

  // /storageinfo answers {info: StorageInfo, admin_discovery}.
  const disks = asArr(asRec(asRec(storage.data).info ?? storage.data).disks).map(asRec);
  const pools = asArr(poolsList.data).map(asRec);
  const poolOptions = pools.map((p) => ({ value: String(asNum(p.id)), label: `Pool ${asNum(p.id)} · ${asStr(p.cmdline)}` }));

  // RustFS's heal handler needs every HealOpts field present (only pool/set are optional).
  const healBody = (dryRun: boolean) => ({ recursive: true, dryRun, remove: false, recreate: false, scanMode: 1, updateParity: false, nolock: false });
  const startHeal = (dryRun: boolean) =>
    confirmThen(
      {
        title: dryRun ? "Start a heal dry run?" : "Start a heal?",
        message: dryRun
          ? "Scans every object and reports what would be repaired, without changing anything."
          : "Re-checks and repairs every object's erasure-coded shards. Safe, but I/O-heavy on large stores.",
        confirmLabel: dryRun ? "Start dry run" : "Start heal",
      },
      () => rfsWrite(dryRun ? "heal dry run" : "start heal", "post", "heal/", { data: healBody(dryRun) }, refresh).catch(() => {}),
    );
  const rebalanceAction = (action: "start" | "stop") =>
    confirmThen(
      { title: `${action === "start" ? "Start" : "Stop"} rebalance?`, message: "Spreads existing data evenly across the pools.", confirmLabel: action === "start" ? "Start" : "Stop" },
      () => rfsWrite(`rebalance ${action}`, "post", `rebalance/${action}`, {}, refresh),
    );
  const showHealStatus = async () => {
    try {
      const { data } = await http.post("/rustfs/proxy/admin/v3/background-heal/status");
      setHealStatus(data ?? {});
    } catch (e) {
      setHealStatus({ error: rfsError(e) });
    }
  };

  return (
    <>
      {storage.error ? (
        <LoadError error={storage.error} />
      ) : (
        <Section title="Drives">
          <Table<Rec>
            soundings
            panelTitle="Drive index"
            rows={storage.data ? disks : undefined}
            rowKey={(d, n) => asStr(d.uuid) || asStr(d.endpoint) + asStr(d.path) || String(n)}
            empty="No drives reported."
            cols={[
              { h: "Drive", f: (d) => `${asStr(d.endpoint)} ${asStr(d.path)}`.trim(), mono: true },
              { h: "State", f: (d) => <Badge kind={asStr(d.state) === "ok" || asStr(d.state) === "online" ? "success" : "danger"} dot>{asStr(d.state) || "unknown"}</Badge> },
              { h: "Pool/Set/Disk", f: (d) => `${asNum(d.pool_index)}/${asNum(d.set_index)}/${asNum(d.disk_index)}` },
              { h: "Usage", f: (d) => <UsageBar used={asNum(d.usedspace)} total={asNum(d.totalspace)} /> },
              {
                h: "Flags",
                f: (d) => (
                  <>
                    {d.healing === true && <Badge kind="warning">healing</Badge>}
                    {d.scanning === true && <Badge kind="info">scanning</Badge>}
                    {d.rootDisk === true && <Badge kind="danger">root disk</Badge>}
                  </>
                ),
              },
            ]}
          />
        </Section>
      )}

      <Section
        title="Pools"
        actions={
          <>
            <Button size="sm" variant="danger" onClick={() => setDecommission(true)}>Decommission…</Button>
            <Button size="sm" onClick={() => setCancel(true)}>Cancel decommission…</Button>
          </>
        }
      >
        {poolsList.error ? (
          <Muted>{rfsError(poolsList.error)}</Muted>
        ) : (
          <Table<Rec>
            soundings
            panelTitle="Pool index"
            rows={poolsList.data ? pools : undefined}
            rowKey={(p, n) => String(asNum(p.id) || n)}
            empty="No pools reported."
            cols={[
              { h: "Pool", f: (p) => asNum(p.id) },
              { h: "Volumes", f: (p) => asStr(p.cmdline), mono: true },
              { h: "State", f: (p) => <Badge kind={asStr(p.status) === "active" ? "success" : "warning"} dot>{asStr(p.status) || "unknown"}</Badge> },
              { h: "Usage", f: (p) => <UsageBar used={asNum(p.usedSize)} total={asNum(p.totalSize)} /> },
              { h: "Decommission", f: (p) => asStr(p.decommissionStatus) || "none" },
              { h: "Rebalance", f: (p) => asStr(p.rebalanceStatus) || "none" },
            ]}
          />
        )}
      </Section>

      <TiersSection />

      <Section
        title="Data maintenance"
        actions={
          <>
            <Button size="sm" onClick={() => rebalanceAction("start")}>Start rebalance</Button>
            <Button size="sm" onClick={() => rebalanceAction("stop")}>Stop rebalance</Button>
            <Button size="sm" onClick={() => startHeal(true)}>Heal dry run</Button>
            <Button size="sm" onClick={() => startHeal(false)}>Start heal</Button>
            <Button size="sm" onClick={showHealStatus}>Heal status</Button>
          </>
        }
      >
        {rebalance.error ? <Muted>Rebalance: {rfsError(rebalance.error)}</Muted> : rebalance.data != null && <RawJson title="Rebalance status" value={rebalance.data} />}
      </Section>

      <InstancesSection />

      <Section title="Format a disk for RustFS">
        <Disks embedded />
      </Section>

      <FormModal
        open={decommission}
        onClose={() => setDecommission(false)}
        title="Decommission a pool"
        submitLabel="Decommission (drains its data)"
        danger
        fields={(v) => [
          { name: "pool", label: "Pool", options: poolOptions, hint: "Objects are moved to the remaining pools; needs at least two pools." },
          { name: "confirm", label: `Type "${v.pool ?? poolOptions[0]?.value ?? "the pool id"}" to confirm`, pattern: new RegExp(`^${escapeRegExp(v.pool ?? poolOptions[0]?.value ?? "\u0000")}$`) },
        ]}
        onSubmit={async (v) => {
          await rfsWrite("decommission pool", "post", "pools/decommission", { params: { pool: v.pool || poolOptions[0]?.value, "by-id": true } }, refresh);
        }}
      />
      <FormModal
        open={cancel}
        onClose={() => setCancel(false)}
        title="Cancel a pool decommission"
        submitLabel="Cancel decommission"
        fields={[{ name: "pool", label: "Pool", options: poolOptions }]}
        onSubmit={async (v) => {
          await rfsWrite("cancel decommission", "post", "pools/cancel", { params: { pool: v.pool || poolOptions[0]?.value, "by-id": true } }, refresh);
        }}
      />
      <Modal open={healStatus != null} onClose={() => setHealStatus(null)} title="Background heal status">
        <RawJson title="Response" value={healStatus} />
      </Modal>
    </>
  );
}

