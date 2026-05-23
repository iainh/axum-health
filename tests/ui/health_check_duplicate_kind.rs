use axum_health::{Check, Result, health_check};

struct DuplicateKind;

#[health_check]
impl DuplicateKind {
    #[health(liveness, readiness, liveness)]
    async fn probe(&self) -> Result<Check> {
        Ok(Check::up())
    }
}

fn main() {}
