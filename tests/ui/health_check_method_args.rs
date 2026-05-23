use axum_health::{Check, Result, health_check};

struct MethodArgs;

#[health_check]
impl MethodArgs {
    #[readiness]
    async fn ready(&self, timeout_ms: u64) -> Result<Check> {
        let _ = timeout_ms;
        Ok(Check::up())
    }
}

fn main() {}
