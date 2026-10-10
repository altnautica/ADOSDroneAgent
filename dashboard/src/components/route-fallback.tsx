import { Skeleton } from "@/components/ui/skeleton";

/** Placeholder shown while a code-split page chunk loads. */
export function RouteFallback() {
  return (
    <div className="p-6 space-y-3" aria-busy="true">
      <Skeleton className="h-6 w-40" />
      <Skeleton className="h-4 w-64" />
      <Skeleton className="h-32 w-full" />
    </div>
  );
}
