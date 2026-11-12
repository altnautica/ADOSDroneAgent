// Node — the home of a workstation or compute node: the World Engine compute
// service's status (role, queue, workers, cluster), its jobs with their
// products, and the live perception-offload sessions it serves to drones. All
// of it is read from the extension's job API through the agent
// (`/api/plugins/com.altnautica.world-engine/x/api/compute/*`); a node that
// does not run the service shows that plainly instead of an error.

import { useCallback, useState } from "react";
import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { EmptyNote, ErrorNote, StaleBadge, Tile, TileGrid } from "@/components/ui/data";
import { readFailed } from "@/lib/read-failed";
import { jobStateKey, sessionStateKey } from "@/lib/screen-words";
import { GuardedButton } from "@/components/ui/guarded-button";
import { ListItem } from "@/components/ui/list-item";
import { TwoPane } from "@/components/ui/two-pane";
import {
  cancelNodeJob,
  getNodeJobs,
  getNodeOutputs,
  getNodeStatus,
  getOffloadSessions,
  isJobActive,
  type NodeJob,
  type OffloadSession,
} from "@/lib/node-api";
import { toast, toastFromError } from "@/shared/api/toast";
import { DASH, fmtInt } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const STATUS_POLL_MS = 2000;
const JOBS_POLL_MS = 3000;

export function NodeScreen() {
  const { t } = useTranslation();
  const [tab, setTab] = useState(0);
  const [jobId, setJobId] = useState<string | null>(null);

  const status = useResource(useCallback((s: AbortSignal) => getNodeStatus(s), []), STATUS_POLL_MS);
  const jobs = useResource(useCallback((s: AbortSignal) => getNodeJobs(s), []), JOBS_POLL_MS);
  const sessions = useResource(useCallback((s: AbortSignal) => getOffloadSessions(s), []), STATUS_POLL_MS, {
    enabled: tab === 1,
  });

  // The service answers 404 or 503 when it is absent or stopped on this node.
  const absent = status.data == null && (status.status === 404 || status.status === 503);
  const st = status.data;
  const selected = jobs.data?.find((j) => j.id === jobId) ?? null;

  let body;
  if (absent) {
    body = <EmptyNote>{t("cockpit.node.unavailable")}</EmptyNote>;
  } else if (!status.ready && !jobs.ready) {
    body = <EmptyNote>{t("cockpit.node.loading")}</EmptyNote>;
  } else {
    body = (
      <div className="flex h-full min-h-0 flex-col gap-[0.6rem]">
        <TileGrid>
          <Tile label={t("cockpit.node.role")} value={st?.role ?? DASH} />
          <Tile label={t("cockpit.node.queue")} value={fmtInt(st?.queueDepth)} />
          <Tile label={t("cockpit.node.activeJobs")} value={fmtInt(st?.activeJobs)} />
          <Tile label={t("cockpit.node.workersIdle")} value={fmtInt(st?.workersIdle)} />
          <Tile label={t("cockpit.node.cluster")} value={st ? fmtInt(st.clusterSlaves + 1) : DASH} />
        </TileGrid>
        <div className="min-h-0 flex-1">
          {tab === 0 ? (
            <TwoPane
              showDetailInPortrait={selected !== null}
              list={
                readFailed(jobs) ? (
                  <ErrorNote onRetry={jobs.refresh}>{t("cockpit.readFailed.node")}</ErrorNote>
                ) : (
                  <JobList jobs={jobs.data ?? []} selectedId={jobId} onSelect={setJobId} />
                )
              }
              detail={selected ? <JobDetail job={selected} onCancelled={jobs.refresh} /> : <EmptyNote>{t("cockpit.node.pickJob")}</EmptyNote>}
            />
          ) : (
            readFailed(sessions) ? (
              <ErrorNote onRetry={sessions.refresh}>{t("cockpit.readFailed.node")}</ErrorNote>
            ) : (
              <SessionList sessions={sessions.data ?? []} />
            )
          )}
        </div>
      </div>
    );
  }

  return (
    <ConsoleScaffold stale={status.stale || jobs.stale}
      title={t("cockpit.node.title")}
      onBack={selected ? () => setJobId(null) : undefined}
      backLabel={t("cockpit.node.back")}
      tabs={[t("cockpit.node.tabJobs"), t("cockpit.node.tabOffload")]}
      selectedTab={tab}
      onSelectTab={setTab}
      actions={<StaleBadge stale={status.stale || jobs.stale} />}
    >
      {body}
    </ConsoleScaffold>
  );
}

function JobList({
  jobs,
  selectedId,
  onSelect,
}: {
  jobs: NodeJob[];
  selectedId: string | null;
  onSelect: (id: string) => void;
}) {
  const { t } = useTranslation();
  if (jobs.length === 0) return <EmptyNote>{t("cockpit.node.noJobs")}</EmptyNote>;
  return (
    <div className="flex flex-col gap-[0.2rem] pr-[0.4rem]">
      {jobs.map((j) => {
        const stateKey = jobStateKey(j.state);
        const state = stateKey ? t(stateKey) : DASH;
        return (
        <ListItem
          key={j.id}
          headline={j.kind || j.id}
          supporting={
            j.state === "running" && j.progress != null
              ? `${state} · ${t("cockpit.node.progress", { pct: Math.round(j.progress * 100) })}`
              : (j.error ?? state)
          }
          selected={j.id === selectedId}
          onClick={() => onSelect(j.id)}
        />
        );
      })}
    </div>
  );
}

function JobDetail({ job, onCancelled }: { job: NodeJob; onCancelled: () => void }) {
  const { t } = useTranslation();
  const outputs = useResource(
    useCallback((s: AbortSignal) => getNodeOutputs(job.id, s), [job.id]),
    JOBS_POLL_MS,
  );
  return (
    <div className="flex flex-col gap-[0.5rem] pl-[0.6rem]">
      <div className="font-mono text-[0.8rem] text-muted-foreground">{job.id}</div>
      {job.error ? <div className="text-[0.85rem] text-err">{job.error}</div> : null}
      <h2 className="text-[0.75rem] font-medium uppercase tracking-wider text-muted-foreground">
        {t("cockpit.node.products")}
      </h2>
      {(outputs.data ?? []).length === 0 ? (
        <div className="text-[0.85rem] text-muted-foreground">{t("cockpit.node.noProducts")}</div>
      ) : (
        (outputs.data ?? []).map((o) => (
          <ListItem key={o.id} headline={o.file ?? o.kind} supporting={[o.kind, o.format].filter(Boolean).join(" · ")} />
        ))
      )}
      {isJobActive(job.state) ? (
        <GuardedButton
          label={t("cockpit.node.cancel")}
          guardLabel={t("cockpit.node.cancelGuard")}
          onFired={() => {
            cancelNodeJob(job.id)
              .then(() => {
                toast.ok(t("cockpit.node.cancelled"));
                onCancelled();
              })
              .catch((err: unknown) => toastFromError(err, t("cockpit.node.cancelFailed")));
          }}
        />
      ) : null}
    </div>
  );
}

function SessionList({ sessions }: { sessions: OffloadSession[] }) {
  const { t } = useTranslation();
  if (sessions.length === 0) return <EmptyNote>{t("cockpit.node.noSessions")}</EmptyNote>;
  return (
    <div className="flex flex-col gap-[0.2rem]">
      {sessions.map((s) => {
        const stateKey = sessionStateKey(s.state);
        return (
        <ListItem
          key={s.id}
          headline={`${s.id} · ${stateKey ? t(stateKey) : DASH}`}
          supporting={t("cockpit.node.sessionDetail", {
            camera: s.cameraId ?? DASH,
            frames: fmtInt(s.framesProcessed),
            reconnects: fmtInt(s.reconnects),
          })}
        />
        );
      })}
    </div>
  );
}
