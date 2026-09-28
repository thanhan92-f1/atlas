// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Calls to RustFS's native admin API through the gateway's allow-listed, server-signed proxy.
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { http, isUnauthorized, toast } from "../../api/client";
import { useUi } from "../../store/ui";

const BASE = "/rustfs/proxy/admin/v3";

export type Rec = Record<string, unknown>;
export type Params = Record<string, string | number | boolean | undefined>;

export const asRec = (v: unknown): Rec => (v && typeof v === "object" && !Array.isArray(v) ? (v as Rec) : {});
export const asArr = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
export const asNum = (v: unknown): number => (typeof v === "number" ? v : Number(v) || 0);
export const asStr = (v: unknown): string => (typeof v === "string" ? v : v == null ? "" : String(v));

/** RustFS answers errors as JSON `{message}` / `{Message}` or S3 XML `<Message>…</Message>`. */
export function rfsError(e: unknown): string {
  const r = (e as { response?: { data?: unknown; status?: number } })?.response;
  const d = r?.data;
  if (typeof d === "string") {
    const m = d.match(/<Message>([^<]*)<\/Message>/);
    return m?.[1] || d.slice(0, 200) || `HTTP ${r?.status}`;
  }
  const rec = asRec(d);
  const err = asRec(rec.error);
  return asStr(err.message || rec.message || rec.Message) || (e as { message?: string })?.message || "request failed";
}

export async function rfsGet<T = unknown>(rel: string, params?: Params): Promise<T> {
  return (await http.get<T>(`${BASE}/${rel}`, { params })).data;
}

/** A write through the proxy: toasts the outcome, refetches via `after`, rethrows on failure. */
export async function rfsWrite(
  label: string,
  method: "put" | "post" | "delete",
  rel: string,
  opts: { params?: Params; data?: unknown } = {},
  after?: () => void,
): Promise<unknown> {
  try {
    const { data } = await http.request({
      method,
      url: `${BASE}/${rel}`,
      params: opts.params,
      data: opts.data,
    });
    toast(`${label} ✓`, "ok");
    after?.();
    return data;
  } catch (e) {
    if (!isUnauthorized(e)) toast(`${label}: ${rfsError(e)}`, "err");
    throw e;
  }
}

export function useRustfs<T = unknown>(rel: string, params?: Params, refetch = 15000, enabled = true) {
  return useQuery<T>({
    queryKey: ["rustfs", rel, params ?? null],
    queryFn: () => rfsGet<T>(rel, params),
    refetchInterval: () => (useUi.getState().paused ? false : refetch),
    enabled,
    retry: false,
  });
}

export function useRefreshRustfs() {
  const qc = useQueryClient();
  return () => qc.invalidateQueries({ queryKey: ["rustfs"] });
}
