# axum-health

`axum-health` provides Kubernetes-friendly health endpoints for Axum using the
protocol shape from Eclipse MicroProfile Health. It keeps the wire format small
and predictable: named checks, `UP`/`DOWN` statuses, JSON responses, and HTTP
200 or 503 status codes.

## High-level features

- Register liveness, readiness and startup checks with async closures.
- Expose the standard health endpoints:

  - `GET /health`
  - `GET /health/live`
  - `GET /health/ready`
  - `GET /health/started`

- Compose backend-specific health providers with `#[health_check]`.
- Attach small diagnostic values to individual check responses.
- Convert check errors into named `DOWN` responses.

## Example

```rust
use axum::Router;
use axum_health::{Check, Health};

let health = Health::builder()
    .liveness("process", || async { Ok(Check::up()) })
    .readiness("database", || async {
        Ok(Check::up().with_data("pool", "available"))
    })
    .startup("migrations", || async { Ok(Check::up()) })
    .build();

let app: Router = Router::new().merge(health.router());
```

Use `router_at` when an application needs a different mount path:

```rust
let app: Router = Router::new().merge(health.router_at("/internal/health"));
```

## Composing backend health checks

When checks naturally belong to backend-specific types, use `#[health_check]`
on each inherent `impl` block and compose them with `include`:

```rust
use axum_health::{Check, Health, Result, health_check};

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

struct LdapHealth {
    client: LdapClient,
}

#[health_check]
impl LdapHealth {
    #[health(liveness, readiness, name = "ldap-directory")]
    async fn bind_probe(&self) -> Result<Check> {
        self.client.bind_probe().await?;
        Ok(Check::up())
    }
}

let health = Health::builder()
    .include(RestHealth { client: rest })
    .include(DatabaseHealth { pool: database })
    .include(LdapHealth { client: ldap })
    .build();
```

## Endpoint behaviour

`Health::router` returns an Axum `Router` with four `GET` routes:

- `/health` runs all registered checks.
- `/health/live` runs liveness checks.
- `/health/ready` runs readiness checks.
- `/health/started` runs startup checks.

`Health::router_at("/internal/health")` exposes the same endpoint layout under
the supplied mount path.

The aggregate status is `UP` only when every selected check is `UP`. A healthy
response uses HTTP 200. If any selected check is `DOWN`, the endpoint returns
HTTP 503.

## MicroProfile mapping

MicroProfile Health describes Java `HealthCheck` implementations annotated with
`@Liveness`, `@Readiness`, or `@Startup`. In this crate those annotations become
registration methods, with an optional macro layer for colocating checks on
stateful backend-owning types:

- `Health::builder().liveness("name", check)`
- `Health::builder().readiness("name", check)`
- `Health::builder().startup("name", check)`
- `Health::builder().check_for([Kind::Liveness, Kind::Readiness], "name", check)`
- `Health::builder().include(DatabaseHealth { pool }).build()`
- `#[health_check] impl DatabaseHealth { ... }`

Each check returns `Result<Check>`. `Ok(Check::up())` and `Ok(Check::down())`
become normal check responses. `Err(_)` is converted into a named `DOWN` check
with an `error` data value.

## Wire format

Successful aggregate health returns HTTP 200:

```json
{
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
}
```

If any selected check is down, the endpoint returns HTTP 503 and an aggregate
`DOWN` status.
