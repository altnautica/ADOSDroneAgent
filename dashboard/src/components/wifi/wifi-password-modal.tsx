import { useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ApiError } from "@/lib/api";
import { toast, toastFromError } from "@/lib/toast";
import { isSecured, joinWifi } from "@/lib/wifi";

interface Target {
  ssid: string;
  security: string;
  saved: boolean;
}

interface Props {
  target: Target | null;
  onClose: () => void;
  onJoined: () => void;
}

export function WifiPasswordModal({ target, onClose, onJoined }: Props) {
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setPassword("");
    setError(null);
    setBusy(false);
  }, [target?.ssid]);

  if (!target) {
    return (
      <Dialog open={false} onOpenChange={(open) => !open && onClose()}>
        <DialogContent />
      </Dialog>
    );
  }

  const secured = isSecured(target.security);

  async function attemptJoin(force = false) {
    if (!target) return;
    if (secured && !target.saved && !password) {
      setError("Password required.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const res = await joinWifi(
        target.ssid,
        secured ? password || null : null,
        force,
      );
      if (res.joined) {
        toast.ok(
          `Joined "${target.ssid}"${res.ip ? ` · ${res.ip}` : ""}`,
        );
        onJoined();
        onClose();
      } else {
        setError(res.error || "Join failed.");
      }
    } catch (err) {
      if (err instanceof ApiError && err.status === 409) {
        const body = (err.body ?? null) as
          | { detail?: { needs_force?: boolean; error?: { message?: string } } }
          | null;
        if (body?.detail?.needs_force) {
          setError(
            body.detail.error?.message ||
              "AP is active. Retry to steal the radio.",
          );
          setBusy(false);
          return;
        }
      }
      toastFromError(err, "Join failed.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={true} onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {target.saved ? "Reconnect" : "Join"}{" "}
            <span className="font-mono">{target.ssid}</span>
          </DialogTitle>
          <DialogDescription asChild>
            <div className="text-sm text-muted-foreground">
              {secured
                ? target.saved
                  ? "Saved credentials will be reused. Type a new password to overwrite."
                  : "Enter the network password."
                : "This network is open. No password required."}
            </div>
          </DialogDescription>
        </DialogHeader>

        {secured && (
          <div className="space-y-2">
            <Label htmlFor="wifi-join-password">Password</Label>
            <Input
              id="wifi-join-password"
              type="password"
              autoComplete="new-password"
              value={password}
              placeholder={
                target.saved ? "(leave blank to reuse saved)" : ""
              }
              maxLength={63}
              autoFocus
              onChange={(e) => {
                setPassword(e.target.value);
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") void attemptJoin();
              }}
            />
            <p className="text-[11px] text-muted-foreground">
              Write-only. The agent never echoes Wi-Fi passwords back.
            </p>
          </div>
        )}

        {error && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {error}
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" disabled={busy} onClick={onClose}>
            Cancel
          </Button>
          <Button disabled={busy} onClick={() => void attemptJoin()}>
            {busy ? "Joining…" : target.saved ? "Reconnect" : "Join"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
