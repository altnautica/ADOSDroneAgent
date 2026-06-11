"""Per-domain logd read-source helpers.

Each module in this package owns one telemetry domain's ``latest_<domain>()``
helper(s). They all reuse the one shared httpx-over-UDS client from
``ados.api.telemetry_source`` (the seam: ``_get_client`` + ``query_rows`` +
``latest_metrics``), so there is one connection to the store, one timeout
policy, one gap-tolerance contract. A lane adds a domain by adding a file
here; the shared client file is never edited.
"""
