//! Minimal Actix Web server used to exercise RustTurbo.
//!
//! It prints `READY <url>` on stdout exactly once, after the socket is bound and the
//! server is accepting. The benchmark harness uses that line to measure *time to
//! application serving* separately from time to compile.

use actix_web::{get, middleware, web, App, HttpResponse, HttpServer, Responder};
use serde::Serialize;

#[derive(Serialize)]
struct Health {
    status: &'static str,
    service: &'static str,
    version: &'static str,
    uptime_ms: u128,
}

#[get("/")]
async fn index() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/plain; charset=utf-8")
        .body("RustTurbo demo is running. Try /health\n")
}

#[get("/health")]
async fn health(started: web::Data<std::time::Instant>) -> impl Responder {
    HttpResponse::Ok().json(Health {
        status: "ok",
        service: "actix-hello",
        version: env!("CARGO_PKG_VERSION"),
        uptime_ms: started.elapsed().as_millis(),
    })
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let requested = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(0);
    let started = web::Data::new(std::time::Instant::now());

    let server = HttpServer::new(move || {
        App::new()
            .app_data(started.clone())
            .wrap(middleware::Logger::default())
            .service(index)
            .service(health)
    })
    .bind(("127.0.0.1", requested))?;

    let addr = server.addrs().first().cloned().unwrap_or_else(|| {
        "127.0.0.1:0".parse().expect("static socket address")
    });

    // Announce readiness only after the socket is bound.
    println!("READY http://{addr}");
    server.run().await
}
