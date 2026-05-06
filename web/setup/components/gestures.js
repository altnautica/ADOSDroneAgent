// Touch gesture helpers built on Pointer Events. Three gestures:
//  - pull-to-refresh on a scroll container
//  - horizontal swipe between siblings
//  - long-press for context menu
//
// All handlers are no-ops on non-touch input. ~80 LOC budget.

export function attachPullToRefresh(targetEl, { onRefresh, threshold = 70 } = {}) {
  let startY = 0;
  let pulling = false;
  let pulled = 0;

  const onDown = (ev) => {
    if (ev.pointerType !== "touch") return;
    if (targetEl.scrollTop > 0) return;
    startY = ev.clientY;
    pulling = true;
    pulled = 0;
  };

  const onMove = (ev) => {
    if (!pulling) return;
    pulled = Math.max(0, ev.clientY - startY);
    if (pulled > 0 && targetEl.scrollTop === 0) {
      targetEl.style.transform = `translateY(${Math.min(pulled / 2, threshold)}px)`;
    }
  };

  const onUp = () => {
    if (!pulling) return;
    pulling = false;
    targetEl.style.transform = "";
    if (pulled >= threshold && onRefresh) {
      try { onRefresh(); } catch (err) { console.warn(err); }
    }
    pulled = 0;
  };

  targetEl.addEventListener("pointerdown", onDown);
  targetEl.addEventListener("pointermove", onMove);
  targetEl.addEventListener("pointerup", onUp);
  targetEl.addEventListener("pointercancel", onUp);

  return () => {
    targetEl.removeEventListener("pointerdown", onDown);
    targetEl.removeEventListener("pointermove", onMove);
    targetEl.removeEventListener("pointerup", onUp);
    targetEl.removeEventListener("pointercancel", onUp);
  };
}

export function attachHorizontalSwipe(targetEl, { onLeft, onRight, threshold = 60 } = {}) {
  let startX = 0;
  let startY = 0;
  let active = false;

  const onDown = (ev) => {
    if (ev.pointerType !== "touch") return;
    startX = ev.clientX;
    startY = ev.clientY;
    active = true;
  };

  const onUp = (ev) => {
    if (!active) return;
    active = false;
    const dx = ev.clientX - startX;
    const dy = ev.clientY - startY;
    if (Math.abs(dy) > Math.abs(dx)) return;
    if (dx > threshold && onRight) onRight();
    else if (dx < -threshold && onLeft) onLeft();
  };

  targetEl.addEventListener("pointerdown", onDown);
  targetEl.addEventListener("pointerup", onUp);
  targetEl.addEventListener("pointercancel", () => { active = false; });

  return () => {
    targetEl.removeEventListener("pointerdown", onDown);
    targetEl.removeEventListener("pointerup", onUp);
  };
}

export function attachLongPress(targetEl, { onLongPress, ms = 500 } = {}) {
  let timer = null;
  const cancel = () => { if (timer) { clearTimeout(timer); timer = null; } };

  const onDown = (ev) => {
    cancel();
    timer = setTimeout(() => {
      if (onLongPress) onLongPress(ev);
    }, ms);
  };

  targetEl.addEventListener("pointerdown", onDown);
  targetEl.addEventListener("pointerup", cancel);
  targetEl.addEventListener("pointermove", cancel);
  targetEl.addEventListener("pointercancel", cancel);

  return () => {
    targetEl.removeEventListener("pointerdown", onDown);
    targetEl.removeEventListener("pointerup", cancel);
    targetEl.removeEventListener("pointermove", cancel);
    targetEl.removeEventListener("pointercancel", cancel);
  };
}
