# Changelog

All notable changes to the ADOS Drone Agent are recorded here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
the project follows [Semantic Versioning](https://semver.org/).

## [0.9.5] - 2026-04-30

### Added

- `ados plugin lint` subcommand. Runs static analysis on a `.adosplug`
  archive (banned Python and JavaScript patterns, network imports
  versus declared permissions, vendor-binary flag, signature presence).
  Returns a scored report and exits non-zero on errors. Same rule set
  the registry submission gate will run server-side.

## [0.9.4] - 2026-04-30

### Added

- Driver-layer base classes for hardware-driver plugins under
  `ados.sdk.drivers`. Covers camera, gimbal, LiDAR, GPS, ESC, and
  payload actuator. Each base ships an abstract class plus frozen
  dataclasses for candidates, capabilities, and per-stream value types.
- Driver error hierarchy (`DriverError`, `DriverDeviceNotFound`,
  `DriverPermissionDenied`) chained under the existing plugin error
  base so driver faults flow through the supervisor's circuit breaker.
- Top-level `ados.sdk` package re-exporting the public driver surface
  for plugin authors.
- Contract tests for the driver base classes covering abstract-ness,
  trivial-subclass instantiability, frozen value types, and error
  hierarchy.
