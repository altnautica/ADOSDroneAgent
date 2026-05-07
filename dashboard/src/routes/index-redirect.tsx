import { Navigate } from "react-router-dom";

import { useStatus } from "@/hooks/use-status";
import { HomeRoute } from "@/routes/home";

// On first boot the agent isn't finalized; route to /setup so the wizard
// is the entry point. Once setup_finalized flips true, render Home.
// While the first poll is in flight we render Home (which already
// handles its own loading state) instead of flashing the wizard.
export function IndexRedirect() {
  const status = useStatus();

  if (status.isSuccess && status.data.setup_finalized === false) {
    return <Navigate to="/setup" replace />;
  }
  return <HomeRoute />;
}
