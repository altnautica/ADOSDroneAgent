// The semantic status tone every screen colours a state with, and the classes
// that paint it. Text never relies on the colour alone: each caller pairs the
// tone with a word or an icon.

/** A semantic status colour keyed off a link/service state string. */
export type Tone = "ok" | "warn" | "err" | "muted";

/** The text colour class for a tone. */
export function toneClass(tone: Tone): string {
  switch (tone) {
    case "ok":
      return "text-ok";
    case "warn":
      return "text-warn";
    case "err":
      return "text-err";
    default:
      return "text-muted-foreground";
  }
}

/** The fill colour class for a tone (dots, bars). */
export function toneDotClass(tone: Tone): string {
  switch (tone) {
    case "ok":
      return "bg-ok";
    case "warn":
      return "bg-warn";
    case "err":
      return "bg-err";
    default:
      return "bg-muted-foreground";
  }
}
