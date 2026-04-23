"""On-drone ML model registry.

On-demand install only — no bundled models.
Features declare model dependencies via their manifest.
The Registry downloads, SHA256-verifies, and installs models on demand.

Model categories:
  detect       — object detection (RKNN > TFLite > ONNX > CPU)
  tracker      — multi-object tracker
  depth        — depth estimation (MiDaS)
  embed        — CLIP embedding
  caption      — image captioning (VLM)
  narrate      — text narration (SmolLM2 class, tier-3+ only)
  tts          — text to speech
  asr          — speech to text
  translate    — translation
  anomaly      — anomaly detection
  ocr          — text recognition
"""
