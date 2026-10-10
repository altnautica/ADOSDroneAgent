// The hls.js light build ships without its own declaration file; it exposes the
// same default export as the full build.
declare module "hls.js/dist/hls.light.mjs" {
  import Hls from "hls.js";
  export default Hls;
}
