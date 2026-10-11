//! The front's route map: which paths it serves natively, which it forwards to
//! the residual Python, and what answers everything else.
//!
//! The LAN front owns the TCP port and answers a fixed set of routes itself.
//! A request that matches no native route goes to the router fallback, which
//! dispatches on [`classify`]: the permanent-Python prefixes
//! ([`PERMANENT_PYTHON_PREFIXES`]) are forwarded to the residual app over its
//! internal Unix socket ([`crate::proxy`]); `/hls/*` is relayed to mediamtx on
//! loopback; the operator UI bundles are served from disk; and any other `/api`
//! path is a 404 (or a 405 when the path is native under another method).
//!
//! [`is_native`] is the single source of truth the auth edge ([`crate::serve`])
//! consults. A native route takes the front's own auth lane: rate limiter,
//! pairing gate, MCP-scope admission. A non-native route is authenticated on the
//! proxied lane instead ([`crate::serve`]'s `proxied_auth_then_forward`: API key,
//! HMAC, dashboard session, WS ticket) before the fallback answers it.
//!
//! The native set is not a list kept here. It is the table the route builder
//! records while it registers the router ([`crate::routes::build`]), installed
//! into [`NATIVE`] when the served router is built, so the edge and the router
//! cannot disagree about what is native.

use std::sync::OnceLock;

use http::Method;

use crate::routes::table::NativeRoute;

/// How the front handles a given request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMode {
    /// The front answers this route itself.
    Native,
    /// A permanent-Python prefix, forwarded to the residual app.
    Residual,
    /// The HLS playback fallback, relayed to mediamtx on loopback.
    Hls,
    /// The browser dashboard and the on-box cockpit, served from their bundles.
    OperatorUi,
    /// An `/api` path no route serves. `other_method` is true when the path is a
    /// native route under a different method (a 405 rather than a 404).
    Unrouted { other_method: bool },
}

/// The native route table plus a per-method matcher compiled from it once.
/// `is_native` runs on every inbound request, so a request is matched by one
/// radix-tree lookup in its own method's tree, the same matcher (matchit) and
/// the same `{name}` / `{*name}` template syntax axum routes on.
pub struct NativeTable {
    routes: Vec<NativeRoute>,
    by_method: Vec<(Method, matchit::Router<()>)>,
}

impl NativeTable {
    pub fn new(routes: Vec<NativeRoute>) -> Self {
        let mut by_method: Vec<(Method, matchit::Router<()>)> = Vec::new();
        for route in &routes {
            let idx = match by_method.iter().position(|(m, _)| *m == route.method) {
                Some(i) => i,
                None => {
                    by_method.push((route.method.clone(), matchit::Router::new()));
                    by_method.len() - 1
                }
            };
            // axum inserted every template into the same matcher when the router
            // was built, and refuses a duplicate `(method, path)`, so a per-method
            // subset always inserts cleanly.
            by_method[idx]
                .1
                .insert(route.path, ())
                .unwrap_or_else(|e| panic!("native route {} {}: {e}", route.method, route.path));
        }
        Self { routes, by_method }
    }

    /// Every row, in registration order.
    pub fn routes(&self) -> &[NativeRoute] {
        &self.routes
    }

    /// True iff this exact `(method, path)` is a native route. The method must
    /// match exactly: a `POST` to a `GET`-only native path is NOT native.
    pub fn is_native(&self, method: &Method, path: &str) -> bool {
        self.by_method
            .iter()
            .find(|(m, _)| m == method)
            .is_some_and(|(_, tree)| tree.at(path).is_ok())
    }

    /// True when the path is native under any method.
    fn serves_path(&self, path: &str) -> bool {
        self.by_method.iter().any(|(_, tree)| tree.at(path).is_ok())
    }

    /// How the front handles a `(method, path)`.
    pub fn classify(&self, method: &Method, path: &str) -> RouteMode {
        if self.is_native(method, path) {
            return RouteMode::Native;
        }
        if is_permanent_python_path(path) {
            return RouteMode::Residual;
        }
        if path == "/hls" || path.starts_with("/hls/") {
            return RouteMode::Hls;
        }
        let api = path == "/api" || path.starts_with("/api/");
        let whep = path == "/whep" || path.starts_with("/whep/");
        if api || whep {
            // A native path under the wrong method is a 405, any other API path a
            // 404. Never the dashboard: a typo'd endpoint must fail crisply, and
            // the media plane is never the shell.
            return RouteMode::Unrouted {
                other_method: self.serves_path(path),
            };
        }
        RouteMode::OperatorUi
    }
}

/// The native table the edge consults, installed by
/// [`crate::routes::build_router`] from the routes it registered.
pub static NATIVE: OnceLock<NativeTable> = OnceLock::new();

/// Install the served router's table. The process builds its router once per
/// listener with the same flags, so a second install carries the same table and
/// is dropped.
pub fn install(routes: Vec<NativeRoute>) {
    if let Err(rejected) = NATIVE.set(NativeTable::new(routes)) {
        let installed = native().routes().len();
        if rejected.routes().len() != installed {
            tracing::warn!(
                installed,
                rejected = rejected.routes().len(),
                "a second router was built with a different native route table; the first stays in force"
            );
        }
    }
}

/// The installed table. A process that consults the edge without building the
/// served router (an edge unit test) gets the default build's table: every
/// route except the hid-rust writes, which is exactly what a build with
/// hid-rust off installs.
fn native() -> &'static NativeTable {
    NATIVE.get_or_init(|| NativeTable::new(crate::routes::describe(false)))
}

/// The full native route set as plain data, for the tools that enumerate it
/// rather than query it: the `docs/api-surface.md` generator and the CI check
/// that every client path literal resolves against a real route. The first field
/// is the HTTP method, or `WS` for a route that only answers a WebSocket upgrade.
/// It includes the hid-rust writes, which a hid-rust build serves.
pub fn native_route_table() -> Vec<(String, &'static str)> {
    crate::routes::describe(true)
        .into_iter()
        .map(|r| (r.label(), r.path))
        .collect()
}

/// The path prefixes the agent keeps in Python by design: the ecosystem-bound
/// features (vision/AI, the setup facade, peripherals, the LCD/OLED display
/// surface). These are the ONLY paths the front forwards to the residual app;
/// every other non-native path is answered here ([`classify`]). When the
/// residual upstream is gone (the zero-Python headless profile), the proxy
/// answers `501` for these: the feature is absent on this profile, not an
/// unknown path.
///
/// These are the paths as MOUNTED, not as the feature is named. The FastAPI app
/// includes each router under `/api` and several routers carry their own `/v1`
/// prefix, so the served path is `/api/v1/setup`, not `/api/setup`. The
/// unversioned `/api/peripherals` (the hardware scan) and `/api/v1/peripherals`
/// (the peripheral plugin registry) are different surfaces and are listed
/// separately. Touch-panel calibration needs no entry of its own: those routes
/// hang off the display and setup routers.
pub const PERMANENT_PYTHON_PREFIXES: [&str; 5] = [
    "/api/vision",
    "/api/v1/setup",
    "/api/peripherals",
    "/api/v1/peripherals",
    "/api/v1/display",
];

/// How the front handles a `(method, path)`. The auth edge consults
/// [`is_native`] alone; this is what the router fallback dispatches on for every
/// request that is not a native route.
pub fn classify(method: &Method, path: &str) -> RouteMode {
    native().classify(method, path)
}

/// True iff the front serves this exact `(method, path)` itself. The auth edge
/// keeps its native posture for these and proxies everything else; the proxy
/// fallback never fires for a native route (axum routes it first).
pub fn is_native(method: &Method, path: &str) -> bool {
    native().is_native(method, path)
}

/// True when a path sits under a known permanent-Python prefix. Used only to pick
/// `501` over `404` in the graceful-degradation reply when the upstream is gone.
pub fn is_permanent_python_path(path: &str) -> bool {
    PERMANENT_PYTHON_PREFIXES.iter().any(|p| {
        path.strip_prefix(p)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests;
