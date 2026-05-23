use axum_health::{health_check, Check};

struct MissingReturn;

#[health_check]
impl MissingReturn {
    #[readiness]
    async fn ready(&self) {
        let _ = Check::up();
    }
}

fn main() {}
