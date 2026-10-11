//! The route-registration builder: one call registers a route on the axum
//! router AND records it in the native route table.
//!
//! The LAN-edge auth applies its native posture (rate limiter, pairing gate,
//! MCP-scope admission) only to paths the table lists, so a route served by the
//! router but missing from the table would be served on the proxied lane, and a
//! table row the router never registered would read as native while answering
//! 404. Recording both in one call is what makes that drift impossible: there is
//! no second list to keep in step.

use axum::handler::Handler;
use axum::routing::{self, MethodRouter};
use axum::Router;
use http::Method;

/// One native route: a `(method, path)` template the front serves itself. A
/// template segment written `{name}` matches any single segment and `{*name}`
/// swallows the tail, the same syntax axum routes on. `websocket` marks a GET
/// that only answers an upgrade, so the route table can tell it apart from a
/// plain HTTP read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRoute {
    pub method: Method,
    pub path: &'static str,
    pub websocket: bool,
}

impl NativeRoute {
    /// The method as the route table prints it: the HTTP method, or `WS` for a
    /// route that only answers a WebSocket upgrade.
    pub fn label(&self) -> String {
        if self.websocket {
            "WS".to_owned()
        } else {
            self.method.to_string()
        }
    }
}

/// A router under construction plus the table of everything registered on it.
pub struct Routes<S> {
    router: Router<S>,
    table: Vec<NativeRoute>,
}

impl<S> Default for Routes<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Routes<S>
where
    S: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            table: Vec::new(),
        }
    }

    /// Register `route` at `path` and record one table row for it. A second
    /// registration on the same path merges into the first (axum merges method
    /// routers per path), so each method is added by its own call.
    fn add(
        mut self,
        method: Method,
        path: &'static str,
        websocket: bool,
        route: MethodRouter<S>,
    ) -> Self {
        self.router = self.router.route(path, route);
        self.table.push(NativeRoute {
            method,
            path,
            websocket,
        });
        self
    }

    pub fn get<H, T>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.add(Method::GET, path, false, routing::get(handler))
    }

    pub fn post<H, T>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.add(Method::POST, path, false, routing::post(handler))
    }

    pub fn put<H, T>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.add(Method::PUT, path, false, routing::put(handler))
    }

    pub fn delete<H, T>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.add(Method::DELETE, path, false, routing::delete(handler))
    }

    pub fn patch<H, T>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.add(Method::PATCH, path, false, routing::patch(handler))
    }

    /// A GET that only answers a WebSocket upgrade. The upgrade request is a
    /// GET, which is what the auth edge matches on; the row is flagged so the
    /// route table prints it as `WS`.
    pub fn ws<H, T>(self, path: &'static str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.add(Method::GET, path, true, routing::get(handler))
    }

    /// A route that needs a layer on its method router (a body limit). `route`
    /// MUST serve exactly `method`: the row records `method`, and a router that
    /// answered another one would serve it off the table.
    pub fn method(self, path: &'static str, method: Method, route: MethodRouter<S>) -> Self {
        self.add(method, path, false, route)
    }

    /// The finished router and the table of every route registered on it.
    pub fn into_parts(self) -> (Router<S>, Vec<NativeRoute>) {
        (self.router, self.table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    async fn status(router: Router, method: Method, path: &str) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        router.oneshot(request).await.unwrap().status()
    }

    /// The builder registers each method of a path by its own `.route()` call,
    /// which relies on axum merging method routers registered on one path
    /// rather than panicking on the second registration.
    #[tokio::test]
    async fn axum_merges_methods_on_same_path() {
        let (router, table) = Routes::<()>::new()
            .get("/x", || async { StatusCode::OK })
            .post("/x", || async { StatusCode::CREATED })
            .delete("/x", || async { StatusCode::ACCEPTED })
            .into_parts();
        assert_eq!(table.len(), 3);
        assert_eq!(
            status(router.clone(), Method::GET, "/x").await,
            StatusCode::OK
        );
        assert_eq!(
            status(router.clone(), Method::POST, "/x").await,
            StatusCode::CREATED
        );
        assert_eq!(
            status(router.clone(), Method::DELETE, "/x").await,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            status(router, Method::PUT, "/x").await,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[test]
    fn a_websocket_row_is_a_get_labelled_ws() {
        let (_, table) = Routes::<()>::new()
            .ws("/stream", || async { StatusCode::OK })
            .get("/read", || async { StatusCode::OK })
            .into_parts();
        assert_eq!(table[0].method, Method::GET);
        assert!(table[0].websocket);
        assert_eq!(table[0].label(), "WS");
        assert_eq!(table[1].label(), "GET");
    }
}
