// Theme controller. Reads store.theme and applies the matching class on
// the html element. Modes: auto | dark | light | outdoor.
//
// auto follows prefers-color-scheme. Outdoor is a deliberate choice the
// operator makes for sun legibility, not auto-derived.

const CLASS_MAP = {
  dark: "theme-dark",
  light: "theme-light",
  outdoor: "theme-outdoor",
};

export function mountTheme(htmlEl, store) {
  const mq = window.matchMedia("(prefers-color-scheme: light)");

  const apply = () => {
    const t = store.get().theme;
    htmlEl.classList.remove("theme-dark", "theme-light", "theme-outdoor");

    let resolved = t;
    if (t === "auto") {
      resolved = mq.matches ? "light" : "dark";
    }
    const cls = CLASS_MAP[resolved];
    if (cls) htmlEl.classList.add(cls);
  };

  store.subscribe(apply);
  if (mq.addEventListener) mq.addEventListener("change", apply);
  else if (mq.addListener) mq.addListener(apply);
  apply();

  return { apply };
}
