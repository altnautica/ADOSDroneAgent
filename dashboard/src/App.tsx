import { Suspense } from "react";
import { Navigate, Route, Routes } from "react-router-dom";

import { DashboardAccessGate } from "@/components/access/DashboardAccessGate";
import { ErrorBoundary } from "@/shared/error-boundary";
import { AppShell } from "@/components/layout/app-shell";
import { SettingsLayout } from "@/components/layout/settings-layout";
import { ProfileGate } from "@/components/profile-gate";
import { ThemeProvider } from "@/components/theme-provider";
import { RouteFallback } from "@/components/route-fallback";
import { TooltipProvider } from "@/components/ui/tooltip";
import { IndexRedirect } from "@/routes/index-redirect";
import { NotFoundRoute } from "@/routes/not-found";
import {
  ROUTE_COMPONENTS,
  ROUTES,
  SETTINGS_PREFIX,
  type RouteEntry,
  type RouteKey,
} from "@/route-table";

function routeElement(entry: RouteEntry) {
  const Page = ROUTE_COMPONENTS[entry.key as RouteKey];
  if (!entry.profiles) return <Page />;
  return (
    <ProfileGate allow={entry.profiles} roles={entry.roles ?? undefined}>
      <Page />
    </ProfileGate>
  );
}

const TOP_ROUTES = ROUTES.filter((r) => !r.path.startsWith(SETTINGS_PREFIX));
const SETTINGS_ROUTES = ROUTES.filter((r) => r.path.startsWith(SETTINGS_PREFIX));

export function App() {
  return (
    <ThemeProvider>
      <TooltipProvider delayDuration={200}>
        <ErrorBoundary>
          <DashboardAccessGate>
            <Suspense fallback={<RouteFallback />}>
              <Routes>
                <Route element={<AppShell />}>
                  <Route index element={<IndexRedirect />} />
                  {TOP_ROUTES.map((r) => (
                    <Route key={r.key} path={r.path} element={routeElement(r)} />
                  ))}
                  <Route path="/settings" element={<SettingsLayout />}>
                    <Route index element={<Navigate to="profile" replace />} />
                    {SETTINGS_ROUTES.map((r) => (
                      <Route
                        key={r.key}
                        path={r.path.slice(SETTINGS_PREFIX.length)}
                        element={routeElement(r)}
                      />
                    ))}
                  </Route>
                  <Route path="*" element={<NotFoundRoute />} />
                </Route>
              </Routes>
            </Suspense>
          </DashboardAccessGate>
        </ErrorBoundary>
      </TooltipProvider>
    </ThemeProvider>
  );
}
