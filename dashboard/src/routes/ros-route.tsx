import { Bot } from "lucide-react";
import { useState } from "react";

import { PageShell } from "@/components/page-shell";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { apiFetch } from "@/lib/api";
import { toast, toastFromError } from "@/lib/toast";

interface RosStatus {
  state?: string;
  installed?: boolean;
  running?: boolean;
  distro?: string;
  workspace?: string;
  foxglove_url?: string;
  rmw?: string;
  active_launch?: string | null;
  error?: string;
}

interface NodeList {
  nodes: string[];
}

interface TopicList {
  topics: { name: string; type?: string }[];
}

export function RosRoute() {
  const status = useResource<RosStatus>("ros-status", "/api/ros/status", 6000);
  const isRunning = !!status.data?.running;

  const nodes = useResource<NodeList>(
    "ros-nodes",
    "/api/ros/nodes",
    isRunning ? 6000 : false,
  );
  const topics = useResource<TopicList>(
    "ros-topics",
    "/api/ros/topics",
    isRunning ? 6000 : false,
  );

  const [confirm, setConfirm] = useState<null | "init" | "stop">(null);
  const [busy, setBusy] = useState(false);

  async function dispatch(kind: "init" | "stop") {
    setBusy(true);
    try {
      await apiFetch(`/api/ros/${kind}`, { method: "POST" });
      toast.ok(
        kind === "init"
          ? "ROS environment starting."
          : "ROS environment stopping.",
        kind === "init"
          ? "The container takes 5–15 seconds to come up."
          : undefined,
      );
      status.refetch();
    } catch (err) {
      toastFromError(err, "ROS dispatch failed.");
    } finally {
      setBusy(false);
    }
  }

  if (!status.data?.installed && !status.isLoading) {
    return (
      <PageShell
        title="ROS"
        blurb="ROS 2 environment management. Currently disabled — install the ROS overlay first."
      >
        <Card>
          <CardContent className="pt-5 pb-5 flex items-start gap-3">
            <Bot className="h-5 w-5 text-muted-foreground mt-0.5" />
            <div>
              <div className="text-sm font-medium">
                ROS environment not installed.
              </div>
              <div className="text-xs text-muted-foreground mt-1">
                The ROS 2 Jazzy overlay is opt-in. Run{" "}
                <span className="font-mono">ados ros install</span> on the
                agent (or use the install script's{" "}
                <span className="font-mono">--with-ros</span> flag) to pull
                the Docker container and bridge service.
              </div>
            </div>
          </CardContent>
        </Card>
      </PageShell>
    );
  }

  return (
    <PageShell
      title="ROS"
      blurb="ROS 2 environment, MAVLink bridge, and live node + topic counts."
      rightAction={
        <Button
          variant={isRunning ? "outline" : "default"}
          size="sm"
          disabled={busy}
          onClick={() => setConfirm(isRunning ? "stop" : "init")}
        >
          {isRunning ? "Stop" : "Start"}
        </Button>
      }
    >
      <Card>
        <CardContent className="pt-5 pb-5 grid grid-cols-2 gap-x-6 gap-y-2 text-sm">
          <div className="text-xs text-muted-foreground">state</div>
          <div className="font-mono">
            {status.data?.state ?? (isRunning ? "running" : "stopped")}
            {isRunning && (
              <span className="ml-2 text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border border-ok/40 text-ok">
                live
              </span>
            )}
          </div>

          <div className="text-xs text-muted-foreground">distro</div>
          <div className="font-mono">{status.data?.distro ?? "—"}</div>

          <div className="text-xs text-muted-foreground">rmw</div>
          <div className="font-mono">{status.data?.rmw ?? "—"}</div>

          <div className="text-xs text-muted-foreground">workspace</div>
          <div className="font-mono text-xs truncate">
            {status.data?.workspace ?? "—"}
          </div>

          {status.data?.active_launch && (
            <>
              <div className="text-xs text-muted-foreground">active launch</div>
              <div className="font-mono text-xs">{status.data.active_launch}</div>
            </>
          )}

          {status.data?.foxglove_url && (
            <>
              <div className="text-xs text-muted-foreground">foxglove</div>
              <div className="font-mono text-xs">
                <a
                  href={status.data.foxglove_url}
                  target="_blank"
                  rel="noreferrer"
                  className="text-primary hover:underline"
                >
                  {status.data.foxglove_url}
                </a>
              </div>
            </>
          )}

          {status.data?.error && (
            <>
              <div className="text-xs text-muted-foreground">error</div>
              <div className="font-mono text-xs text-destructive">
                {status.data.error}
              </div>
            </>
          )}
        </CardContent>
      </Card>

      <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
        <Card>
          <CardContent className="pt-4 pb-4 space-y-2">
            <div className="text-sm font-semibold">Nodes</div>
            {!isRunning ? (
              <p className="text-xs text-muted-foreground">
                start the environment to discover nodes.
              </p>
            ) : (nodes.data?.nodes?.length ?? 0) === 0 ? (
              <p className="text-xs text-muted-foreground">
                no nodes registered yet. running launch files publish here.
              </p>
            ) : (
              <ul className="font-mono text-xs space-y-1 max-h-64 overflow-y-auto">
                {nodes.data!.nodes.map((n, i) => (
                  <li key={i} className="truncate" title={n}>
                    {n}
                  </li>
                ))}
              </ul>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardContent className="pt-4 pb-4 space-y-2">
            <div className="text-sm font-semibold">Topics</div>
            {!isRunning ? (
              <p className="text-xs text-muted-foreground">
                start the environment to discover topics.
              </p>
            ) : (topics.data?.topics?.length ?? 0) === 0 ? (
              <p className="text-xs text-muted-foreground">no topics published yet.</p>
            ) : (
              <ul className="font-mono text-xs space-y-1 max-h-64 overflow-y-auto">
                {topics.data!.topics.map((t, i) => (
                  <li key={i} className="flex justify-between gap-3">
                    <span className="truncate" title={t.name}>
                      {t.name}
                    </span>
                    <span className="text-muted-foreground shrink-0 truncate max-w-[40%]">
                      {t.type ?? ""}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </CardContent>
        </Card>
      </div>

      <ConfirmDialog
        open={confirm === "init"}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        title="Start the ROS environment?"
        description="Starts the ROS 2 Docker container, the MAVLink bridge, and the foxglove WebSocket. Takes 5–15 seconds."
        confirmLabel="Start"
        onConfirm={() => dispatch("init")}
      />
      <ConfirmDialog
        open={confirm === "stop"}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        title="Stop the ROS environment?"
        description="Stops the container and the bridge. Any running launch files exit. Recordings are flushed."
        confirmLabel="Stop"
        destructive
        onConfirm={() => dispatch("stop")}
      />
    </PageShell>
  );
}
