import { Construction } from "lucide-react";
import { Link } from "react-router-dom";

import { Button } from "@/components/ui/button";

export function ComingSoonRoute({
  title,
  description,
  shipsIn,
}: {
  title: string;
  description?: string;
  shipsIn?: string;
}) {
  return (
    <div className="max-w-md space-y-4 py-12">
      <div className="inline-flex items-center justify-center h-12 w-12 rounded-lg bg-muted">
        <Construction className="h-5 w-5 text-muted-foreground" />
      </div>
      <h1 className="text-xl font-semibold tracking-tight">{title}</h1>
      <p className="text-sm text-muted-foreground">
        {description ?? "This page is part of the dashboard rebuild and will land in a follow-up release."}
        {shipsIn ? <span className="block mt-1 font-mono text-xs">target: {shipsIn}</span> : null}
      </p>
      <Button asChild variant="outline" size="sm">
        <Link to="/">Back to Home</Link>
      </Button>
    </div>
  );
}
