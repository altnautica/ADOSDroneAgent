// Canonical web code shared by the agent's dashboard and HDMI cockpit. This
// tree is copied into each app's `src/shared/` by scripts/sync-web-shared.sh;
// edit it here, never in a copy.

export { consumeUrlKey, getApiKey, invalidateApiKeyCache, setApiKey } from "./api-key";
export { clearSession, getSession, onSessionChange, setSession } from "./session";
export {
  ApiError,
  apiFetch,
  credentialHeaders,
  errorDetail,
  isAuthChallenge,
  setAuthChallengeHandler,
  type FetchOptions,
} from "./api-fetch";
export { withMediaAuth } from "./media-auth";
export { WS_TICKET_PROTOCOL, mintWsTicket, ticketProtocols } from "./ws-ticket";
export {
  resolveWhepResource,
  startWhep,
  whepUrlFor,
  type WhepResult,
  type WhepSession,
} from "./whep";
export { HLS_MAX_FATAL_NETWORK_ERRORS, startHls, type HlsResult, type HlsSession } from "./hls";
export {
  VIDEO_NO_FRAME_MS,
  createVideoTransport,
  defaultDialer,
  defaultFrameWatcher,
  type TransportKind,
  type VideoFeedState,
  type VideoTransport,
  type VideoTransportOptions,
  type VideoTransportSnapshot,
} from "./video-transport";
export { useVideoTransport, type UseVideoTransportOptions } from "./use-video-transport";
export {
  PROFILE_RETRY_MS,
  invalidateProfile,
  normalizeProfile,
  probePairingInfo,
  probeProfile,
  useProfile,
  type AgentProfile,
  type PairingInfoLite,
} from "./use-profile";
export { useWakeLock, type WakeLockState } from "./wake-lock";
export {
  configureResource,
  useResource,
  type Resource,
  type ResourceHooks,
  type ResourceOptions,
} from "./use-resource";
export { ErrorBoundary } from "./error-boundary";
export * from "./format";
export { EXTENSION_THEMES, type ExtensionThemeId } from "./extension-theme.generated";
