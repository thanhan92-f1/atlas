// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
import { Badge, StatCard } from "../../ui/kit";
import { Table } from "../../ui/Table";
import { fmtBytes, num } from "../../lib/format";
import { asArr, asNum, asRec, asStr, useRustfs, type Rec } from "./api";
import { LoadError, RawJson, Section, StatGrid } from "./common";

const uptime = (secs: number) => {
  if (!secs) return "—";
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  return d ? `${d}d ${h}h` : `${h}h ${Math.floor((secs % 3600) / 60)}m`;
};

export default function OverviewTab() {
  const info = useRustfs("info");
  const usage = useRustfs("datausageinfo", undefined, 30000);

  if (info.error) return <LoadError error={info.error} />;
  // /info answers {info: InfoMessage, admin_discovery, bitrotSelftest}.
  const i = asRec(asRec(info.data).info ?? info.data);
  const u = asRec(usage.data);
  const backend = asRec(i.backend);
  const online = asNum(backend.onlineDisks);
  const offline = asNum(backend.offlineDisks);
  const servers = asArr(i.servers).map(asRec);
  const bucketsUsage = Object.entries(asRec(u.buckets_usage));
  const capacity = asNum(u.total_capacity);

  return (
    <>
      <StatGrid>
        <StatCard label="Objects" value={num(asNum(asRec(i.objects).count))} sub={`${num(asNum(asRec(i.versions).count))} versions`} />
        <StatCard label="Buckets" value={num(asNum(asRec(i.buckets).count))} />
        <StatCard label="Used" value={fmtBytes(asNum(asRec(i.usage).size))} sub={capacity ? `${fmtBytes(asNum(u.total_used_capacity))} of ${fmtBytes(capacity)} disk` : undefined} />
        <StatCard
          label="Drives online"
          value={`${online}/${online + offline}`}
          sub={offline ? `${offline} offline` : "all online"}
          accent={offline ? "var(--at-danger, #e5484d)" : undefined}
        />
        <StatCard
          label="Erasure coding"
          value={backend.standardSCParity != null ? `EC:${asNum(backend.standardSCParity)}` : "—"}
          sub={`${asArr(backend.totalSets).join(",") || "?"} set(s) × ${asArr(backend.totalDrivesPerSet).join(",") || "?"} drives`}
        />
        <StatCard label="Mode" value={asStr(i.mode) || "—"} sub={`${asStr(i.region) || "default region"} · ${asStr(i.deploymentID).slice(0, 8)}`} />
      </StatGrid>

      <Section title="Servers">
        <Table<Rec>
          soundings
          panelTitle="Server index"
          rows={info.data ? servers : undefined}
          rowKey={(r, n) => asStr(r.endpoint) || String(n)}
          empty="No servers reported."
          cols={[
            { h: "Endpoint", f: (r) => asStr(r.endpoint), mono: true },
            { h: "State", f: (r) => <Badge kind={asStr(r.state) === "online" ? "success" : "danger"} dot>{asStr(r.state) || "unknown"}</Badge> },
            { h: "Version", f: (r) => asStr(r.version) || "—", mono: true },
            { h: "Uptime", f: (r) => uptime(asNum(r.uptime)) },
            { h: "Drives", f: (r) => asArr(r.drives).length },
          ]}
        />
      </Section>

      {bucketsUsage.length > 0 && (
        <Section title="Usage by bucket">
          <Table<[string, unknown]>
            soundings
            panelTitle="Bucket usage"
            rows={bucketsUsage}
            rowKey={([name]) => name}
            cols={[
              { h: "Bucket", f: ([name]) => name, mono: true },
              { h: "Objects", f: ([, b]) => num(asNum(asRec(b).objects_count)) },
              { h: "Versions", f: ([, b]) => num(asNum(asRec(b).versions_count)) },
              { h: "Size", f: ([, b]) => fmtBytes(asNum(asRec(b).size)) },
            ]}
          />
        </Section>
      )}

      <RawJson title="Raw server info" value={info.data} />
      {usage.data != null && <RawJson title="Raw data usage" value={usage.data} />}
    </>
  );
}
