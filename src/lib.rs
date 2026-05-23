//! MicroProfile-inspired health endpoints for Axum.
//!
//! `axum-health` keeps the useful parts of MicroProfile Health: named checks,
//! `UP`/`DOWN` status values, `/health`, `/health/live`, `/health/ready`, and
//! `/health/started` endpoints. It adapts the API to Rust by registering async
//! closures with a [`Health`] registry instead of using Java annotations.
//!
//! ```
//! use axum::Router;
//! use axum_health::{Check, Health};
//!
//! let health = Health::builder()
//!     .liveness("process", || async { Ok(Check::up()) })
//!     .readiness("database", || async {
//!         Ok(Check::up().with_data("pool", "available"))
//!     })
//!     .build();
//!
//! let app: Router = Router::new().merge(health.router());
//! ```
//!
//! # Composing backend health checks
//!
//! Use [`health_check`] when checks naturally live on a struct that owns
//! backend clients such as REST clients, database pools, or LDAP connections.
//!
//! ```
//! use axum_health::{Check, Health, Result, health_check};
//!
//! struct DatabaseHealth;
//!
//! #[health_check]
//! impl DatabaseHealth {
//!     #[readiness(name = "database")]
//!     async fn ready(&self) -> Result<Check> {
//!         Ok(Check::up())
//!     }
//! }
//!
//! let health = Health::builder().include(DatabaseHealth).build();
//! ```

pub use axum_health_macros::health_check;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::StatusCode;
use serde::Serialize;
use serde_json::{Map, Value};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Error type returned by health check closures.
pub type Error = Box<dyn StdError + Send + Sync>;

/// Result type returned by health check closures.
pub type Result<T> = std::result::Result<T, Error>;

type CheckFuture = Pin<Box<dyn Future<Output = Result<Check>> + Send>>;
type CheckFn = Arc<dyn Fn() -> CheckFuture + Send + Sync>;

/// MicroProfile health status value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    /// The application or component is healthy.
    Up,
    /// The application or component is unhealthy.
    Down,
}

impl Status {
    fn http_status(self) -> StatusCode {
        match self {
            Self::Up => StatusCode::OK,
            Self::Down => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// A single health check result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    status: Status,
    #[serde(skip_serializing_if = "Map::is_empty")]
    data: Map<String, Value>,
}

impl Check {
    /// Creates an `UP` check result.
    pub fn up() -> Self {
        Self {
            status: Status::Up,
            data: Map::new(),
        }
    }

    /// Creates a `DOWN` check result.
    pub fn down() -> Self {
        Self {
            status: Status::Down,
            data: Map::new(),
        }
    }

    /// Adds a JSON-serializable data value to this check.
    ///
    /// Data values should be small and safe to expose to unauthenticated probe
    /// consumers. MicroProfile Health allows string, boolean, and number values;
    /// this method also accepts nested JSON because Rust callers often already
    /// have structured diagnostics.
    pub fn with_data(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        let value = serde_json::to_value(value).unwrap_or_else(|error| {
            Value::String(format!("failed to serialize health data: {error}"))
        });
        self.data.insert(key.into(), value);
        self
    }
}

/// The health check kind associated with an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Liveness checks are exposed at `/health/live`.
    Liveness,
    /// Readiness checks are exposed at `/health/ready`.
    Readiness,
    /// Startup checks are exposed at `/health/started`.
    Startup,
}

impl Kind {
    fn path(self) -> &'static str {
        match self {
            Self::Liveness => "/live",
            Self::Readiness => "/ready",
            Self::Startup => "/started",
        }
    }
}

#[derive(Clone)]
struct RegisteredCheck {
    name: Arc<str>,
    kind: Kind,
    check: CheckFn,
}

/// A cloneable registry of health checks.
#[derive(Clone, Default)]
pub struct Health {
    checks: Arc<[RegisteredCheck]>,
}

impl Health {
    /// Starts building a health registry.
    pub fn builder() -> HealthBuilder {
        HealthBuilder::default()
    }

    /// Returns an Axum router with `/health`, `/health/live`, `/health/ready`,
    /// and `/health/started` routes.
    pub fn router(self) -> axum::Router {
        axum::Router::new()
            .route("/health", get(all))
            .nest(
                "/health",
                axum::Router::new()
                    .route(Kind::Liveness.path(), get(liveness))
                    .route(Kind::Readiness.path(), get(readiness))
                    .route(Kind::Startup.path(), get(startup)),
            )
            .with_state(self)
    }

    async fn run(&self, kind: Option<Kind>) -> HealthPayload {
        let mut checks = Vec::new();

        for registered in self.checks.iter() {
            if kind.is_some_and(|kind| registered.kind != kind) {
                continue;
            }

            checks.push(run_check(registered).await);
        }

        let status = if checks.iter().all(|check| check.status == Status::Up) {
            Status::Up
        } else {
            Status::Down
        };

        HealthPayload { status, checks }
    }
}

/// Builder for [`Health`].
#[derive(Default)]
pub struct HealthBuilder {
    checks: Vec<RegisteredCheck>,
}

/// A provider that can register one or more health check procedures.
///
/// This is implemented by the [`health_check`] macro for backend-specific
/// types. A single provider can contribute liveness, readiness, startup, or
/// multiple kinds of checks.
pub trait HealthCheck: Send + Sync + 'static {
    /// Registers this provider's checks with the supplied builder.
    fn register(self, builder: HealthBuilder) -> HealthBuilder
    where
        Self: Sized;
}

impl HealthBuilder {
    /// Includes a backend-specific health check provider.
    pub fn include<C>(self, check: C) -> Self
    where
        C: HealthCheck,
    {
        check.register(self)
    }

    /// Registers a liveness check.
    pub fn liveness<F, Fut>(self, name: impl Into<String>, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Check>> + Send + 'static,
    {
        self.check(Kind::Liveness, name, check)
    }

    /// Registers a readiness check.
    pub fn readiness<F, Fut>(self, name: impl Into<String>, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Check>> + Send + 'static,
    {
        self.check(Kind::Readiness, name, check)
    }

    /// Registers a startup check.
    pub fn startup<F, Fut>(self, name: impl Into<String>, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Check>> + Send + 'static,
    {
        self.check(Kind::Startup, name, check)
    }

    /// Registers the same check for more than one health kind.
    pub fn check_for<F, Fut>(
        mut self,
        kinds: impl IntoIterator<Item = Kind>,
        name: impl Into<String>,
        check: F,
    ) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Check>> + Send + 'static,
    {
        let name = name.into();
        let check = Arc::new(move || Box::pin(check()) as CheckFuture);
        for kind in kinds {
            self.checks.push(RegisteredCheck {
                name: Arc::from(name.as_str()),
                kind,
                check: check.clone(),
            });
        }
        self
    }

    /// Finishes the registry.
    pub fn build(self) -> Health {
        Health {
            checks: Arc::from(self.checks),
        }
    }

    fn check<F, Fut>(mut self, kind: Kind, name: impl Into<String>, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Check>> + Send + 'static,
    {
        let check = Arc::new(move || Box::pin(check()) as CheckFuture);
        self.checks.push(RegisteredCheck {
            name: Arc::from(name.into()),
            kind,
            check,
        });
        self
    }
}

async fn run_check(registered: &RegisteredCheck) -> CheckResponse {
    match (registered.check)().await {
        Ok(check) => CheckResponse {
            name: registered.name.to_string(),
            status: check.status,
            data: check.data,
        },
        Err(error) => CheckResponse {
            name: registered.name.to_string(),
            status: Status::Down,
            data: Map::from_iter([("error".to_owned(), Value::String(error.to_string()))]),
        },
    }
}

async fn all(State(health): State<Health>) -> Response {
    health_response(health.run(None).await)
}

async fn liveness(State(health): State<Health>) -> Response {
    health_response(health.run(Some(Kind::Liveness)).await)
}

async fn readiness(State(health): State<Health>) -> Response {
    health_response(health.run(Some(Kind::Readiness)).await)
}

async fn startup(State(health): State<Health>) -> Response {
    health_response(health.run(Some(Kind::Startup)).await)
}

fn health_response(payload: HealthPayload) -> Response {
    (payload.status.http_status(), Json(payload)).into_response()
}

#[derive(Debug, PartialEq, Serialize)]
struct HealthPayload {
    status: Status,
    checks: Vec<CheckResponse>,
}

#[derive(Debug, PartialEq, Serialize)]
struct CheckResponse {
    name: String,
    status: Status,
    #[serde(skip_serializing_if = "Map::is_empty")]
    data: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    #[tokio::test]
    async fn empty_health_is_up() {
        let response = Health::builder()
            .build()
            .router()
            .oneshot(request("/health/ready"))
            .await
            .expect("health route should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({
                "status": "UP",
                "checks": []
            })
        );
    }

    #[tokio::test]
    async fn readiness_runs_only_readiness_checks() {
        let response = Health::builder()
            .liveness("process", || async { Ok(Check::down()) })
            .readiness("database", || async {
                Ok(Check::up().with_data("pool", "available"))
            })
            .build()
            .router()
            .oneshot(request("/health/ready"))
            .await
            .expect("health route should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({
                "status": "UP",
                "checks": [
                    {
                        "name": "database",
                        "status": "UP",
                        "data": {
                            "pool": "available"
                        }
                    }
                ]
            })
        );
    }

    #[tokio::test]
    async fn aggregate_health_runs_all_kinds() {
        let response = Health::builder()
            .liveness("process", || async { Ok(Check::up()) })
            .readiness("database", || async { Ok(Check::down()) })
            .startup("migrations", || async { Ok(Check::up()) })
            .build()
            .router()
            .oneshot(request("/health"))
            .await
            .expect("health route should respond");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(response).await,
            json!({
                "status": "DOWN",
                "checks": [
                    {"name": "process", "status": "UP"},
                    {"name": "database", "status": "DOWN"},
                    {"name": "migrations", "status": "UP"}
                ]
            })
        );
    }

    #[tokio::test]
    async fn failed_check_becomes_down_response() {
        let response = Health::builder()
            .startup("bootstrap", || async {
                Err::<Check, _>("connection pool unavailable".into())
            })
            .build()
            .router()
            .oneshot(request("/health/started"))
            .await
            .expect("health route should respond");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(response).await,
            json!({
                "status": "DOWN",
                "checks": [
                    {
                        "name": "bootstrap",
                        "status": "DOWN",
                        "data": {
                            "error": "connection pool unavailable"
                        }
                    }
                ]
            })
        );
    }

    #[tokio::test]
    async fn same_check_can_apply_to_multiple_kinds() {
        let response = Health::builder()
            .check_for([Kind::Liveness, Kind::Readiness], "shared", || async {
                Ok(Check::up())
            })
            .build()
            .router()
            .oneshot(request("/health"))
            .await
            .expect("health route should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({
                "status": "UP",
                "checks": [
                    {"name": "shared", "status": "UP"},
                    {"name": "shared", "status": "UP"}
                ]
            })
        );
    }

    fn request(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request should be valid")
    }

    async fn body_json(response: axum::response::Response) -> Value {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        serde_json::from_slice(&body).expect("response body should be JSON")
    }
}
