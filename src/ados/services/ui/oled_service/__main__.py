"""Entrypoint for ``python -m ados.services.ui.oled_service``.

The package itself is not directly executable; this module exists so
``ados-oled.service`` (which invokes the package via ``-m``) routes to
the service's ``main()`` runner. Keeping the entrypoint thin lets the
real service code stay in ``service.py`` until that file's
decomposition lands as its own change.
"""

from ados.services.ui.oled_service.service import main

if __name__ == "__main__":
    main()
