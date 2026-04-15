"""Setup webapp services (MSN-025 Wave B).

Captive DNS and probe responder run only while first-time setup is
in progress. Once the user completes setup an external writer drops
`/var/lib/ados/setup-complete` and the unit exits 0. The systemd
unit (wired in Wave D) uses `Restart=no` so the responder does not
keep reviving itself post-setup.
"""
