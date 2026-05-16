import { useState } from "react";
import { Link2, Unlink, Plus } from "lucide-react";

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
  const profile = status.data?.profile;
  const subject = profile === "ground_station" ? "ground station" : "drone";

  return (
    <div className="space-y-6 max-w-3xl">
      <header>
        <h1 className="text-xl font-semibold tracking-tight">Pairing</h1>
        <p className="text-sm text-muted-foreground">
          Link this {subject} to one or more Mission Control instances. Codes
          rotate automatically; new codes are generated on agent restart and
          when a pairing succeeds.
        </p>
      </header>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Link2 className="h-3.5 w-3.5" />
            Current code
          </CardTitle>
        </CardHeader>
        <CardContent>
          {info.isLoading && !info.data && (
            <p className="text-xs text-muted-foreground">loading…</p>
          )}
          {info.data && (
            <div className="space-y-2">
              <div className="flex items-center gap-2">
                {info.data.paired ? (
                  <Badge variant="ok">paired</Badge>
                ) : (
                  <Badge variant="info">awaiting pair</Badge>
                )}
                {info.data.beacon_state && (
                  <Badge variant="outline">beacon: {info.data.beacon_state}</Badge>
                )}
              </div>
              {info.data.pairing_code ? (
                <MaskedCode code={info.data.pairing_code} />
              ) : (
                <p className="text-sm text-muted-foreground">
                  no code published — restart the agent or unpair to refresh
                </p>
              )}
            </div>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Accept from Mission Control</CardTitle>
        </CardHeader>
        <CardContent>
          <AcceptCodeForm />
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center justify-between">
            <span>Paired devices</span>
            {info.data?.paired_with && info.data.paired_with.length > 0 && (
              <Button
                variant="outline"
                size="sm"
                onClick={() => unpair.mutate()}
                disabled={unpair.isPending}
              >
                <Unlink className="h-3.5 w-3.5" />
                Unpair all
              </Button>
            )}
          </CardTitle>
        </CardHeader>
        <CardContent>
          {!info.data?.paired_with || info.data.paired_with.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              No devices paired yet. Codes are visible above.
            </p>
          ) : (
            <ul className="divide-y divide-border/50">
              {info.data.paired_with.map((d) => (
                <li
                  key={d.client_id}
                  className="py-2.5 flex items-center justify-between gap-3 first:pt-0 last:pb-0"
                >
                  <div className="min-w-0 flex-1">
                    <div className="text-sm font-medium truncate">
                      {d.display_name ?? d.client_id}
                    </div>
                    <div className="text-xs text-muted-foreground font-mono truncate">
                      {d.client_id}
                    </div>
                  </div>
                  <div className="text-xs text-muted-foreground whitespace-nowrap">
                    paired {new Date(d.paired_at).toLocaleString()}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
