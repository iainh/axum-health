use axum_health::{Check, Result, health_check};

struct NonAsync;

#[health_check]
impl NonAsync {
    #[liveness]
    fn live(&self) -> Result<Check> {
        Ok(Check::up())
    }
}

fn main() {}
