// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// RustFS administration: cluster state, drives/pools (with disk formatting), and IAM access — all via
// RustFS's own admin API, forwarded and signed by the gateway (`/rustfs/proxy/admin/...`).
import { useState } from "react";
import { Tabs } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import OverviewTab from "./rustfs/OverviewTab";
import DrivesTab from "./rustfs/DrivesTab";
import AccessTab from "./rustfs/AccessTab";

const TABS = ["Overview", "Drives & pools", "Access"];

export default function RustfsAdmin() {
  const [tab, setTab] = useState(TABS[0]);
  return (
    <ListPage
      crumbs={navCrumbs("rustfs")}
      eyebrow="STORAGE · OBJECT"
      title="RustFS"
      state="Cluster health, drives and pools, and users, policies and service accounts — the S3 object backend's own admin API."
    >
      <Tabs tabs={TABS} value={tab} onChange={setTab} />
      {tab === "Overview" && <OverviewTab />}
      {tab === "Drives & pools" && <DrivesTab />}
      {tab === "Access" && <AccessTab />}
    </ListPage>
  );
}
