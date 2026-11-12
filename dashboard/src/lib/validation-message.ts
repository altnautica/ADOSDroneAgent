// A form check that failed, in words. The schema's own message is English
// written for developers and names the wire path (`self_hosted.url: Invalid
// url`); the operator reads which field is wrong and how to fix it.

import type { ParseKeys } from "i18next";

import i18n from "@/shared/i18n";

const FIELD_KEYS: Record<string, ParseKeys> = {
  "self_hosted.url": "settingsMore.validation.url",
  "self_hosted.mqtt_broker": "settingsMore.validation.broker",
  "self_hosted.mqtt_port": "settingsMore.validation.port",
  board_override: "settingsMore.validation.boardOverride",
};

export function issueMessage(issue: { path: ReadonlyArray<PropertyKey> }): string {
  const key = FIELD_KEYS[issue.path.map(String).join(".")];
  return i18n.t(key ?? "settingsMore.validation.invalid");
}
