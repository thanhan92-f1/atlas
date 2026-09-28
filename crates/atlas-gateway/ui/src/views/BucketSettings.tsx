// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Per-bucket settings for RustFS buckets: versioning, lifecycle, access policy, quota and object
// versions. Every call goes through the gateway's allow-listed RustFS proxy
// (`/rustfs/proxy/s3/{bucket}?<sub-resource>` and `/rustfs/proxy/admin/v3/quota/{bucket}`), which
// signs the request and returns RustFS's own XML/JSON verbatim — parsed here, not re-declared.
import { useCallback, useEffect, useState } from "react";
import { apiError, http, toast } from "../api/client";
import type { StorageBucket } from "../api/types";
import { Badge, Button, SlideOver, Tabs } from "../ui/kit";
import { Table } from "../ui/Table";
import { fmtBytes } from "../lib/format";

const S3_NS = "http://s3.amazonaws.com/doc/2006-03-01/";
const TABS = ["Versioning", "Lifecycle", "Access", "Quota", "Versions"] as const;
type TabName = (typeof TABS)[number];

interface Raw {
  status: number;
  text: string;
}

/** One proxied call; never throws on an HTTP error status — callers interpret 404 as "not configured". */
async function call(method: string, url: string, body?: string, contentType?: string): Promise<Raw> {
  const r = await http.request({
    method,
    url,
    data: body,
    headers: body !== undefined && contentType ? { "Content-Type": contentType } : undefined,
    responseType: "text",
    transformResponse: [(d: unknown) => d],
    validateStatus: () => true,
  });
  return { status: r.status, text: typeof r.data === "string" ? r.data : "" };
}

const ok = (r: Raw) => r.status >= 200 && r.status < 300;

/** A readable message from an S3 XML error, an Atlas JSON error, or the raw text. */
function errText(r: Raw): string {
  const m = r.text.match(/<Message>([^<]*)<\/Message>/);
  if (m) return m[1];
  try {
    const j = JSON.parse(r.text);
    return j?.error?.message || j?.message || `HTTP ${r.status}`;
  } catch {
    return r.text.slice(0, 160) || `HTTP ${r.status}`;
  }
}

const s3Url = (bucket: string, sub: string) => `/rustfs/proxy/s3/${encodeURIComponent(bucket)}?${sub}`;
const quotaUrl = (bucket: string) => `/rustfs/proxy/admin/v3/quota/${encodeURIComponent(bucket)}`;

function parseXml(text: string): Document {
  return new DOMParser().parseFromString(text, "application/xml");
}
const firstText = (el: Element | Document, name: string): string =>
  el.getElementsByTagName(name)[0]?.textContent?.trim() ?? "";

export const escapeXml = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&apos;");

// ---------------------------------------------------------------- lifecycle model

export interface LifecycleRule {
  id: string;
  prefix: string;
  status: string;
  days: string;
  noncurrentDays: string;
  /** The rule's XML as RustFS returned it — re-sent untouched so unedited rules (transitions, ...) survive. */
  raw: string;
}

export function parseLifecycle(text: string): LifecycleRule[] {
  const doc = parseXml(text);
  const ser = new XMLSerializer();
  return Array.from(doc.getElementsByTagName("Rule")).map((el) => ({
    id: firstText(el, "ID"),
    prefix: firstText(el, "Prefix"),
    status: firstText(el, "Status"),
    days: el.getElementsByTagName("Expiration")[0]
      ? firstText(el.getElementsByTagName("Expiration")[0], "Days")
      : "",
    noncurrentDays: el.getElementsByTagName("NoncurrentVersionExpiration")[0]
      ? firstText(el.getElementsByTagName("NoncurrentVersionExpiration")[0], "NoncurrentDays")
      : "",
    raw: ser.serializeToString(el),
  }));
}

export function buildRule(id: string, prefix: string, days: string, noncurrentDays: string): string {
  const exp = days ? `<Expiration><Days>${escapeXml(days)}</Days></Expiration>` : "";
  const nc = noncurrentDays
    ? `<NoncurrentVersionExpiration><NoncurrentDays>${escapeXml(noncurrentDays)}</NoncurrentDays></NoncurrentVersionExpiration>`
    : "";
  return `<Rule><ID>${escapeXml(id)}</ID><Filter><Prefix>${escapeXml(prefix)}</Prefix></Filter><Status>Enabled</Status>${exp}${nc}</Rule>`;
}

export const buildLifecycle = (rules: string[]) =>
  `<LifecycleConfiguration xmlns="${S3_NS}">${rules.join("")}</LifecycleConfiguration>`;

// ---------------------------------------------------------------- panel

export default function BucketSettings({ bucket, onClose }: { bucket: StorageBucket | null; onClose: () => void }) {
  const [tab, setTab] = useState<TabName>("Versioning");
  const [versioning, setVersioning] = useState<string | null>(null);
  const name = bucket ? bucket.bucket_name || bucket.name || bucket.id : "";

  const loadVersioning = useCallback(async () => {
    if (!name) return;
    try {
      const r = await call("GET", s3Url(name, "versioning"));
      setVersioning(ok(r) ? firstText(parseXml(r.text), "Status") || "Unversioned" : "Unknown");
    } catch (e) {
      setVersioning("Unknown");
      toast(`versioning: ${apiError(e)}`, "err");
    }
  }, [name]);

  useEffect(() => {
    if (name) loadVersioning();
  }, [name, loadVersioning]);

  if (!bucket) return null;
  const tabs = TABS.filter((t) => t !== "Versions" || versioning === "Enabled" || versioning === "Suspended");
  const close = () => {
    setTab("Versioning");
    setVersioning(null);
    onClose();
  };

  return (
    <SlideOver open={!!bucket} onClose={close} title={<span className="mono">{name} · settings</span>} width={620}>
      <Tabs tabs={[...tabs]} value={tabs.includes(tab) ? tab : "Versioning"} onChange={(t) => setTab(t as TabName)} />
      {tab === "Versioning" && <VersioningTab bucket={name} status={versioning} onChanged={loadVersioning} />}
      {tab === "Lifecycle" && <LifecycleTab bucket={name} />}
      {tab === "Access" && <AccessTab bucket={name} />}
      {tab === "Quota" && <QuotaTab bucket={name} />}
      {tab === "Versions" && <VersionsTab bucket={name} />}
    </SlideOver>
  );
}

// ---------------------------------------------------------------- versioning

function VersioningTab({ bucket, status, onChanged }: { bucket: string; status: string | null; onChanged: () => void }) {
  const [busy, setBusy] = useState(false);
  const set = async (next: "Enabled" | "Suspended") => {
    setBusy(true);
    try {
      const body = `<VersioningConfiguration xmlns="${S3_NS}"><Status>${next}</Status></VersioningConfiguration>`;
      const r = await call("PUT", s3Url(bucket, "versioning"), body, "application/xml");
      if (!ok(r)) throw new Error(errText(r));
      toast(`versioning ${next.toLowerCase()}`, "ok");
      onChanged();
    } catch (e) {
      toast(`versioning: ${e instanceof Error ? e.message : String(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };
  return (
    <div>
      <p className="mb-3">
        Status:{" "}
        <Badge kind={status === "Enabled" ? "success" : status === "Suspended" ? "warning" : "neutral"} dot>
          {status ?? "…"}
        </Badge>
      </p>
      <p className="mb-3" style={{ color: "var(--at-ink-4)" }}>
        With versioning enabled every overwrite and delete keeps the previous version (a delete adds a delete
        marker). Suspending stops creating new versions but keeps the existing ones.
      </p>
      <div className="flex gap-2">
        <Button variant="primary" loading={busy} disabled={status === "Enabled"} onClick={() => set("Enabled")}>
          Enable
        </Button>
        <Button loading={busy} disabled={status !== "Enabled"} onClick={() => set("Suspended")}>
          Suspend
        </Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- lifecycle

function LifecycleTab({ bucket }: { bucket: string }) {
  const [rules, setRules] = useState<LifecycleRule[] | null>(null);
  const [id, setId] = useState("");
  const [prefix, setPrefix] = useState("");
  const [days, setDays] = useState("");
  const [ncDays, setNcDays] = useState("");
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const r = await call("GET", s3Url(bucket, "lifecycle"));
      setRules(ok(r) ? parseLifecycle(r.text) : []);
      if (!ok(r) && r.status !== 404) toast(`lifecycle: ${errText(r)}`, "err");
    } catch (e) {
      setRules([]);
      toast(`lifecycle: ${apiError(e)}`, "err");
    }
  }, [bucket]);
  useEffect(() => {
    load();
  }, [load]);

  const save = async (next: LifecycleRule[]) => {
    setBusy(true);
    try {
      const r = next.length
        ? await call("PUT", s3Url(bucket, "lifecycle"), buildLifecycle(next.map((x) => x.raw)), "application/xml")
        : await call("DELETE", s3Url(bucket, "lifecycle"));
      if (!ok(r) && r.status !== 404) throw new Error(errText(r));
      toast("lifecycle saved", "ok");
      await load();
    } catch (e) {
      toast(`lifecycle: ${e instanceof Error ? e.message : String(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };

  const add = () => {
    const ruleId = id.trim();
    if (!ruleId || (!days.trim() && !ncDays.trim())) {
      toast("rule needs an ID and an expiration (days and/or noncurrent days)", "err");
      return;
    }
    const raw = buildRule(ruleId, prefix.trim(), days.trim(), ncDays.trim());
    save([...(rules ?? []), { id: ruleId, prefix, status: "Enabled", days, noncurrentDays: ncDays, raw }]);
    setId("");
    setPrefix("");
    setDays("");
    setNcDays("");
  };

  return (
    <div>
      <Table
        rows={rules ?? undefined}
        rowKey={(r) => r.id + r.raw.length}
        cols={[
          { h: "ID", f: (r) => r.id || "—", mono: true },
          { h: "Prefix", f: (r) => r.prefix || "(all)", mono: true },
          { h: "Status", f: (r) => <Badge kind={r.status === "Enabled" ? "success" : "neutral"}>{r.status || "?"}</Badge> },
          { h: "Expire", f: (r) => (r.days ? `${r.days} d` : "—") },
          { h: "Noncurrent", f: (r) => (r.noncurrentDays ? `${r.noncurrentDays} d` : "—") },
        ]}
        actions={(r) => (
          <Button size="sm" variant="danger" loading={busy} onClick={() => save((rules ?? []).filter((x) => x !== r))}>
            Del
          </Button>
        )}
        empty="No lifecycle rules."
      />
      <div className="mt-4 grid gap-2">
        <div className="at-caption">Add rule</div>
        <input className="field" placeholder="rule id" value={id} onChange={(e) => setId(e.target.value)} />
        <input className="field" placeholder="prefix (empty = whole bucket)" value={prefix} onChange={(e) => setPrefix(e.target.value)} />
        <input className="field" type="number" min={1} placeholder="expire current versions after (days)" value={days} onChange={(e) => setDays(e.target.value)} />
        <input className="field" type="number" min={1} placeholder="expire noncurrent versions after (days, optional)" value={ncDays} onChange={(e) => setNcDays(e.target.value)} />
        <div>
          <Button variant="primary" loading={busy} onClick={add}>
            Add rule
          </Button>
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- access policy

const publicReadPolicy = (bucket: string) =>
  JSON.stringify(
    {
      Version: "2012-10-17",
      Statement: [
        {
          Effect: "Allow",
          Principal: { AWS: ["*"] },
          Action: ["s3:GetObject"],
          Resource: [`arn:aws:s3:::${bucket}/*`],
        },
      ],
    },
    null,
    2,
  );

function AccessTab({ bucket }: { bucket: string }) {
  const [text, setText] = useState("");
  const [configured, setConfigured] = useState<boolean | null>(null);
  const [confirmPublic, setConfirmPublic] = useState(false);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const r = await call("GET", s3Url(bucket, "policy"));
      if (ok(r)) {
        setConfigured(true);
        try {
          setText(JSON.stringify(JSON.parse(r.text), null, 2));
        } catch {
          setText(r.text);
        }
      } else {
        setConfigured(false);
        setText("");
        if (r.status !== 404) toast(`policy: ${errText(r)}`, "err");
      }
    } catch (e) {
      setConfigured(false);
      toast(`policy: ${apiError(e)}`, "err");
    }
  }, [bucket]);
  useEffect(() => {
    load();
  }, [load]);

  const put = async (json: string) => {
    setBusy(true);
    try {
      JSON.parse(json);
      const r = await call("PUT", s3Url(bucket, "policy"), json, "application/json");
      if (!ok(r)) throw new Error(errText(r));
      toast("policy applied", "ok");
      setConfirmPublic(false);
      await load();
    } catch (e) {
      toast(`policy: ${e instanceof Error ? e.message : String(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };
  const makePrivate = async () => {
    setBusy(true);
    try {
      const r = await call("DELETE", s3Url(bucket, "policy"));
      if (!ok(r) && r.status !== 404) throw new Error(errText(r));
      toast("bucket is private", "ok");
      await load();
    } catch (e) {
      toast(`policy: ${e instanceof Error ? e.message : String(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      <p className="mb-3">
        Access:{" "}
        <Badge kind={configured ? "warning" : "success"} dot>
          {configured === null ? "…" : configured ? "custom policy" : "private"}
        </Badge>
      </p>
      <div className="flex gap-2 mb-3">
        <Button loading={busy} onClick={makePrivate}>
          Private
        </Button>
        {!confirmPublic ? (
          <Button onClick={() => setConfirmPublic(true)}>Public read…</Button>
        ) : (
          <Button variant="danger" loading={busy} onClick={() => put(publicReadPolicy(bucket))}>
            Confirm: make every object world-readable
          </Button>
        )}
      </div>
      <textarea
        className="field mono"
        style={{ width: "100%", minHeight: 220 }}
        placeholder='Bucket policy JSON (empty = private). Example: {"Version":"2012-10-17","Statement":[...]}'
        value={text}
        onChange={(e) => setText(e.target.value)}
      />
      <div className="mt-2">
        <Button variant="primary" loading={busy} disabled={!text.trim()} onClick={() => put(text)}>
          Apply policy
        </Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- quota

interface QuotaInfo {
  quota?: number | null;
  quota_type?: string;
}
interface QuotaStats {
  quota_limit?: number | null;
  current_usage?: number;
  remaining_quota?: number | null;
  usage_percentage?: number | null;
}

function QuotaTab({ bucket }: { bucket: string }) {
  const [quota, setQuota] = useState<QuotaInfo | null>(null);
  const [stats, setStats] = useState<QuotaStats | null>(null);
  const [gb, setGb] = useState("");
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const q = await call("GET", quotaUrl(bucket));
      setQuota(ok(q) ? (JSON.parse(q.text) as QuotaInfo) : {});
      const s = await call("GET", `/rustfs/proxy/admin/v3/quota-stats/${encodeURIComponent(bucket)}`);
      setStats(ok(s) ? (JSON.parse(s.text) as QuotaStats) : null);
    } catch (e) {
      setQuota({});
      toast(`quota: ${apiError(e)}`, "err");
    }
  }, [bucket]);
  useEffect(() => {
    load();
  }, [load]);

  const set = async () => {
    const n = Number(gb);
    if (!(n > 0)) {
      toast("enter a quota in GB (> 0)", "err");
      return;
    }
    setBusy(true);
    try {
      const r = await call("PUT", quotaUrl(bucket), JSON.stringify({ quota: Math.round(n * 1024 ** 3), quota_type: "HARD" }), "application/json");
      if (!ok(r)) throw new Error(errText(r));
      toast("quota set", "ok");
      setGb("");
      await load();
    } catch (e) {
      toast(`quota: ${e instanceof Error ? e.message : String(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };
  const clear = async () => {
    setBusy(true);
    try {
      const r = await call("DELETE", quotaUrl(bucket));
      if (!ok(r)) throw new Error(errText(r));
      toast("quota cleared", "ok");
      await load();
    } catch (e) {
      toast(`quota: ${e instanceof Error ? e.message : String(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };

  const limit = quota?.quota ?? stats?.quota_limit ?? null;
  return (
    <div>
      <p className="mb-2">
        Hard quota: <b>{limit ? fmtBytes(limit) : "none"}</b>
      </p>
      {stats && (
        <p className="mb-3" style={{ color: "var(--at-ink-4)" }}>
          Used {fmtBytes(stats.current_usage ?? 0)}
          {stats.usage_percentage != null ? ` (${stats.usage_percentage.toFixed(1)}%)` : ""}
          {stats.remaining_quota != null ? ` · ${fmtBytes(stats.remaining_quota)} remaining` : ""}
        </p>
      )}
      <div className="flex gap-2 items-center">
        <input className="field" style={{ width: 160 }} type="number" min={0} step="any" placeholder="quota (GB)" value={gb} onChange={(e) => setGb(e.target.value)} />
        <Button variant="primary" loading={busy} onClick={set}>
          Set quota
        </Button>
        <Button variant="danger" loading={busy} disabled={!limit} onClick={clear}>
          Clear
        </Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- versions

interface ObjectVersion {
  key: string;
  versionId: string;
  isLatest: boolean;
  size: string;
  lastModified: string;
  deleteMarker: boolean;
}

export function parseVersions(text: string): ObjectVersion[] {
  const doc = parseXml(text);
  const rows: ObjectVersion[] = [];
  for (const el of Array.from(doc.getElementsByTagName("Version"))) {
    rows.push({
      key: firstText(el, "Key"),
      versionId: firstText(el, "VersionId"),
      isLatest: firstText(el, "IsLatest") === "true",
      size: firstText(el, "Size"),
      lastModified: firstText(el, "LastModified"),
      deleteMarker: false,
    });
  }
  for (const el of Array.from(doc.getElementsByTagName("DeleteMarker"))) {
    rows.push({
      key: firstText(el, "Key"),
      versionId: firstText(el, "VersionId"),
      isLatest: firstText(el, "IsLatest") === "true",
      size: "",
      lastModified: firstText(el, "LastModified"),
      deleteMarker: true,
    });
  }
  return rows.sort((a, b) => a.key.localeCompare(b.key) || b.lastModified.localeCompare(a.lastModified));
}

function VersionsTab({ bucket }: { bucket: string }) {
  const [rows, setRows] = useState<ObjectVersion[] | null>(null);
  const load = useCallback(async () => {
    try {
      const r = await call("GET", s3Url(bucket, "versions"));
      if (!ok(r)) throw new Error(errText(r));
      setRows(parseVersions(r.text));
    } catch (e) {
      setRows([]);
      toast(`versions: ${e instanceof Error ? e.message : String(e)}`, "err");
    }
  }, [bucket]);
  useEffect(() => {
    load();
  }, [load]);
  return (
    <div>
      <div className="mb-3">
        <Button onClick={load}>Refresh</Button>
      </div>
      <Table
        rows={rows ?? undefined}
        rowKey={(v) => `${v.key}@${v.versionId}${v.deleteMarker ? "#dm" : ""}`}
        cols={[
          { h: "Key", f: (v) => v.key, mono: true },
          { h: "Version", f: (v) => v.versionId || "null", mono: true },
          { h: "Latest", f: (v) => (v.isLatest ? "yes" : "") },
          { h: "Size", f: (v) => (v.deleteMarker ? "—" : fmtBytes(Number(v.size) || 0)) },
          { h: "Modified", f: (v) => v.lastModified },
          { h: "Marker", f: (v) => (v.deleteMarker ? <Badge kind="warning">delete marker</Badge> : "") },
        ]}
        empty="No object versions."
      />
    </div>
  );
}
