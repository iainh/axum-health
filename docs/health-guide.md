# Axum Health Guide

This guide shows how to add MicroProfile-inspired health endpoints to an Axum
application with `axum-health`.

Health endpoints let external systems determine whether an application process
is alive, ready to receive traffic, or finished with startup work. This is
especially useful in Kubernetes and other environments where probes drive
restart and load-balancing decisions.

## Prerequisites

To complete this guide, you need:

- Rust 1.85 or later
- An Axum application
- A Tokio runtime
- Roughly 15 minutes

## Architecture

In this guide, we build an Axum application that exposes four `GET` endpoints:

- `/health` runs all registered checks.
- `/health/live` runs liveness checks.
- `/health/ready` runs readiness checks.
- `/health/started` runs startup checks.

Each endpoint returns JSON with an aggregate `status` and a `checks` array. The
aggregate status is `UP` only when every selected check is `UP`. Healthy
responses use HTTP 200, and unhealthy responses use HTTP 503.

## Add axum-health

Add `axum-health` to your application:

```toml
[dependencies]
axum-health = "0.1"
```

You also need `axum` and `tokio` if your project does not already depend on
them:

```toml
[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Create a Health Registry

Start by creating a registry with a simple liveness check:

```rust
use axum::Router;
use axum_health::{Check, Health};

let health = Health::builder()
    .liveness("process", || async { Ok(Check::up()) })
    .build();

let app: Router = Router::new().merge(health.router());
```

`Health::router()` mounts the health endpoints at `/health`. If your
application already uses that path, mount the same endpoint layout somewhere
else:

```rust
let app: Router = Router::new().merge(health.router_at("/internal/health"));
```

With this mount path, the aggregate endpoint is `/internal/health` and the
kind-specific endpoints are `/internal/health/live`, `/internal/health/ready`,
and `/internal/health/started`.

## Run the Liveness Check

Start your Axum application and call the liveness endpoint:

```sh
curl -i http://localhost:3000/health/live
```

The response has an aggregate status and one named check:

```json
{
  "status": "UP",
  "checks": [
    {
      "name": "process",
      "status": "UP"
    }
  ]
}
```

A liveness check should answer whether the process should continue running. If
it returns `DOWN`, an orchestrator will usually restart the instance.

## Add a Readiness Check

Readiness checks answer a different question: can this instance receive traffic
right now?

Use readiness for dependencies such as databases, queues, caches, and external
APIs. A failed readiness check usually removes an instance from service without
restarting it.

```rust
use axum_health::{Check, Health};

let health = Health::builder()
    .liveness("process", || async { Ok(Check::up()) })
    .readiness("database", || async {
        Ok(Check::up().with_data("pool", "available"))
    })
    .build();
```

Call the readiness endpoint:

```sh
curl -i http://localhost:3000/health/ready
```

Only readiness checks run on `/health/ready`. The liveness check still appears
on `/health/live`, and both checks appear on `/health`.

## Add a Startup Check

Startup checks are useful when application initialization can take longer than
the normal liveness budget. For example, an application might need to run
migrations, warm a cache, or load a model before it is safe to start liveness
probing.

```rust
use axum_health::{Check, Health};

let health = Health::builder()
    .startup("migrations", || async { Ok(Check::up()) })
    .build();
```

Startup checks are exposed at:

```sh
curl -i http://localhost:3000/health/started
```

## Return a Negative Health Result

A check can return `Check::down()` when the application can make a decision
locally:

```rust
use axum_health::{Check, Health};

let database_connected = false;

let health = Health::builder()
    .readiness("database", move || async move {
        if database_connected {
            Ok(Check::up())
        } else {
            Ok(Check::down().with_data("reason", "connection unavailable"))
        }
    })
    .build();
```

The readiness endpoint now returns HTTP 503:

```json
{
  "status": "DOWN",
  "checks": [
    {
      "name": "database",
      "status": "DOWN",
      "data": {
        "reason": "connection unavailable"
      }
    }
  ]
}
```

Return `DOWN` for expected unhealthy states. Return an error when the check
itself fails unexpectedly. `axum-health` converts errors into named `DOWN`
responses with an `error` data field:

```rust
use axum_health::{Check, Health, Result};

async fn verify_database() -> Result<()> {
    // Ping the database here.
    Ok(())
}

let health = Health::builder()
    .readiness("database", || async {
        verify_database().await?;
        Ok(Check::up())
    })
    .build();
```

## Add Diagnostic Data

Use `with_data` for small values that help operators understand the result:

```rust
use axum_health::Check;

let check = Check::up()
    .with_data("pool", "available")
    .with_data("open_connections", 4)
    .with_data("replica", true);
```

Data should be safe to expose wherever your health endpoints are reachable. Do
not include secrets, credentials, connection strings, personally identifiable
information, or large payloads.

If serialization failures should make the health check fail, use
`try_with_data`:

```rust
use axum_health::{Check, Result};

fn database_check() -> Result<Check> {
    Check::up().try_with_data("open_connections", 4)
}
```

## Register One Check for Multiple Kinds

Sometimes the same probe is meaningful for more than one health kind. Use
`check_for` with explicit kinds:

```rust
use axum_health::{Check, Health, Kind};

let health = Health::builder()
    .check_for([Kind::Liveness, Kind::Readiness], "ldap-directory", || async {
        Ok(Check::up())
    })
    .build();
```

This check appears in both `/health/live` and `/health/ready`.

## Compose Backend Health Providers

For larger applications, health logic often belongs beside backend clients. Use
`#[health_check]` on inherent `impl` blocks to turn those methods into
registrable health providers.

```rust
use axum_health::{Check, Health, Result, health_check};

struct DatabaseHealth {
    pool: DatabasePool,
}

#[health_check]
impl DatabaseHealth {
    #[readiness(name = "database")]
    async fn ready(&self) -> Result<Check> {
        self.pool.acquire().await?;
        Ok(Check::up())
    }
}

struct RestHealth {
    client: RestClient,
}

#[health_check]
impl RestHealth {
    #[liveness(name = "rest-api")]
    async fn live(&self) -> Result<Check> {
        self.client.ping().await?;
        Ok(Check::up())
    }
}

let health = Health::builder()
    .include(DatabaseHealth { pool })
    .include(RestHealth { client })
    .build();
```

The macro supports these method attributes:

- `#[liveness]`
- `#[readiness]`
- `#[startup]`
- `#[health(liveness, readiness, name = "shared-name")]`

Annotated methods must be `async`, take `&self`, take no other arguments, and
return `axum_health::Result<axum_health::Check>`.

The generated provider also has an `into_health` helper when a single backend
owns the whole registry:

```rust
let health = DatabaseHealth { pool }.into_health();
```

## Kubernetes Probes

A typical Kubernetes configuration maps each endpoint to the matching probe:

```yaml
livenessProbe:
  httpGet:
    path: /health/live
    port: 3000
readinessProbe:
  httpGet:
    path: /health/ready
    port: 3000
startupProbe:
  httpGet:
    path: /health/started
    port: 3000
```

Use liveness sparingly. It should detect states that require a restart, not
temporary dependency failures. Dependency checks usually belong in readiness so
traffic can stop while the process remains available to recover.

## Complete Example

```rust
use axum::Router;
use axum_health::{Check, Health, Result, health_check};

struct DatabaseHealth {
    pool: DatabasePool,
}

#[health_check]
impl DatabaseHealth {
    #[readiness(name = "database")]
    async fn ready(&self) -> Result<Check> {
        self.pool.acquire().await?;
        Ok(Check::up().with_data("pool", "available"))
    }
}

struct RestHealth {
    client: RestClient,
}

#[health_check]
impl RestHealth {
    #[liveness(name = "rest-api")]
    async fn live(&self) -> Result<Check> {
        self.client.ping().await?;
        Ok(Check::up())
    }
}

fn app(pool: DatabasePool, client: RestClient) -> Router {
    let health = Health::builder()
        .startup("boot", || async { Ok(Check::up()) })
        .include(DatabaseHealth { pool })
        .include(RestHealth { client })
        .build();

    Router::new().merge(health.router())
}
```

## Conclusion

`axum-health` provides a small health-check layer for Axum applications:

- Use liveness checks for restart decisions.
- Use readiness checks for traffic decisions.
- Use startup checks for slow initialization.
- Attach concise diagnostic data when it helps operators.
- Compose backend-owned checks with `#[health_check]` as the application grows.

