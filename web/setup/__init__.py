"""Universal setup webapp assets, vanilla HTML, CSS, and ES modules.

Browser-side surface for the agent's REST API at ``/api/v1/setup/*``.
A single ``index.html`` shell loads ``app.js``, which routes (History
API) into per-view modules under ``views/`` and per-component modules
under ``components/``. Multiple agent backends in this repository
serve these assets from this canonical location, so the operator UX
stays identical regardless of which backend is running.

Tree:

  - index.html               SPA shell, single mount point
  - dashboard.css            Mobile, tablet, and desktop visual system
  - app.js                   Bootstrap, route table, polling
  - router.js                History API client router
  - state.js                 Pub-sub store and polling helper
  - components.js            DOM helpers (el, chip, statTile, panel, ...)
  - components/              Header, bottom dock, command palette, sheets
  - views/dashboard.js       The one-pager
  - views/logs.js            Log streaming view
  - views/settings/          Profile, cloud, network, display, advanced
"""
