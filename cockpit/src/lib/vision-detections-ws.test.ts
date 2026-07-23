import { describe, expect, it } from "vitest";

import { mapWireBatch, parseWireDetectionJson } from "@/lib/vision-detections-ws";

describe("mapWireBatch", () => {
  it("maps a v2 batch onto the store shape (snake_case -> camelCase)", () => {
    const mapped = mapWireBatch({
      v: 2,
      model_id: "yolov8n",
      camera_id: "cam-0",
      frame_id: 42,
      ts_ms: 1000,
      frame_width: 1280,
      frame_height: 720,
      detections: [
        {
          bbox: { x: 10, y: 20, width: 30, height: 40 },
          class_label: "person",
          confidence: 0.9,
          track_id: 7,
          lock_state: "locked",
        },
      ],
    });
    expect(mapped).not.toBeNull();
    expect(mapped!.modelId).toBe("yolov8n");
    expect(mapped!.cameraId).toBe("cam-0");
    expect(mapped!.frameId).toBe(42);
    expect(mapped!.frameWidth).toBe(1280);
    expect(mapped!.frameHeight).toBe(720);
    expect(mapped!.detections).toHaveLength(1);
    const d = mapped!.detections[0];
    expect(d.bbox).toEqual({ x: 10, y: 20, width: 30, height: 40 });
    expect(d.classLabel).toBe("person");
    expect(d.confidence).toBe(0.9);
    expect(d.trackId).toBe(7);
    expect(d.lockState).toBe("locked");
  });

  it("DROPS a batch whose wire version we do not speak (never mis-maps)", () => {
    expect(mapWireBatch({ v: 3, detections: [] })).toBeNull();
    expect(mapWireBatch({ v: 1, detections: [] })).toBeNull();
  });

  it("maps a batch with no version field (back-compat)", () => {
    const mapped = mapWireBatch({ model_id: "m", camera_id: "c", detections: [] });
    expect(mapped).not.toBeNull();
    expect(mapped!.modelId).toBe("m");
  });

  it("defaults absent frame dimensions to the engine's normalized size", () => {
    const mapped = mapWireBatch({ v: 2, detections: [] });
    expect(mapped!.frameWidth).toBe(640);
    expect(mapped!.frameHeight).toBe(480);
  });

  it("only accepts the three valid lock states, else null", () => {
    const mapped = mapWireBatch({
      v: 2,
      detections: [
        { class_label: "a", confidence: 0.5, lock_state: "uncertain" },
        { class_label: "b", confidence: 0.5, lock_state: "lost" },
        { class_label: "c", confidence: 0.5, lock_state: "bogus" },
        { class_label: "d", confidence: 0.5 },
      ],
    });
    expect(mapped!.detections.map((d) => d.lockState)).toEqual([
      "uncertain",
      "lost",
      null,
      null,
    ]);
  });

  it("carries a box-less percept through with no bbox", () => {
    const mapped = mapWireBatch({
      v: 2,
      detections: [{ class_label: "region", confidence: 0.4 }],
    });
    expect(mapped!.detections[0].bbox).toBeUndefined();
  });

  it("coerces a non-numeric track id to null", () => {
    const mapped = mapWireBatch({
      v: 2,
      detections: [{ class_label: "x", confidence: 0.5, track_id: "nope" }],
    });
    expect(mapped!.detections[0].trackId).toBeNull();
  });
});

describe("parseWireDetectionJson", () => {
  it("parses valid JSON into a batch", () => {
    const mapped = parseWireDetectionJson(
      JSON.stringify({ v: 2, model_id: "m", camera_id: "c", detections: [] }),
    );
    expect(mapped).not.toBeNull();
    expect(mapped!.cameraId).toBe("c");
  });

  it("returns null for malformed JSON (never throws)", () => {
    expect(parseWireDetectionJson("{ not json")).toBeNull();
    expect(parseWireDetectionJson("")).toBeNull();
  });

  it("returns null for a non-object payload (array / primitive)", () => {
    expect(parseWireDetectionJson("[1,2,3]")).toBeNull();
    expect(parseWireDetectionJson("42")).toBeNull();
  });

  it("returns null for a version it does not speak", () => {
    expect(parseWireDetectionJson(JSON.stringify({ v: 99, detections: [] }))).toBeNull();
  });
});
