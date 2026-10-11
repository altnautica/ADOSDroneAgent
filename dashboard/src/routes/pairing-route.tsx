import { useState } from "react";
import { Link2, Unlink, Plus, Smartphone } from "lucide-react";
import { useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  useAcceptCode,
  useDecidePhoneInvite,
  usePairingInfo,
  usePhoneInvites,
  useUnpair,
} from "@/hooks/use-pairing";
import { useNodeIdentity } from "@/hooks/use-node-identity";

function MaskedCode({ code }: { code: string }) {
  return (
    <div className="font-mono text-3xl tracking-[0.4em] py-2 select-all">{code}</div>
  );
}

/** Phones with their own radio asking this ground station for the fleet
 *  receive keys. Nothing is shared until the operator approves one. */
function PhoneReceiversCard() {
  const { t } = useTranslation();
  const invites = usePhoneInvites();
  const decide = useDecidePhoneInvite();
  const pending = invites.data?.pending ?? [];
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Smartphone className="h-3.5 w-3.5" />
          {t("pairing.phones.title")}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">
          {t("pairing.phones.description")}
        </p>
        {invites.isError && !invites.data && (
          <p className="text-xs text-destructive">{t("pairing.phones.readError")}</p>
        )}
        {invites.data && pending.length === 0 && (
          <p className="text-sm text-muted-foreground">{t("pairing.phones.empty")}</p>
        )}
        <ul className="space-y-2">
          {pending.map((p) => (
            <li
              key={p.invite_id}
              className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-border px-3 py-2"
            >
              <div className="min-w-0">
                <div className="truncate text-sm">{p.label}</div>
                <div className="font-mono text-sm tracking-wider select-all">
                  {p.phone_fingerprint}
                </div>
              </div>
              <div className="flex gap-2">
                <Button
                  variant="outline"
                  size="sm"
                  disabled={decide.isPending}
                  onClick={() => decide.mutate({ id: p.invite_id, decision: "reject" })}
                  aria-label={t("pairing.phones.rejectLabel", { label: p.label })}
                >
                  {t("pairing.phones.reject")}
                </Button>
                <Button
                  variant="default"
                  size="sm"
                  disabled={decide.isPending}
                  onClick={() => decide.mutate({ id: p.invite_id, decision: "approve" })}
                  aria-label={t("pairing.phones.approveLabel", { label: p.label })}
                >
                  {t("pairing.phones.approve")}
                </Button>
              </div>
            </li>
          ))}
        </ul>
        {decide.isError && (
          <p className="text-xs text-destructive">
            {t("pairing.phones.decideError")}
          </p>
        )}
      </CardContent>
    </Card>
  );
}

function AcceptCodeForm() {
  const { t } = useTranslation();
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
      <Label htmlFor="paste-code">{t("pairing.accept.label")}</Label>
      <div className="flex items-center gap-2">
        <Input
          id="paste-code"
          placeholder={t("pairing.accept.placeholder")}
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
          {t("pairing.accept.pair")}
        </Button>
      </div>
      {accept.isError && (
        <p className="text-xs text-destructive">
          {accept.error instanceof Error ? accept.error.message : t("pairing.accept.failed")}
        </p>
      )}
      {accept.isSuccess && (
        <p className="text-xs text-ok">{t("pairing.accept.success")}</p>
      )}
    </form>
  );
}

export function PairingRoute() {
  const { t } = useTranslation();
  const info = usePairingInfo();
  const unpair = useUnpair();
  const { profile } = useNodeIdentity();
  const [confirmUnpair, setConfirmUnpair] = useState(false);
  const subject =
    profile === "ground_station" ? t("pairing.subject.groundStation") : t("pairing.subject.drone");
  const paired = info.data?.paired === true;

  return (
    <div className="space-y-6 max-w-3xl">
      <header>
        <h1 className="text-xl font-semibold tracking-tight">{t("pairing.page.title")}</h1>
        <p className="text-sm text-muted-foreground">
          {t("pairing.page.blurb", { subject })}
        </p>
      </header>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center justify-between">
            <span className="flex items-center gap-2">
              <Link2 className="h-3.5 w-3.5" />
              {t("pairing.page.title")}
            </span>
            {paired && (
              <Button
                variant="outline"
                size="sm"
                onClick={() => setConfirmUnpair(true)}
                disabled={unpair.isPending}
              >
                <Unlink className="h-3.5 w-3.5" />
                {t("pairing.page.unpair")}
              </Button>
            )}
          </CardTitle>
        </CardHeader>
        <CardContent>
          {info.isLoading && !info.data && (
            <p className="text-xs text-muted-foreground">{t("pairing.page.loading")}</p>
          )}
          {info.isError && !info.data && (
            <p className="text-xs text-destructive">{t("pairing.page.readError")}</p>
          )}
          {info.data &&
            (paired ? (
              <div className="space-y-2">
                <Badge variant="ok">{t("pairing.page.paired")}</Badge>
                <div className="grid grid-cols-[110px_1fr] gap-y-1 text-sm">
                  <span className="text-xs text-muted-foreground">{t("pairing.page.owner")}</span>
                  <span className="font-mono truncate">{info.data.owner_id ?? "—"}</span>
                  <span className="text-xs text-muted-foreground">{t("pairing.page.paired")}</span>
                  <span className="font-mono">
                    {info.data.paired_at != null
                      ? new Date(info.data.paired_at * 1000).toLocaleString()
                      : "—"}
                  </span>
                </div>
              </div>
            ) : (
              <div className="space-y-2">
                <Badge variant="info">{t("pairing.page.awaiting")}</Badge>
                {info.data.pairing_code ? (
                  <MaskedCode code={info.data.pairing_code} />
                ) : (
                  <p className="text-sm text-muted-foreground">
                    {t("pairing.page.codeHidden")}
                  </p>
                )}
              </div>
            ))}
          {unpair.isError && (
            <p className="pt-2 text-xs text-destructive">
              {unpair.error instanceof Error ? unpair.error.message : t("pairing.page.unpairFailed")}
            </p>
          )}
        </CardContent>
      </Card>

      {!paired && (
        <Card>
          <CardHeader>
            <CardTitle>{t("pairing.accept.cardTitle")}</CardTitle>
          </CardHeader>
          <CardContent>
            <AcceptCodeForm />
          </CardContent>
        </Card>
      )}

      {profile === "ground_station" && <PhoneReceiversCard />}

      <ConfirmDialog
        open={confirmUnpair}
        onOpenChange={setConfirmUnpair}
        title={t("pairing.confirm.title", { subject })}
        description={t("pairing.confirm.description")}
        confirmLabel={t("pairing.page.unpair")}
        destructive
        onConfirm={async () => {
          await unpair.mutateAsync().catch(() => undefined);
        }}
      />
    </div>
  );
}
