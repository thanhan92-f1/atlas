// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useState } from "react";
import { Copyable, Badge, Button, FormModal, Modal, SlideOver, Tabs } from "../../ui/kit";
import { Table } from "../../ui/Table";
import { del } from "../../ui/confirm";
import { toast } from "../../api/client";
import { asArr, asRec, asStr, rfsWrite, useRefreshRustfs, useRustfs, type Rec } from "./api";
import { Dynamic, EditModal, LoadError, Muted, RawJson, Section } from "./common";

const SUBTABS = ["Users", "Groups", "Policies", "Service accounts"];
const POLICY_TEMPLATE = `{
  "Version": "2012-10-17",
  "Statement": [
    { "Effect": "Allow", "Action": ["s3:GetObject", "s3:ListBucket"], "Resource": ["arn:aws:s3:::*"] }
  ]
}`;

function parseJson(text: string, what: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    toast(`${what} is not valid JSON`, "err");
    throw new Error("invalid json");
  }
}

const SECRET_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const randomSecret = () => Array.from(crypto.getRandomValues(new Uint8Array(32)), (b) => SECRET_CHARS[b % SECRET_CHARS.length]).join("");

const statusBadge = (s: string) => <Badge kind={s === "enabled" ? "success" : "neutral"} dot>{s || "unknown"}</Badge>;

function UsersPane({ policies }: { policies: string[] }) {
  const refresh = useRefreshRustfs();
  const users = useRustfs("list-users");
  const [add, setAdd] = useState(false);
  const [assign, setAssign] = useState<string | null>(null);
  const [created, setCreated] = useState<{ accessKey: string; secretKey: string } | null>(null);
  const rows = Object.entries(asRec(users.data));

  return (
    <Section title="Users" actions={<Button size="sm" variant="primary" onClick={() => setAdd(true)}>Add user</Button>}>
      {users.error && <LoadError error={users.error} />}
      <Table<[string, unknown]>
        soundings
        panelTitle="User index"
        rows={users.data ? rows : undefined}
        rowKey={([k]) => k}
        empty="No IAM users — the root credential is the only identity."
        cols={[
          { h: "Access key", f: ([k]) => k, mono: true },
          { h: "Status", f: ([, u]) => statusBadge(asStr(asRec(u).status)) },
          { h: "Policy", f: ([, u]) => asStr(asRec(u).policyName) || "—" },
          { h: "Groups", f: ([, u]) => asArr(asRec(u).memberOf).map(asStr).join(", ") || "—" },
        ]}
        actions={([k, u]) => (
          <>
            <Button size="sm" onClick={() => setAssign(k)}>Policy…</Button>
            <Button
              size="sm"
              onClick={() =>
                rfsWrite(
                  `${asStr(asRec(u).status) === "enabled" ? "disable" : "enable"} ${k}`,
                  "put",
                  "set-user-status",
                  { params: { accessKey: k, status: asStr(asRec(u).status) === "enabled" ? "disabled" : "enabled" } },
                  refresh,
                ).catch(() => {})
              }
            >
              {asStr(asRec(u).status) === "enabled" ? "Disable" : "Enable"}
            </Button>
            <Button size="sm" variant="danger" onClick={() => del(`user ${k}`, () => rfsWrite("remove user", "delete", "remove-user", { params: { accessKey: k } }, refresh))}>Del</Button>
          </>
        )}
      />
      <EditModal
        open={add}
        onClose={() => setAdd(false)}
        title="Add user"
        submitLabel="Create user"
        fields={[
          { name: "accessKey", label: "Access key", placeholder: "alice" },
          { name: "secretKey", label: "Secret key (optional)", type: "password", optional: true, hint: "At least 8 characters. Leave empty to generate one — it is shown once." },
        ]}
        onSubmit={async (v) => {
          const secretKey = v.secretKey || randomSecret();
          await rfsWrite("add user", "put", "add-user", { params: { accessKey: v.accessKey.trim() }, data: { secretKey, status: "enabled" } }, refresh);
          if (!v.secretKey) setCreated({ accessKey: v.accessKey.trim(), secretKey });
        }}
      />
      <Modal
        open={created !== null}
        onClose={() => setCreated(null)}
        title="User created"
        footer={<Button variant="primary" onClick={() => setCreated(null)}>I have saved the secret</Button>}
      >
        <Muted>The generated secret key is shown once and cannot be retrieved again — copy it now.</Muted>
        <div className="at-caption">Access key</div>
        <Copyable text={created?.accessKey ?? ""} />
        <div className="at-caption" style={{ marginTop: 12 }}>Secret key</div>
        <Copyable text={created?.secretKey ?? ""} />
      </Modal>
      <FormModal
        open={assign !== null}
        onClose={() => setAssign(null)}
        title={`Policy for ${assign ?? ""}`}
        submitLabel="Attach"
        fields={[{ name: "policy", label: "Policy", options: policies.map((p) => ({ value: p, label: p })), hint: policies.length ? undefined : "No policies yet — add one on the Policies tab." }]}
        onSubmit={async (v) => {
          await rfsWrite("attach policy", "put", "set-user-or-group-policy", { params: { policyName: v.policy, userOrGroup: assign ?? "", isGroup: false } }, refresh);
        }}
      />
    </Section>
  );
}

function GroupsPane() {
  const refresh = useRefreshRustfs();
  const groups = useRustfs("groups");
  const [form, setForm] = useState<{ group: string } | null>(null);
  const [detail, setDetail] = useState<string | null>(null);
  const detailQ = useRustfs("group", { group: detail ?? "" }, 15000, detail !== null);
  const names = Array.isArray(groups.data) ? groups.data.map(asStr) : asArr(asRec(groups.data).groups).map(asStr);

  return (
    <Section title="Groups" actions={<Button size="sm" variant="primary" onClick={() => setForm({ group: "" })}>Add group / members</Button>}>
      {groups.error && <LoadError error={groups.error} />}
      <Table<string>
        soundings
        panelTitle="Group index"
        rows={groups.data != null ? names : undefined}
        rowKey={(g) => g}
        empty="No groups."
        cols={[{ h: "Group", f: (g) => g, mono: true }]}
        actions={(g) => (
          <>
            <Button size="sm" onClick={() => setDetail(g)}>Members</Button>
            <Button size="sm" onClick={() => setForm({ group: g })}>Edit…</Button>
            <Button size="sm" variant="danger" onClick={() => del(`group ${g}`, () => rfsWrite("delete group", "delete", `group/${encodeURIComponent(g)}`, {}, refresh))}>Del</Button>
          </>
        )}
      />
      <SlideOver open={detail !== null} onClose={() => setDetail(null)} title={`GROUP · ${detail ?? ""}`}>
        {detailQ.error ? <LoadError error={detailQ.error} /> : detailQ.data != null && <Dynamic title="Group" data={detailQ.data} />}
        {detailQ.data != null && <RawJson title="Raw group" value={detailQ.data} />}
      </SlideOver>
      <FormModal
        open={form !== null}
        onClose={() => setForm(null)}
        title="Group members"
        submitLabel="Apply"
        fields={[
          { name: "group", label: "Group", value: form?.group ?? "" },
          { name: "members", label: "Members (access keys, comma separated)" },
          { name: "action", label: "Action", options: [{ value: "add", label: "Add members (creates the group if new)" }, { value: "remove", label: "Remove members" }] },
        ]}
        onSubmit={async (v) => {
          await rfsWrite(
            "update group",
            "put",
            "update-group-members",
            {
              data: {
                group: v.group.trim(),
                members: v.members.split(",").map((m) => m.trim()).filter(Boolean),
                isRemove: v.action === "remove",
                groupStatus: "enabled",
              },
            },
            refresh,
          );
        }}
      />
    </Section>
  );
}

function PoliciesPane({ data, error }: { data: unknown; error: unknown }) {
  const refresh = useRefreshRustfs();
  const [add, setAdd] = useState(false);
  const [view, setView] = useState<string | null>(null);
  const map = asRec(data);
  const rows = Object.entries(map);

  return (
    <Section title="Policies" actions={<Button size="sm" variant="primary" onClick={() => setAdd(true)}>Add policy</Button>}>
      {error != null && <LoadError error={error} />}
      <Table<[string, unknown]>
        soundings
        panelTitle="Policy index"
        rows={data != null ? rows : undefined}
        rowKey={([k]) => k}
        empty="No policies."
        cols={[
          { h: "Name", f: ([k]) => k, mono: true },
          { h: "Statements", f: ([, p]) => asArr(asRec(p).Statement).length || "—" },
        ]}
        actions={([k]) => (
          <>
            <Button size="sm" onClick={() => setView(k)}>View</Button>
            <Button size="sm" variant="danger" onClick={() => del(`policy ${k}`, () => rfsWrite("remove policy", "delete", "remove-canned-policy", { params: { name: k } }, refresh))}>Del</Button>
          </>
        )}
      />
      <SlideOver open={view !== null} onClose={() => setView(null)} title={`POLICY · ${view ?? ""}`}>
        <RawJson title="Policy document" value={view ? map[view] : null} />
      </SlideOver>
      <EditModal
        open={add}
        onClose={() => setAdd(false)}
        title="Add policy"
        submitLabel="Save policy"
        fields={[
          { name: "name", label: "Name", placeholder: "read-only" },
          { name: "policy", label: "Policy document (JSON)", type: "textarea", value: POLICY_TEMPLATE },
        ]}
        onSubmit={(v) => rfsWrite("add policy", "put", "add-canned-policy", { params: { name: v.name.trim() }, data: parseJson(v.policy, "the policy") }, refresh)}
      />
    </Section>
  );
}

function ServiceAccountsPane({ users }: { users: string[] }) {
  const refresh = useRefreshRustfs();
  const [user, setUser] = useState("");
  const list = useRustfs("list-service-accounts", user.trim() ? { user: user.trim() } : undefined);
  const [add, setAdd] = useState(false);
  const [created, setCreated] = useState<{ accessKey: string; secretKey: string } | null>(null);
  const accounts = (Array.isArray(list.data) ? list.data : asArr(asRec(list.data).accounts)).map(asRec);

  return (
    <Section title="Service accounts" actions={<Button size="sm" variant="primary" onClick={() => setAdd(true)}>Add service account</Button>}>
      <div style={{ marginBottom: 12 }}>
        <input
          className="field"
          list="rustfs-users"
          placeholder="Owner access key (blank = the gateway's own credential)"
          value={user}
          onChange={(e) => setUser(e.target.value)}
        />
        <datalist id="rustfs-users">{users.map((u) => <option key={u} value={u} />)}</datalist>
      </div>
      {list.error && <LoadError error={list.error} />}
      <Table<Rec>
        soundings
        panelTitle="Service account index"
        rows={list.data != null ? accounts : undefined}
        rowKey={(a, n) => asStr(a.accessKey) || String(n)}
        empty="No service accounts for this owner."
        cols={[
          { h: "Access key", f: (a) => asStr(a.accessKey), mono: true },
          { h: "Name", f: (a) => asStr(a.name) || "—" },
          { h: "Status", f: (a) => statusBadge(asStr(a.accountStatus || a.status)) },
          { h: "Parent", f: (a) => asStr(a.parentUser) || "—", mono: true },
          { h: "Expires", f: (a) => asStr(a.expiration) || "never" },
        ]}
        actions={(a) => (
          <Button size="sm" variant="danger" onClick={() => del(`service account ${asStr(a.accessKey)}`, () => rfsWrite("delete service account", "delete", "delete-service-account", { params: { accessKey: asStr(a.accessKey) } }, refresh))}>Del</Button>
        )}
      />
      <EditModal
        open={add}
        onClose={() => setAdd(false)}
        title="Add service account"
        submitLabel="Create"
        fields={[
          { name: "targetUser", label: "Owner access key (optional)", optional: true, value: user },
          { name: "name", label: "Name (optional)", optional: true },
          { name: "policy", label: "Restricting policy (JSON, optional)", type: "textarea", optional: true, hint: "Blank = inherits the owner's permissions." },
        ]}
        onSubmit={async (v) => {
          const data: Rec = {};
          if (v.targetUser.trim()) data.targetUser = v.targetUser.trim();
          if (v.name.trim()) data.name = v.name.trim();
          if (v.policy.trim()) data.policy = parseJson(v.policy, "the policy");
          const res = asRec(asRec(await rfsWrite("add service account", "put", "add-service-account", { data }, refresh)).credentials);
          setCreated({ accessKey: asStr(res.accessKey), secretKey: asStr(res.secretKey) });
        }}
      />
      <Modal
        open={created !== null}
        onClose={() => setCreated(null)}
        title="Service account created"
        footer={<Button variant="primary" onClick={() => setCreated(null)}>I have saved the secret</Button>}
      >
        <Muted>The secret key is shown once and cannot be retrieved again — copy it now.</Muted>
        <div className="at-caption">Access key</div>
        <Copyable text={created?.accessKey ?? ""} />
        <div className="at-caption" style={{ marginTop: 12 }}>Secret key</div>
        <Copyable text={created?.secretKey ?? ""} />
      </Modal>
    </Section>
  );
}

export default function AccessTab() {
  const [tab, setTab] = useState(SUBTABS[0]);
  const policies = useRustfs("list-canned-policies");
  const users = useRustfs("list-users");
  return (
    <>
      <Tabs tabs={SUBTABS} value={tab} onChange={setTab} />
      {tab === "Users" && <UsersPane policies={Object.keys(asRec(policies.data))} />}
      {tab === "Groups" && <GroupsPane />}
      {tab === "Policies" && <PoliciesPane data={policies.data} error={policies.error} />}
      {tab === "Service accounts" && <ServiceAccountsPane users={Object.keys(asRec(users.data))} />}
    </>
  );
}
