// Which pending reboot-gated changes a lighter restart can apply.

/** Whether every pending change is a video setting, which a restart of the
 *  video service applies without rebooting the board. */
export function videoRestartApplies(paths: readonly string[]): boolean {
  return paths.length > 0 && paths.every((p) => p.startsWith("video."));
}
