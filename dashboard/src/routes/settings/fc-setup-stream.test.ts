import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { useFcSetup } from "@/shared/setup-wizard/use-fc-setup";

// The first render, before any effect runs: the stream has not opened yet.
function firstRender(stream: boolean): string {
  function Probe() {
    const setup = useFcSetup({ stream });
    return createElement("i", null, String(setup.streamUp));
  }
  return renderToStaticMarkup(createElement(Probe));
}

describe("useFcSetup stream state", () => {
  it("reports the stream down until a socket opens", () => {
    expect(firstRender(true)).toBe("<i>false</i>");
  });

  it("reports up when the stream is not held open", () => {
    expect(firstRender(false)).toBe("<i>true</i>");
  });
});
