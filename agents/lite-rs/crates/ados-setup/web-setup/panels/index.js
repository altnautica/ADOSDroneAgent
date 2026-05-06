// Panel barrel. Single import path so the dashboard view doesn't have
// to track every per-panel module file.

export { renderVideoPanel } from "./drone/video.js";
export { renderFcPanel } from "./drone/fc.js";
export { renderMavlinkPanel } from "./drone/mavlink.js";
export { renderCameraPanel } from "./drone/camera.js";
export { renderSensorsPanel } from "./drone/sensors.js";
export { renderPluginsPanel } from "./drone/plugins.js";

export { renderCloudPanel } from "./common/cloud.js";
export { renderNetworkPanel } from "./common/network.js";
export { renderServicesPanel } from "./common/services.js";
