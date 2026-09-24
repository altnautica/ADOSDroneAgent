"""Ground-station profile services.

The receive plane, networking, USB tether and front panel run natively
(``ados-groundlink``, ``ados-net``, ``ados-pic``). The Python modules here
are the mediamtx WHEP republisher (``mediamtx``), mesh pairing
(``pairing_daemon`` and its REST client), the mesh role and batman-adv
bring-up, WFB pair state, and the read-side managers the REST routes use.
Each daemon module is runnable via ``python -m`` for its systemd unit.
"""
