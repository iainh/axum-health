# axum-health

`axum-health` is a Rust/Axum reimagining of Eclipse MicroProfile Health. It
keeps the protocol-level ideas: named health checks, `UP`/`DOWN` statuses, JSON
payloads, and the standard health endpoints:

- `GET /health`
- `GET /health/live`
- `GET /health/ready`
- `GET /health/started`

The Rust API is explicit instead of annotation-based. Checks are async closures
registered with a small builder, and the result is an Axum `Router` that can be
merged into an application.

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

## Design

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

## Wire Format

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
