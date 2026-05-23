use axum_health::{Check, Result, health_check};

struct MutSelf;

#[health_check]
impl MutSelf {
    #[startup]
    async fn started(&mut self) -> Result<Check> {
        Ok(Check::up())
    }
}

fn main() {}
