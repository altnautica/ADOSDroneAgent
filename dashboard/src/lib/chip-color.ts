// The flight chip's state colours are video-overlay hues, tuned for dark
// video. As text on the dashboard card they are mixed toward the theme's
// foreground, so they darken on the light theme and stay bright on the dark
// one; the raw hue is kept for the border.
export function chipTextColor(color: string): string {
  return `color-mix(in oklab, ${color} 62%, hsl(var(--foreground)))`;
}
