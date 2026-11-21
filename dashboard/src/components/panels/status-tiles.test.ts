import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import "@/shared/i18n";

import { FollowStatus } from "@/components/panels/status-tiles";
import { freshNavFrames, homeFollow } from "@/lib/home-status";

/** The follow line the Home card draws for a nav-status body: the fresh
 *  follow frame through the shared rule, rendered. */
const line = (data: Record<string, unknown>, ageMs = 0): string | null => {
  const fresh = freshNavFrames({ follow: { age_ms: ageMs, data } }, 1_000, 1_000);
  const follow = homeFollow(fresh.follow, fresh.pilot);
  return follow ? renderToStaticMarkup(createElement(FollowStatus, { follow })) : null;
};

describe("Home vehicle status: the follow line", () => {
  it("shows the phase word and names the phone as the subject source", () => {
    const out = line({ active: true, phase: "following", track_id: 3, subject_source: "beacon" });
    expect(out).toContain("Following #3");
    expect(out).toContain('data-testid="follow-subject-source"');
    expect(out).toContain("Following on phone position");
  });

  it("names no source while the follow flies on vision", () => {
    const out = line({ active: true, phase: "following", track_id: 3, subject_source: "vision" });
    expect(out).toContain("Following #3");
    expect(out).not.toContain('data-testid="follow-subject-source"');
  });

  it("renders the waiting prompt, its body and the resume and stop actions in the spec's words", () => {
    const out = line({
      active: true,
      phase: "holding-await-operator",
      hold_reason: "search-exhausted",
      reacquire_mode: "smart",
      track_id: 7,
      operator_resume_seq: 4,
    });
    expect(out).toContain("Waiting for you #7");
    expect(out).toContain("Target lost: holding, waiting for instructions");
    expect(out).toContain("The search found no match for the subject.");
    expect(out).toMatch(/<button[^>]*>Search again<\/button>/);
    expect(out).toMatch(/<button[^>]*>Stop following<\/button>/);
  });

  it("draws no line while follow is off", () => {
    expect(line({ active: false })).toBeNull();
  });
});
