use axum_health::{Check, Result, health_check};

struct EmptyName;

#[health_check]
impl EmptyName {
    #[startup(name = "")]
    async fn started(&self) -> Result<Check> {
        Ok(Check::up())
    }
}

fn main() {}
