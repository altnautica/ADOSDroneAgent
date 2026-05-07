import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { apiFetch } from "@/lib/api";

export interface PairingInfo {
  drone_id: string;
  paired: boolean;
  pairing_code?: string;
  code_expires_at?: string | null;
  paired_with?: PairedDevice[];
  beacon_state?: string;
}

export interface PairedDevice {
  client_id: string;
  paired_at: string;
  last_seen?: string | null;
  display_name?: string;
}

export function usePairingInfo() {
  return useQuery<PairingInfo>({
    queryKey: ["pairing-info"],
    queryFn: ({ signal }) => apiFetch<PairingInfo>("/api/pairing/info", { signal }),
    refetchInterval: 5_000,
  });
}

export function useAcceptCode() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (code: string) =>
      apiFetch("/api/pairing/accept", { method: "POST", body: { code } }),
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
