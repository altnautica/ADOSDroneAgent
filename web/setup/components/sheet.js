// Re-export of the sheet primitive so callers have a stable per-component
// import path. The implementation lives in ../components.js so legacy
// helpers and the new dashboard share one definition.

export { sheet } from "../components.js";
