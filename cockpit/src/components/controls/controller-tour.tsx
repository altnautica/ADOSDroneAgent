// The controller tour on the first controller connect, on whichever screen is
// up: three cards (sticks, safety buttons, skills) in a sheet, shown once per
// cockpit (`tourSeen`). While the browser has announced a controller it has
// not yet reported (the Gamepad API waits for a button press), the sheet asks
// for a button press first.

import { useState } from "react";

import { Sheet } from "@/components/ui/sheet";
import { cockpitText } from "@/lib/cockpit-text";
import { TOUR_CARDS, tourStage } from "@/lib/controller-tour";
import { useControlsStore } from "@/stores/controls-store";
import { useInputStore } from "@/stores/input-store";

export function ControllerTour() {
  const pad = useInputStore((s) => s.pad);
  const announced = useInputStore((s) => s.padAnnounced);
  const seen = useControlsStore((s) => s.tourSeen);
  const markSeen = useControlsStore((s) => s.markTourSeen);
  const [card, setCard] = useState(0);
  const stage = tourStage(pad !== null, announced, seen);
  if (stage === "none") return null;
  const id = TOUR_CARDS[card];
  const last = card === TOUR_CARDS.length - 1;
  return (
    <Sheet
      title="Controller tour"
      onDismiss={markSeen}
      closeLabel="Skip"
      footer={
        stage === "tour" ? (
          <div className="flex gap-[0.4rem]" data-testid="controls-tour">
            <button type="button" onClick={markSeen} className="min-h-[44px] flex-1 rounded-lg border border-border px-[0.7rem] text-[0.85rem]">
              Skip
            </button>
            <button
              type="button"
              onClick={() => (last ? markSeen() : setCard(card + 1))}
              className="min-h-[44px] flex-1 rounded-lg border border-primary bg-primary/25 px-[0.7rem] text-[0.85rem]"
            >
              {last ? "Done" : "Next"}
            </button>
          </div>
        ) : null
      }
    >
      {stage === "press" ? (
        <p className="text-[0.9rem]">{cockpitText("controls.press_any_button")}</p>
      ) : (
        <>
          <h3 className="text-[1rem] font-semibold">{cockpitText(`controls.tour.${id}.title`)}</h3>
          <p className="text-[0.85rem]">{cockpitText(`controls.tour.${id}.body`)}</p>
        </>
      )}
    </Sheet>
  );
}
