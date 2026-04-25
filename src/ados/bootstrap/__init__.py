"""Bootstrap helpers for ADOS Drone Agent.

Modules here run at first boot or supervisor startup, before the main
service graph comes up. profile_detect picks the air vs ground-station
profile automatically based on hardware fingerprint.
"""
