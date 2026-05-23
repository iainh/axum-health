use axum_health::{Check, Result, health_check};

struct NoKind;

#[health_check]
impl NoKind {
    #[health]
    async fn probe(&self) -> Result<Check> {
        Ok(Check::up())
    }
}

fn main() {}
