import { cn } from "@/lib/utils";

/** The desktop aside: held under the sticky header (h-14 plus its 1 px
 *  border) at the viewport's height, so the nav scrolls on its own and the
 *  collapse button stays at the bottom of the viewport however long the page. */
export function sidebarAsideClass(collapsed: boolean): string {
  return cn(
    "hidden lg:flex sticky top-[57px] h-[calc(100dvh-57px)] self-start shrink-0 flex-col overflow-hidden border-r border-border bg-background/40 transition-[width]",
    collapsed ? "w-16" : "w-56",
  );
}
