// @vitest-environment happy-dom
// The flight-state chip's mission ETA comes from the agent's `ar` frame, which
// the agent computes only for a camera this client leases. The layer stack
// drops its view lease when the video is lost; the chip keeps its own
// `ar_route` lease while a mission runs, so the ETA stays.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { LayerStack } from "@/components/feed/layers/layer-stack";
import { useFlightChip } from "@/hooks/use-pilot";
import { layerFrames } from "@/lib/layer-frames";
import { connectNavStream } from "@/lib/nav-stream";
import type { VideoFeedState } from "@/shared/video-transport";

vi.mock("@/shared/ws-ticket", () => ({ WS_TICKET_PROTOCOL: "ticket", mintWsTicket: async () => null }));

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

class FakeSocket {
  static OPEN = 1;
  static last: FakeSocket | null = null;
  readyState = 0;
  sent: string[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((m: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: unknown = null;
  constructor() {
    FakeSocket.last = this;
  }
  open(): void {
    this.readyState = 1;
    this.onopen?.();
  }
  send(text: string): void {
    this.sent.push(text);
  }
  close(): void {
    this.readyState = 3;
    this.onclose?.();
  }
}

const PILOT = { owner: "mission_fc", armed: true, landed: false, control_mode: "ados", mission: { state: "active", seq: 4, total: 6 } };
const AR_CAMERA = { frame_w: 1280, frame_h: 720, items: [{ t: "wp", seq: 4, role: "next", x: 640, y: 360, eta_s: 12 }] };

/** The view lease the agent holds for this socket: the last one sent. */
function lease(sock: FakeSocket): { camera_id: string; layers: string[] } {
  return JSON.parse(sock.sent.at(-1) ?? "{}");
}

/** One agent tick: a fresh pilot frame, and the `ar` frame projected onto this
 *  client's lease the way the nav hub does (sent only while `ar_route` or
 *  `ar_motion` is leased, its cameras narrowed to the leased one). */
function agentTick(sock: FakeSocket): void {
  sock.onmessage?.({ data: JSON.stringify({ kind: "pilot", ts_ms: Date.now(), data: PILOT }) });
  const l = lease(sock);
  if (!l.layers?.some((x) => x === "ar_route" || x === "ar_motion")) return;
  const cameras = l.camera_id === "main" ? { main: AR_CAMERA } : {};
  sock.onmessage?.({ data: JSON.stringify({ kind: "ar", ts_ms: Date.now(), data: { ts_ms: Date.now(), cameras } }) });
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date", "performance"] });
  vi.stubGlobal("WebSocket", FakeSocket);
  vi.stubGlobal("fetch", async () => new Response("{}", { status: 200, headers: { "Content-Type": "application/json" } }));
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  layerFrames.clear();
});

describe("the chip's mission ETA", () => {
  it("survives a video loss", async () => {
    let chip: string | null = null;
    const Screen = ({ videoState }: { videoState: VideoFeedState }) => {
      chip = useFlightChip()?.text ?? null;
      return createElement(LayerStack, { cameraId: null, videoWidth: 1280, videoHeight: 720, videoState, fpsCap: 0 });
    };
    const disconnect = connectNavStream();
    await act(async () => vi.advanceTimersByTimeAsync(0));
    const sock = FakeSocket.last!;
    act(() => sock.open());
    const root = createRoot(document.createElement("div"));
    await act(async () => root.render(createElement(Screen, { videoState: "live" })));
    for (let i = 0; i < 4; i += 1) {
      await act(async () => {
        agentTick(sock);
        await vi.advanceTimersByTimeAsync(500);
      });
    }
    expect(chip).toContain("ETA");
    const withVideo = chip;

    // The video is lost: the layer stack lets its lease go; the chip keeps
    // `ar_route` on the camera the layers leased.
    await act(async () => root.render(createElement(Screen, { videoState: "failed" })));
    // Well past the 2 s an `ar` frame stays fresh for the ETA.
    for (let i = 0; i < 12; i += 1) {
      await act(async () => {
        agentTick(sock);
        await vi.advanceTimersByTimeAsync(500);
      });
    }
    expect(chip).toBe(withVideo);
    expect(lease(sock)).toEqual({ type: "view", camera_id: "main", layers: ["ar_route"] });

    await act(async () => root.unmount());
    disconnect();
  });
});
