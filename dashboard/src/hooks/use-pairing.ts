import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { apiFetch } from "@/shared/api-fetch";

/** `GET /api/pairing/info`. A node has one owner: `owner_id`/`paired_at` are
 *  set only while paired, `pairing_code` only while unpaired (and only to a
 *  caller on the node's own networks). */
export interface PairingInfo {
  device_id: string;
  name?: string;
  paired: boolean;
  pairing_code: string | null;
  owner_id: string | null;
  /** Unix seconds. */
  paired_at: number | null;
}

export function usePairingInfo() {
  return useQuery<PairingInfo>({
    queryKey: ["pairing-info"],
    queryFn: ({ signal }) => apiFetch<PairingInfo>("/api/pairing/info", { signal }),
    refetchInterval: 5_000,
  });
}

/** The agent's /api/pairing/accept response envelope. The route ALWAYS returns
 * HTTP 200 and carries the real outcome in `ok`; a `{ok:false}` body is a
 * FAILURE, not a success. */
interface AcceptCodeResponse {
  ok: boolean;
  error?: string | null;
  message?: string | null;
}

export function useAcceptCode() {
  const qc = useQueryClient();
  return useMutation({
    // The accept route is HTTP-200-on-failure: the outcome is in the `ok` field,
    // never the status code. Inspect it and throw on `ok !== true` so the form
    // surfaces the real error instead of printing "paired successfully" off a
    // resolved 200 with a `{ok:false}` body ("presence is not proof").
    mutationFn: async (code: string) => {
      const res = await apiFetch<AcceptCodeResponse>("/api/pairing/accept", {
        method: "POST",
        body: { code },
      });
      if (!res || res.ok !== true) {
        throw new Error(
          res?.message || res?.error || "Pairing failed. Check the code and try again.",
        );
      }
      return res;
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["pairing-info"] }),
  });
}

export function useUnpair() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => apiFetch("/api/pairing/unpair", { method: "POST" }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["pairing-info"] }),
  });
}

/** One phone receiver waiting for approval on a ground station
 *  (`GET /api/v1/ground-station/wfb/invite`). */
export interface PhoneInvite {
  invite_id: string;
  label: string;
  /** SHA-256 prefix of the phone's key, also shown on the phone. */
  phone_fingerprint: string;
  expires_at_ms: number;
}

export function usePhoneInvites() {
  return useQuery<{ pending: PhoneInvite[] }>({
    queryKey: ["phone-invites"],
    queryFn: ({ signal }) =>
      apiFetch<{ pending: PhoneInvite[] }>("/api/v1/ground-station/wfb/invite", { signal }),
    refetchInterval: 2_000,
  });
}

export function useDecidePhoneInvite() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, decision }: { id: string; decision: "approve" | "reject" }) =>
      apiFetch(`/api/v1/ground-station/wfb/invite/${encodeURIComponent(id)}/${decision}`, {
        method: "POST",
      }),
    onSettled: () => qc.invalidateQueries({ queryKey: ["phone-invites"] }),
  });
}
