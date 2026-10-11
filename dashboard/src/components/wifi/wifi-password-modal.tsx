import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

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
import { ApiError } from "@/shared/api-fetch";
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
  const { t } = useTranslation();
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
      setError(t("wifi.join.passwordRequired"));
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
          res.ip
            ? t("wifi.join.joinedWithIp", { ssid: target.ssid, ip: res.ip })
            : t("wifi.join.joined", { ssid: target.ssid }),
        );
        onJoined();
        onClose();
      } else {
        setError(res.error || t("wifi.join.failed"));
      }
    } catch (err) {
      if (err instanceof ApiError && err.status === 409) {
        const body = (err.body ?? null) as
          | { detail?: { needs_force?: boolean; error?: { message?: string } } }
          | null;
        if (body?.detail?.needs_force) {
          setError(
            body.detail.error?.message ||
              t("wifi.join.apActive"),
          );
          setBusy(false);
          return;
        }
      }
      toastFromError(err, t("wifi.join.failed"));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={true} onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {target.saved ? t("wifi.common.reconnect") : t("wifi.common.join")}{" "}
            <span className="font-mono">{target.ssid}</span>
          </DialogTitle>
          <DialogDescription asChild>
            <div className="text-sm text-muted-foreground">
              {secured
                ? target.saved
                  ? t("wifi.join.savedHint")
                  : t("wifi.join.enterPassword")
                : t("wifi.join.openNetwork")}
            </div>
          </DialogDescription>
        </DialogHeader>

        {secured && (
          <div className="space-y-2">
            <Label htmlFor="wifi-join-password">{t("wifi.join.password")}</Label>
            <Input
              id="wifi-join-password"
              type="password"
              autoComplete="new-password"
              value={password}
              placeholder={
                target.saved ? t("wifi.join.reusePlaceholder") : ""
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
              {t("wifi.join.writeOnly")}
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
            {t("wifi.common.cancel")}
          </Button>
          <Button disabled={busy} onClick={() => void attemptJoin()}>
            {busy
              ? t("wifi.join.joining")
              : target.saved
                ? t("wifi.common.reconnect")
                : t("wifi.common.join")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
