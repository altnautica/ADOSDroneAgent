// The vision inference backends the native engine selects between. Mirrors the
// `vision.backend` enum in schemas/agent-config.schema.json (a unit test holds
// the two equal); the config write refuses any other value.

export const VISION_BACKEND_OPTIONS: ReadonlyArray<{
  value: "auto" | "rknn" | "hailo" | "onnx" | "mock";
  label: string;
  description: string;
}> = [
  {
    value: "auto",
    label: "Auto",
    description: "Rockchip NPU on Rockchip boards, else ONNX on the CPU. Never picks Hailo.",
  },
  { value: "rknn", label: "RKNN", description: "Rockchip NPU." },
  { value: "hailo", label: "Hailo", description: "Hailo-8 accelerator (Pi 5 AI HAT). Choose it explicitly." },
  { value: "onnx", label: "ONNX", description: "CPU inference via ONNX Runtime." },
  { value: "mock", label: "Mock", description: "Synthetic detections for bench tests." },
];
