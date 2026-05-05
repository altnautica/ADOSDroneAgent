# Public CLI command contract

Documents the `ados` CLI surface every backend implementation must honor. Source of truth: `src/ados/cli/main.py` in the Python full agent.

## Public commands

The public surface is four commands. Backends must implement all four with byte-for-byte equivalent argument parsing, exit codes, and output.

### `ados` (no subcommand)

Bare invocation. Default behavior depends on the terminal context.

| Context | Behavior |
|---|---|
| Attached to a TTY (stdin and stdout both) | Launch interactive read-only dashboard with rich live updates every 2 s |
| Not a TTY | Print plain-text status to stdout |

Calls `GET /api/v1/setup/status` to read state. Exits with code 0 on success, non-zero on connection error.

### `ados status`

Print agent status.

| Argument | Type | Default | Behavior |
|---|---|---|---|
| `--json`, `-j` | flag | false | Emit JSON to stdout instead of plain text |

Plain-text output: human-readable status block with profile, version, FC connection, video state, MAVLink state, services. Goes to stdout.

JSON output: the full `SetupStatus` object from `GET /api/v1/setup/status`, formatted with `json.dumps(data, indent=2)`. Goes to stdout.

Exit code 0 on success, non-zero on error.

### `ados update`

Check for and optionally install agent updates.

| Argument | Type | Default | Behavior |
|---|---|---|---|
| `--check-only` | flag | false | Check for updates without installing |
| `--yes`, `-y` | flag | false | Install without interactive prompt |
| `--json` | flag | false | Emit JSON result to stdout |

Calls in sequence:

1. `GET /api/ota` — current version
2. `POST /api/ota/check` — fetch available version (timeout 30 s)
3. `POST /api/ota/install` — install (timeout 300 s); skipped when `--check-only`
4. `POST /api/ota/restart` — restart agent (timeout 30 s); skipped when `--check-only`

JSON output shape:

```json
{
  "current": { "version": "0.10.1", "channel": "stable" },
  "check":   { "available": "0.10.2", "notes": "..." }
}
```

Without `--yes`, the command prompts `Install {version} now? [y/N]` before step 3.

Exit code 0 on success, non-zero on error.

### `ados uninstall`

Remove the agent from the system.

| Argument | Type | Default | Behavior |
|---|---|---|---|
| `--purge` | flag | false | Also remove the config directory `/etc/ados/` |
| `--yes`, `-y` | flag | false | Skip confirmation prompts |

Linux behavior:

1. Verify the user is root; refuse otherwise.
2. Stop and disable any running agent services.
3. Remove the install directory and data directory.
4. With `--purge`, also remove the config directory.
5. Reload the init system's daemon.
6. Print a single confirmation line: `ADOS Drone Agent uninstalled.`

macOS / dev behavior: tries `pipx uninstall`, then `uv tool uninstall`, then `pip uninstall ados-drone-agent` in order.

Without `--yes`, the command prompts twice: once to confirm the removal list, once to confirm the config-directory removal when `--purge` is set.

Exit code 0 on success, non-zero on error.

## Hidden commands

`ados demo` exists as a development entrypoint but is hidden from `--help`. It accepts a `--port` flag (default 8080) and starts the agent in demo mode with simulated telemetry. Backends are not required to implement this command.

## Subgroups

`ados plugin {install, enable, disable, remove, list, info, perms, logs, test}` is documented in the plugin system spec. Backends that ship the plugin host must implement these subcommands; backends that do not (the lightweight Rust backend at v0.1) skip them entirely. Operators see a "command not found" error on a backend that does not ship the plugin host.

## Output conventions

| Channel | Use |
|---|---|
| stdout | All success output. JSON output. The dashboard (when interactive). |
| stderr | Error messages from the underlying click/clap framework. |

`--json` is available on `status` and `update`. Other commands have no JSON mode and produce only human-readable output.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Generic error (connection failure, HTTP non-2xx, validation failure) |
| 2 | Argument parse error (handled by the CLI framework) |

Backends must use exit code 0 only when the requested action completed successfully; any failure is exit code 1 with a human-readable message on stderr.

## Conformance

A backend implementation is conformant when:

1. The four public commands accept the documented arguments and produce the documented output shapes.
2. JSON output on `status` and `update` matches the documented field names.
3. Interactive prompts on `update` and `uninstall` follow the documented copy and `--yes` skip behavior.
4. Exit codes match.
5. Hidden and subgroup commands are either implemented as documented or omitted entirely (no half-implementations).

Backends must not invent new public commands or rename existing flags.
