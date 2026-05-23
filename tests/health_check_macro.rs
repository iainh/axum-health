use axum::body::{Body, to_bytes};
use axum_health::{Check, Health, Result, health_check};
use http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

#[derive(Clone)]
struct RestBackend {
    available: bool,
}

#[derive(Clone)]
struct DatabaseBackend {
    connected: bool,
}

#[derive(Clone)]
struct LdapBackend {
    reachable: bool,
}

struct RestHealth {
    backend: RestBackend,
}

#[health_check]
impl RestHealth {
    #[liveness(name = "rest-api")]
    async fn live(&self) -> Result<Check> {
        Ok(status(self.backend.available).with_data("backend", "rest"))
    }
}

struct DatabaseHealth {
    backend: DatabaseBackend,
}

#[health_check]
impl DatabaseHealth {
    #[readiness(name = "database")]
    async fn ready(&self) -> Result<Check> {
        Ok(status(self.backend.connected).with_data("backend", "database"))
    }
}

struct LdapHealth {
    backend: LdapBackend,
}

#[health_check]
impl LdapHealth {
    #[health(liveness, readiness, name = "ldap-directory")]
    async fn bind_probe(&self) -> Result<Check> {
        Ok(status(self.backend.reachable).with_data("backend", "ldap"))
    }
}

struct GenericHealth<B> {
    backend: Arc<B>,
}

#[health_check]
impl<B> GenericHealth<B>
where
    B: BackendProbe + Send + Sync + 'static,
{
    #[startup]
    async fn backend(&self) -> Result<Check> {
        Ok(status(self.backend.is_available()))
    }
}

trait BackendProbe {
    fn is_available(&self) -> bool;
}

impl BackendProbe for RestBackend {
    fn is_available(&self) -> bool {
        self.available
    }
}

#[tokio::test]
async fn macro_generates_composable_backend_health_check() {
    let health = Health::builder()
        .include(RestHealth {
            backend: RestBackend { available: true },
        })
        .include(DatabaseHealth {
            backend: DatabaseBackend { connected: false },
        })
        .include(LdapHealth {
            backend: LdapBackend { reachable: true },
        })
        .build();

    let ready = health
        .clone()
        .router()
        .oneshot(request("/health/ready"))
        .await
        .expect("health route should respond");

    assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(ready).await,
        json!({
            "status": "DOWN",
            "checks": [
                {
                    "name": "database",
                    "status": "DOWN",
                    "data": {
                        "backend": "database"
                    }
                },
                {
                    "name": "ldap-directory",
                    "status": "UP",
                    "data": {
                        "backend": "ldap"
                    }
                }
            ]
        })
    );

    let live = health
        .router()
        .oneshot(request("/health/live"))
        .await
        .expect("health route should respond");

    assert_eq!(live.status(), StatusCode::OK);
    assert_eq!(
        body_json(live).await,
        json!({
            "status": "UP",
            "checks": [
                {
                    "name": "rest-api",
                    "status": "UP",
                    "data": {
                        "backend": "rest"
                    }
                },
                {
                    "name": "ldap-directory",
                    "status": "UP",
                    "data": {
                        "backend": "ldap"
                    }
                }
            ]
        })
    );
}

#[tokio::test]
async fn macro_supports_generic_backend_wrappers() {
    let health = GenericHealth {
        backend: Arc::new(RestBackend { available: true }),
    }
    .into_health();

    let response = health
        .router()
        .oneshot(request("/health/started"))
        .await
        .expect("health route should respond");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        json!({
            "status": "UP",
            "checks": [
                {
                    "name": "backend",
                    "status": "UP"
                }
            ]
        })
    );
}

fn status(up: bool) -> Check {
    if up { Check::up() } else { Check::down() }
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
