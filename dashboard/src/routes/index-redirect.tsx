import { Navigate } from "react-router-dom";

import { useStatus } from "@/hooks/use-status";
import { HomeRoute } from "@/routes/home";

function sessionSkipFlag(): boolean {
  // Mirrors the on-disk setup_skipped state for the case where the
  // POST that persists the flag failed transiently (5xx, network
  // hiccup). The setup-route handler writes this flag before
  // navigating home so the next reload doesn't bounce the operator
  // back into the wizard while the agent is briefly unreachable.
  try {
    return window.sessionStorage.getItem("ados:setup_skipped") === "1";
  } catch {
    return false;
  }
}

// Setup is non-blocking. On a fresh boot the wizard is offered but the
// operator can click Skip to Home at any point and reach the dashboard
// immediately. Either setup_finalized OR setup_skipped (server- or
// session-side) routes to Home. While the first poll is in flight we
// render nothing (avoids a flash of either Home or the wizard before
// the data lands).
export function IndexRedirect() {
  const status = useStatus();

  if (status.isLoading) {
    return null;
  }
  const finalized = status.data?.setup_finalized === true;
  const skipped = status.data?.setup_skipped === true || sessionSkipFlag();
  if (!finalized && !skipped) {
    return <Navigate to="/setup" replace />;
  }
  return <HomeRoute />;
}
