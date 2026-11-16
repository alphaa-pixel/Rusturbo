//! A deliberately dependency-heavy Actix Web service.
//!
//! Nothing here talks to a database at startup — sqlx is compiled (which is the point of
//! the benchmark: a large, realistic dependency graph) but no connection is attempted,
//! so the binary starts on a machine with no Postgres.

use actix_web::{get, middleware, web, App, HttpResponse, HttpServer, Responder};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Serialize)]
struct Health {
    status: &'static str,
    service: &'static str,
    version: &'static str,
    uptime_ms: u128,
    requests: u64,
    deps_in_graph: usize,
}

static REQUESTS: AtomicU64 = AtomicU64::new(0);

#[get("/")]
async fn index() -> impl Responder {
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    HttpResponse::Ok().body("actix-large: RustTurbo large-graph demo\n")
}

#[get("/health")]
async fn health(started: web::Data<Instant>) -> impl Responder {
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    HttpResponse::Ok().json(Health {
        status: "ok",
        service: "actix-large",
        version: env!("CARGO_PKG_VERSION"),
        uptime_ms: started.elapsed().as_millis(),
        requests: REQUESTS.load(Ordering::Relaxed),
        deps_in_graph: 0,
    })
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let requested = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(0);
    let started = web::Data::new(Instant::now());

    let server = HttpServer::new(move || {
        App::new()
            .app_data(started.clone())
            .wrap(middleware::Logger::default())
            .service(index)
            .service(health)
    })
    .bind(("127.0.0.1", requested))?;

    let addr = server
        .addrs()
        .first()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:0".parse().expect("static socket address"));

    println!("READY http://{addr}");
    server.run().await
}
