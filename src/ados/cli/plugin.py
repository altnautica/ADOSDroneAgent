"""``ados plugin`` CLI subcommand tree.

The lifecycle commands (``list``, ``info``, ``install``, ``enable``,
``disable``, ``remove``, ``perms``, ``pin``, ``unpin``, ``auto-update``)
drive the native plugin lifecycle over the local control surface
(``/api/plugins``); the agent owns plugin state, units and signature checks.
``logs`` reads the plugin log files, and ``lint``, ``test``, ``sign`` and
``keygen`` are local developer tools.

Output is human-readable by default; ``--json`` switches to a machine
envelope ``{"ok": bool, "code": int, "kind": str, "data": ...}``.

Exit code map:

* 0 success
* 1 generic failure
* 2 manifest invalid
* 3 signature invalid
* 4 permission denied (capability change)
* 5 plugin not found
* 6 wrong state
* 7 resource limit
* 8 compatibility failed
"""

from __future__ import annotations

# The subcommands live in sibling modules; importing them registers each one on
# ``plugin_group``. Every name this module used to define stays importable here.
from ados.cli._plugin_common import (  # noqa: F401
    _EXIT_BY_AGENT_CODE,
    _SIGNER_ID_RE,
    _UNSIGNED_HINT,
    EXIT_COMPATIBILITY,
    EXIT_GENERIC,
    EXIT_MANIFEST_INVALID,
    EXIT_NOT_FOUND,
    EXIT_OK,
    EXIT_PERMISSION_DENIED,
    EXIT_RESOURCE_LIMIT,
    EXIT_SIGNATURE_INVALID,
    EXIT_WRONG_STATE,
    KIND_BY_CODE,
    _agent,
    _call,
    _emit_err,
    _emit_ok,
    _fail,
    _failure,
    _is_builtin_install,
    _seg,
    plugin_group,
)
from ados.cli._plugin_dev import (  # noqa: F401
    lint,
    test_plugin,
)
from ados.cli._plugin_manage import (  # noqa: F401
    _catalog_entry,
    _grant_all,
    _plan_install,
    disable,
    enable,
    install,
    list_plugins,
    perms,
    remove,
)
from ados.cli._plugin_runtime import (  # noqa: F401
    auto_update,
    info,
    logs,
    pin,
    unpin,
)
from ados.cli._plugin_sign import (  # noqa: F401
    keygen,
    sign_plugin,
)
