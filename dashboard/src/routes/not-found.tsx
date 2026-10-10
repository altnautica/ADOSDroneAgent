import { SearchX } from "lucide-react";
import { Link, useLocation } from "react-router-dom";

import { Button } from "@/components/ui/button";

export function NotFoundRoute() {
  const { pathname } = useLocation();
  return (
    <div className="max-w-md space-y-4 py-12">
      <div className="inline-flex items-center justify-center h-12 w-12 rounded-lg bg-muted">
        <SearchX className="h-5 w-5 text-muted-foreground" aria-hidden />
      </div>
      <h1 className="text-xl font-semibold tracking-tight">Page not found</h1>
      <p className="text-sm text-muted-foreground">
        There is no dashboard page at{" "}
        <span className="font-mono text-xs break-all select-text">{pathname}</span>.
        It may belong to a different node profile.
      </p>
      <Button asChild variant="outline" size="sm">
        <Link to="/">Back to Home</Link>
      </Button>
    </div>
  );
}
