# proto/ — language-neutral contracts

This directory holds the canonical contracts that any agent implementation in this repository must conform to. Contracts are language-neutral so multiple backends (Python full agent under `src/ados/`, Rust lite agent under `agents/lite-rs/`, future implementations) can speak the same protocol surface.

## Layout

| Domain | Path | Purpose |
|---|---|---|
| Cloud | `proto/cloud/` | MQTT topic schema, cloud HTTP heartbeat OpenAPI, RTSP path conventions for cloud relay |
| Setup | `proto/setup/` | OpenAPI for the universal setup webapp REST surface (`/api/v1/setup/*`) |
| State | `proto/state/` | Wire-format spec for the state IPC stream and JSON shape conventions |
| IPC | `proto/ipc/` | Unix-socket framing for in-process inter-service communication |
| CLI | `proto/cli/` | Public CLI command contract — argument shapes, exit codes, output conventions |

## Conformance

A backend implementation is "valid" when it passes the conformance test suite that targets the contracts above. CI runs the same suite against every backend in `agents/`, ensuring the cloud relay and ground control station treat all backends interchangeably.

## Adding a new contract

1. Add the contract document under the appropriate sub-directory.
2. Update the table above.
3. Update the conformance test suite to verify the new contract.
4. Document the source-of-truth code module (if extracted from an existing implementation) so future backends can verify against the same reference.
