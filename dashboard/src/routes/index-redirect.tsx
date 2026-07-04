import { HomeRoute } from "@/routes/home";

// Onboarding lives in the CLI installer, not a browser wizard, so the
// dashboard is operational immediately after install. The index route
// renders Home directly.
export function IndexRedirect() {
  return <HomeRoute />;
}
