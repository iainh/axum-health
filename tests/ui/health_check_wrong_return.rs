use axum_health::{health_check, Check};

struct WrongReturn;

#[health_check]
impl WrongReturn {
    #[liveness]
    async fn live(&self) -> Check {
        Check::up()
    }
}

fn main() {}
