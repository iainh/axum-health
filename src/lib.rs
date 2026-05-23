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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use serde_json::{Map, Value};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

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
    /// have structured diagnostics. If serialization failures should make the
    /// health check fail, use [`Self::try_with_data`] instead.
    pub fn with_data(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        let value = serde_json::to_value(value).unwrap_or_else(|error| {
            Value::String(format!("failed to serialize health data: {error}"))
        });
        self.data.insert(key.into(), value);
        self
    }

    /// Tries to add a JSON-serializable data value to this check.
    ///
    /// This is the fallible variant of [`Self::with_data`] for checks that
    /// should return an error if diagnostic data cannot be serialized.
    pub fn try_with_data(mut self, key: impl Into<String>, value: impl Serialize) -> Result<Self> {
        self.data.insert(key.into(), serde_json::to_value(value)?);
        Ok(self)
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
        let checks = run_checks_concurrently(
            self.checks
                .iter()
                .filter(|registered| kind.is_none_or(|kind| registered.kind == kind)),
        )
        .await;

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
        let name = validate_name(name);
        let check = Arc::new(move || Box::pin(check()) as CheckFuture);
        for kind in kinds {
            self.checks.push(RegisteredCheck {
                name: name.clone(),
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
            name: validate_name(name),
            kind,
            check,
        });
        self
    }
}

fn validate_name(name: impl Into<String>) -> Arc<str> {
    let name = name.into();
    assert!(!name.is_empty(), "health check names must not be empty");
    Arc::from(name)
}

struct PendingCheck {
    name: Arc<str>,
    future: Option<CheckFuture>,
    response: Option<CheckResponse>,
}

async fn run_checks_concurrently<'a>(
    checks: impl IntoIterator<Item = &'a RegisteredCheck>,
) -> Vec<CheckResponse> {
    let mut pending = checks
        .into_iter()
        .map(|registered| PendingCheck {
            name: registered.name.clone(),
            future: Some((registered.check)()),
            response: None,
        })
        .collect::<Vec<_>>();

    std::future::poll_fn(|cx| {
        let mut all_ready = true;

        for check in &mut pending {
            let Some(future) = &mut check.future else {
                continue;
            };

            match future.as_mut().poll(cx) {
                Poll::Ready(result) => {
                    check.response = Some(check_response(check.name.clone(), result));
                    check.future = None;
                }
                Poll::Pending => all_ready = false,
            }
        }

        if all_ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;

    pending
        .into_iter()
        .map(|check| {
            check.response.unwrap_or_else(|| {
                check_response(check.name, Err("health check did not complete".into()))
            })
        })
        .collect()
}

fn check_response(name: Arc<str>, result: Result<Check>) -> CheckResponse {
    match result {
        Ok(check) => CheckResponse {
            name: name.to_string(),
            status: check.status,
            data: check.data,
        },
        Err(error) => CheckResponse {
            name: name.to_string(),
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
    use axum::http::Request;
    use serde::Serialize;
    use serde::ser::{SerializeStruct, Serializer};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
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

    #[test]
    #[should_panic(expected = "health check names must not be empty")]
    fn direct_registration_rejects_empty_names() {
        let _health = Health::builder()
            .liveness("", || async { Ok(Check::up()) })
            .build();
    }

    #[test]
    fn try_with_data_returns_serialization_errors() {
        let error = Check::up()
            .try_with_data("broken", FailingData)
            .expect_err("failing serializer should return an error");

        assert_eq!(error.to_string(), "health data failed to serialize");
    }

    #[tokio::test]
    async fn selected_checks_run_concurrently() {
        let running = Arc::new(AtomicUsize::new(0));
        let max_running = Arc::new(AtomicUsize::new(0));

        let response = Health::builder()
            .readiness(
                "first",
                concurrent_check(running.clone(), max_running.clone()),
            )
            .readiness(
                "second",
                concurrent_check(running.clone(), max_running.clone()),
            )
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
                    {"name": "first", "status": "UP"},
                    {"name": "second", "status": "UP"}
                ]
            })
        );
        assert_eq!(max_running.load(Ordering::SeqCst), 2);
    }

    fn concurrent_check(
        running: Arc<AtomicUsize>,
        max_running: Arc<AtomicUsize>,
    ) -> impl Fn() -> Pin<Box<dyn Future<Output = Result<Check>> + Send>> + Send + Sync + 'static
    {
        move || {
            let running = running.clone();
            let max_running = max_running.clone();
            Box::pin(async move {
                let current = running.fetch_add(1, Ordering::SeqCst) + 1;
                max_running.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(Check::up())
            })
        }
    }

    struct FailingData;

    impl Serialize for FailingData {
        fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut data = serializer.serialize_struct("FailingData", 1)?;
            data.serialize_field("broken", &BrokenField("health data failed to serialize"))?;
            data.end()
        }
    }

    struct BrokenField(&'static str);

    impl Serialize for BrokenField {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(serde::ser::Error::custom(self.0))
        }
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
