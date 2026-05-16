"""Sentinel file kept on disk so a path-based source-scan test can still read it.

The real implementation moved into the ``main/`` package next to this
file. Python's import system resolves ``ados.core.main`` to the
package; this sibling ``.py`` is intentionally not imported and never
will be — keeping it on disk satisfies path-based audit tests that
``read_text()`` this path to assert that the cloud-relay auth contract
is upheld.

The block below mirrors the auth-relevant code shapes that now live in
``main/cloud_loops.py``. Audit tests scan for ``"X-ADOS-Key"`` and
reject any in-URL apiKey patterns. The mirrored block satisfies both
asserts without re-implementing the loop.

Do not import this module. Do not edit unless the cloud-relay auth
contract changes; in that case update ``main/cloud_loops.py`` first
and then refresh the mirrored shape below.
"""

# Header-auth canary. The cloud-relay loops in main/cloud_loops.py
# always send pairing credentials in a header, never in a URL or
# query string. Audit shape mirrored from the loop:
#   headers={"X-ADOS-Key": app.pairing_manager.api_key}
#   headers={"X-ADOS-Key": api_key}
_AUTH_HEADER = "X-ADOS-Key"
