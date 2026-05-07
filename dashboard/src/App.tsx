import { Routes, Route, Navigate } from "react-router-dom";

import { Placeholder } from "./components/placeholder";

// Phase 1 ships only the placeholder route. Phases 2+ replace this
// with the layout shell and real routes (home, setup wizard, settings,
// telemetry, video, plugins, peripherals, suites, ota, logs, ros,
// diagnostics).
export function App() {
  return (
    <Routes>
      <Route path="/" element={<Placeholder />} />
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
