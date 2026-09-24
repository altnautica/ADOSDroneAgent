import { useState } from "react";
import { Link2, Unlink, Plus } from "lucide-react";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  useAcceptCode,
  usePairingInfo,
  useUnpair,
} from "@/hooks/use-pairing";
import { useStatus } from "@/hooks/use-status";

function MaskedCode({ code }: { code: string }) {
  return (
    <div className="font-mono text-3xl tracking-[0.4em] py-2 select-all">{code}</div>
  );
}

function AcceptCodeForm() {
  const [value, setValue] = useState("");
  const accept = useAcceptCode();

  const onSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!value.trim()) return;
    try {
      await accept.mutateAsync(value.trim());
      setValue("");
    } catch {
      // Error surfaced via accept.isError
    }
  };

  return (
    <form onSubmit={onSubmit} className="space-y-2">
      <Label htmlFor="paste-code">Accept code from Mission Control</Label>
      <div className="flex items-center gap-2">
        <Input
          id="paste-code"
          placeholder="paste 6-digit code"
          maxLength={12}
          value={value}
          onChange={(e) => setValue(e.target.value.toUpperCase())}
        />
        <Button
          type="submit"
          variant="default"
          size="default"
          disabled={!value.trim() || accept.isPending}
        >
          <Plus className="h-3.5 w-3.5" />
          Pair
        </Button>
      </div>
      {accept.isError && (
        <p className="text-xs text-destructive">
          {accept.error instanceof Error ? accept.error.message : "pair failed"}
        </p>
      )}
      {accept.isSuccess && (
        <p className="text-xs text-ok">paired successfully.</p>
      )}
    </form>
  );
}

export function PairingRoute() {
  const info = usePairingInfo();
  const unpair = useUnpair();
  const status = useStatus();
  const [confirmUnpair, setConfirmUnpair] = useState(false);
  const profile = status.data?.profile;
  const subject = profile === "ground_station" ? "ground station" : "drone";
  const paired = info.data?.paired === true;

  return (
    <div className="space-y-6 max-w-3xl">
      <header>
        <h1 className="text-xl font-semibold tracking-tight">Pairing</h1>
        <p className="text-sm text-muted-foreground">
          Link this {subject} to its Mission Control owner. An unpaired node
          publishes a code here; pairing binds it to one owner until it is
          unpaired.
        </p>
      </header>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center justify-between">
            <span className="flex items-center gap-2">
              <Link2 className="h-3.5 w-3.5" />
              Pairing
            </span>
            {paired && (
              <Button
                variant="outline"
                size="sm"
                onClick={() => setConfirmUnpair(true)}
                disabled={unpair.isPending}
              >
                <Unlink className="h-3.5 w-3.5" />
                Unpair
              </Button>
            )}
          </CardTitle>
        </CardHeader>
        <CardContent>
          {info.isLoading && !info.data && (
            <p className="text-xs text-muted-foreground">loading…</p>
          )}
          {info.isError && !info.data && (
            <p className="text-xs text-destructive">Could not read the pairing state.</p>
          )}
          {info.data &&
            (paired ? (
              <div className="space-y-2">
                <Badge variant="ok">paired</Badge>
                <div className="grid grid-cols-[110px_1fr] gap-y-1 text-sm">
                  <span className="text-xs text-muted-foreground">owner</span>
                  <span className="font-mono truncate">{info.data.owner_id ?? "—"}</span>
                  <span className="text-xs text-muted-foreground">paired</span>
                  <span className="font-mono">
                    {info.data.paired_at != null
                      ? new Date(info.data.paired_at * 1000).toLocaleString()
                      : "—"}
                  </span>
                </div>
              </div>
            ) : (
              <div className="space-y-2">
                <Badge variant="info">awaiting pair</Badge>
                {info.data.pairing_code ? (
                  <MaskedCode code={info.data.pairing_code} />
                ) : (
                  <p className="text-sm text-muted-foreground">
                    The code is shown only on this node's own networks. Open the
                    dashboard from the node's LAN, hotspot or USB link to read it.
                  </p>
                )}
              </div>
            ))}
          {unpair.isError && (
            <p className="pt-2 text-xs text-destructive">
              {unpair.error instanceof Error ? unpair.error.message : "unpair failed"}
            </p>
          )}
        </CardContent>
      </Card>

      {!paired && (
        <Card>
          <CardHeader>
            <CardTitle>Accept from Mission Control</CardTitle>
          </CardHeader>
          <CardContent>
            <AcceptCodeForm />
          </CardContent>
        </Card>
      )}

      <ConfirmDialog
        open={confirmUnpair}
        onOpenChange={setConfirmUnpair}
        title={`Unpair this ${subject}?`}
        description="Mission Control loses access until the node is paired again, and this dashboard asks for its PIN again."
        confirmLabel="Unpair"
        destructive
        onConfirm={async () => {
          await unpair.mutateAsync().catch(() => undefined);
        }}
      />
    </div>
  );
}
