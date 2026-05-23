use axum_health::{Check, Result, health_check};

trait Probe {
    async fn live(&self) -> Result<Check>;
}

struct TraitImpl;

#[health_check]
impl Probe for TraitImpl {
    #[liveness]
    async fn live(&self) -> Result<Check> {
        Ok(Check::up())
    }
}

fn main() {}
