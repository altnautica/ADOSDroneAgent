"""Compute-node Python helpers.

Machine-learning steps the compute node runs that belong in the Python
ecosystem (ML inference), invoked by the Rust compute engine. Kept small and
lazy-importing so the package imports without the heavy ML stack present.
"""
