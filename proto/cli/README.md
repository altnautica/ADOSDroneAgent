# proto/cli/ — public CLI command contract

Documents the public `ados` CLI surface every backend implementation must honor.

## Files

| File | Purpose | Status |
|---|---|---|
| `commands.md` | Four-command surface — args, exit codes, output shapes | TODO |

## Public command surface

| Command | Purpose |
|---|---|
| `ados` (bare) | Read-only terminal status when attached to a TTY |
| `ados status` | Print agent status as plain text or `--json` |
| `ados update` | Check for and install agent updates (`--check-only`, `--yes`, `--json`) |
| `ados uninstall` | Remove the agent and optionally purge config (`--purge`, `--yes`) |

Backends may ship additional hidden development commands (e.g. `ados demo`) and subgroups (e.g. `ados plugin` on backends that support the plugin system). The four public commands above are the minimum surface every backend must implement.

## Source of truth

The Python full agent at `src/ados/cli/main.py` is the reference implementation.
