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

// Setup is opt-in. The dashboard Home renders at `/` whenever the
// agent reports the rig is operational (`setup_complete`) OR the
// operator finalized / skipped the wizard previously. The Setup
// button in the header is the only forced surface; the wizard never
// hijacks the root URL. While the first poll is in flight we render
// nothing to avoid flashing either Home or the wizard.
export function IndexRedirect() {
  const status = useStatus();

  if (status.isLoading) {
    return null;
  }
  const complete = status.data?.setup_complete === true;
  const finalized = status.data?.setup_finalized === true;
  const skipped = status.data?.setup_skipped === true || sessionSkipFlag();
  if (!complete && !finalized && !skipped) {
    return <Navigate to="/setup" replace />;
  }
  return <HomeRoute />;
}
