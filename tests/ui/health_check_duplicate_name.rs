use axum_health::{Check, Result, health_check};

struct DuplicateName;

#[health_check]
impl DuplicateName {
    #[readiness(name = "database", name = "db")]
    async fn ready(&self) -> Result<Check> {
        Ok(Check::up())
    }
}

fn main() {}
