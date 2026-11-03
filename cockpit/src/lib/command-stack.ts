// The overlay command stack. Every open overlay (sheet, dialog, menu) and any
// screen that owns a command (a console scaffold's tabs) registers a handler
// here; the navigator offers each folded NavCommand to the newest enabled
// handler first and only routes it itself when none takes it. So the panel
// buttons, the gamepad and the keyboard drive whatever is on top, and nothing
// underneath moves while an overlay is open. Mirrors the Android console
// command dispatcher.

import type { NavCommand } from "@/nav/navigator";

/** Returns true when it took the command. */
export type CommandHandler = (cmd: NavCommand) => boolean;

const handlers: CommandHandler[] = [];

/** Register a handler on top of the stack. Returns its removal. */
export function pushCommandHandler(handler: CommandHandler): () => void {
  handlers.push(handler);
  return () => {
    const i = handlers.lastIndexOf(handler);
    if (i >= 0) handlers.splice(i, 1);
  };
}

/** Offer a command to the stack, newest first. True when a handler took it. */
export function offerCommand(cmd: NavCommand): boolean {
  for (let i = handlers.length - 1; i >= 0; i -= 1) {
    if (handlers[i](cmd)) return true;
  }
  return false;
}

/** Whether any handler is registered (an overlay is open). */
export function hasCommandHandlers(): boolean {
  return handlers.length > 0;
}
