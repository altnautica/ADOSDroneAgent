// @vitest-environment happy-dom
// The Feed's main video lives once, for the Feed's lifetime: swapping the
// pilot and map views, or turning the screen between landscape and portrait,
// moves the same <video> (and the transport playing into it) between the
// grid's slots instead of unmounting it and dialling again.

import { createElement, Fragment } from "react";
import { createPortal } from "react-dom";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { LandscapeGrid, PortraitGrid, type FeedSlots } from "@/components/feed/feed-layouts";
import { VideoSlot } from "@/components/feed/video-slot";
import { useVideoHost } from "@/hooks/use-video-host";
import { gridMeasures } from "@/lib/cockpit-layout";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

type Layout = "pilot" | "map" | "portrait";

let mounts = 0;
let enabledSeen: boolean[] = [];

/** A stable ref callback runs once per element instance: a mount counter. */
function countMount(el: HTMLVideoElement | null) {
  if (el) mounts += 1;
}

/** Stands in for the video layer: counts its mounts, records whether the
 *  transport would run, and renders the element the transport plays into. */
function Player({ enabled }: { enabled: boolean }) {
  enabledSeen.push(enabled);
  return createElement("video", { "data-testid": "player", ref: countMount });
}

const m = gridMeasures("medium", false);
const pane = (className: string) => createElement("div", { className, "data-testid": "map" });

function Harness({ layout }: { layout: Layout }) {
  const video = useVideoHost();
  const slots: FeedSlots = {
    video: createElement(VideoSlot, { attach: video.attach }),
    layers: null,
    hud: null,
    topBar: null,
    alerts: null,
    overlays: null,
    map: pane,
    cards: { map: pane },
    pip: null,
    leftColumn: () => null,
    rightColumn: () => null,
    bottomRow: null,
    followStrip: null,
    skillPicker: null,
    ack: null,
    visionPanel: null,
    onscreenSticks: null,
  };
  const grid =
    layout === "portrait"
      ? createElement(PortraitGrid, { slots })
      : createElement(LandscapeGrid, {
          slots,
          m,
          input: { width: 1280, height: 720, m, widthClass: "medium", view: layout, flies: true, strip: false, gimbal: false },
        });
  return createElement(Fragment, null, createPortal(createElement(Player, { enabled: video.attached }), video.host), grid);
}

let host: HTMLDivElement;
let root: Root;

async function show(layout: Layout) {
  await act(async () => root.render(createElement(Harness, { layout })));
}
const player = () => host.querySelector<HTMLVideoElement>('[data-testid="player"]');

beforeEach(() => {
  mounts = 0;
  enabledSeen = [];
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
});

describe("the Feed's persistent main video", () => {
  it("keeps one video element through a pilot-map swap and a rotation", async () => {
    await show("pilot");
    const first = player();
    expect(first).not.toBeNull();
    expect(first!.closest('[data-zone="stage"]')).not.toBeNull();

    await show("map");
    expect(player()).toBe(first);
    expect(first!.closest('[data-testid="video-card"]')).not.toBeNull();

    await show("portrait");
    expect(player()).toBe(first);

    await show("pilot");
    expect(player()).toBe(first);
    expect(first!.closest('[data-zone="stage"]')).not.toBeNull();
    expect(mounts).toBe(1);
  });

  it("keeps its transport running across the swaps", async () => {
    await show("pilot");
    const firstOn = enabledSeen.indexOf(true);
    expect(firstOn).toBeGreaterThanOrEqual(0);
    await show("map");
    await show("portrait");
    await show("pilot");
    expect(enabledSeen.slice(firstOn)).not.toContain(false);
  });
});
