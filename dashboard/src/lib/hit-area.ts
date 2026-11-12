// On a touch screen a control keeps its drawn size but takes taps from a
// larger box: an invisible `::after` layer that reaches past the edges, so a
// 20 px switch or a 36 px button answers a 44 px finger.
export const COARSE_HIT_AREA =
  "relative [@media(pointer:coarse)]:after:absolute [@media(pointer:coarse)]:after:-inset-3 [@media(pointer:coarse)]:after:content-['']";

// Same expansion for a control already 36 px tall: 4 px each way gives 44.
export const COARSE_HIT_AREA_SM =
  "relative [@media(pointer:coarse)]:after:absolute [@media(pointer:coarse)]:after:-inset-1 [@media(pointer:coarse)]:after:content-['']";
