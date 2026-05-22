import { Navigate } from "react-router-dom";

import { useStatus } from "@/hooks/use-status";
import { HomeRoute } from "@/routes/home";

// Setup is non-blocking. On a fresh boot the wizard is offered but the
// operator can click Skip to Home at any point and reach the dashboard
// immediately. Either setup_finalized OR setup_skipped routes to Home.
// While the first poll is in flight we render nothing (avoids a flash
// of either Home or the wizard before the data lands).
export function IndexRedirect() {
  const status = useStatus();

  if (status.isLoading) {
    return null;
  }
  const finalized = status.data?.setup_finalized === true;
  const skipped = status.data?.setup_skipped === true;
  if (!finalized && !skipped) {
    return <Navigate to="/setup" replace />;
  }
  return <HomeRoute />;
}
