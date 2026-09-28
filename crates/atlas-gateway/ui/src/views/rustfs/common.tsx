// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useEffect, useState } from "react";
import { Button, Field, Label, Modal, colorizeJson } from "../../ui/kit";
import { Table } from "../../ui/Table";
import { asArr, asRec, rfsError, type Rec } from "./api";

export function RawJson({ title, value }: { title: string; value: unknown }) {
  return (
    <details className="at-panel" style={{ marginBottom: 12, padding: 12 }}>
      <summary className="at-caption" style={{ cursor: "pointer" }}>{title}</summary>
      <pre className="mono" style={{ marginTop: 8, overflow: "auto", fontSize: 12 }}>
        {colorizeJson(JSON.stringify(value ?? null, null, 2))}
      </pre>
    </details>
  );
}

export function LoadError({ error }: { error: unknown }) {
  return (
    <div className="at-panel" style={{ padding: 16, marginBottom: 12, color: "var(--at-danger, #e5484d)" }}>
      {rfsError(error)}
    </div>
  );
}

export const StatGrid = ({ children }: { children: React.ReactNode }) => (
  <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit,minmax(200px,1fr))", gap: 12, marginBottom: 16 }}>
    {children}
  </div>
);

export const Section = ({ title, actions, children }: { title: string; actions?: React.ReactNode; children: React.ReactNode }) => (
  <section style={{ marginBottom: 24 }}>
    <div style={{ display: "flex", alignItems: "center", gap: 12, marginBottom: 8 }}>
      <div className="at-caption" style={{ flex: 1, color: "var(--at-ink)", fontSize: 13 }}>{title}</div>
      {actions}
    </div>
    {children}
  </section>
);

/** A table whose columns are the scalar keys of an array of unknown-shaped objects. */
export function AutoTable({ title, rows }: { title: string; rows: unknown[] }) {
  const recs = rows.map(asRec);
  const keys = [...new Set(recs.flatMap((r) => Object.keys(r).filter((k) => typeof r[k] !== "object" || r[k] === null)))].slice(0, 8);
  return (
    <Table<Rec>
      soundings
      panelTitle={title}
      rows={recs}
      rowKey={(_, i) => String(i)}
      empty="Nothing reported."
      cols={keys.map((k) => ({ h: k, f: (r: Rec) => String(r[k] ?? "") }))}
    />
  );
}

export type EField = {
  name: string;
  label: string;
  type?: "text" | "password" | "textarea";
  value?: string;
  optional?: boolean;
  hint?: string;
  placeholder?: string;
};

/** A small form modal for the inputs the kit's FormModal cannot do (password, multi-line JSON). */
export function EditModal({
  open,
  onClose,
  title,
  fields,
  submitLabel = "Save",
  danger,
  onSubmit,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  fields: EField[];
  submitLabel?: string;
  danger?: boolean;
  onSubmit: (vals: Record<string, string>) => Promise<unknown>;
}) {
  const [vals, setVals] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (open) {
      // Re-seed on open only — reset-on-open, not a sync loop.
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setVals(Object.fromEntries(fields.map((f) => [f.name, f.value ?? ""])));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);
  const missing = fields.some((f) => !f.optional && !(vals[f.name] ?? "").trim());
  const submit = async () => {
    setBusy(true);
    try {
      await onSubmit(vals);
      onClose();
    } catch {
      /* the write helper already toasted; keep the form open */
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal
      open={open}
      onClose={onClose}
      title={title}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant={danger ? "danger" : "primary"} loading={busy} disabled={missing} onClick={submit}>
            {submitLabel}
          </Button>
        </>
      }
    >
      <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
        {fields.map((f) => (
          <div key={f.name}>
            <Label>{f.label}</Label>
            {f.type === "textarea" ? (
              <textarea
                className="field mono"
                rows={10}
                style={{ width: "100%" }}
                value={vals[f.name] ?? ""}
                placeholder={f.placeholder}
                onChange={(e) => setVals((s) => ({ ...s, [f.name]: e.target.value }))}
              />
            ) : (
              <Field
                type={f.type === "password" ? "password" : "text"}
                autoComplete="off"
                value={vals[f.name] ?? ""}
                placeholder={f.placeholder}
                onChange={(e) => setVals((s) => ({ ...s, [f.name]: e.target.value }))}
              />
            )}
            {f.hint && <div className="at-caption" style={{ marginTop: 4 }}>{f.hint}</div>}
          </div>
        ))}
      </div>
    </Modal>
  );
}

export const escapeRegExp = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/** An unknown-shaped admin response: an array (or an object holding one) as a table, else raw JSON. */
export function Dynamic({ title, data }: { title: string; data: unknown }) {
  if (Array.isArray(data)) return <AutoTable title={title} rows={data} />;
  const nested = Object.values(asRec(data)).find(Array.isArray);
  if (nested) return <AutoTable title={title} rows={asArr(nested)} />;
  return <RawJson title={title} value={data} />;
}

export const Muted = ({ children }: { children: React.ReactNode }) => (
  <div className="at-caption" style={{ padding: "8px 0" }}>{children}</div>
);
