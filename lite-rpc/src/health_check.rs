use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use crate::rpc_tester::RPC_RESPONDING;

// Define the health check handler
async fn health_check() -> impl Responder {
    let health_status = RPC_RESPONDING.get();
    if health_status > 0.0 {
        HttpResponse::Ok().body("Service is healthy")
    } else {
        HttpResponse::ServiceUnavailable().body("Service is unhealthy")
    }
}

pub async fn start_health_service(addr: &str) -> std::io::Result<()> {
    HttpServer::new(|| {
        App::new().route("/health", web::get().to(health_check))
    })
    .bind(addr)?
    .run()
    .await
}
