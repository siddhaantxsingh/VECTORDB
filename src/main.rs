//! `vectordb serve --data DIR [--http ADDR] [--grpc ADDR] [--sync always|never]`
//! `vectordb check --data DIR`   verify every collection's invariants after recovery
use std::sync::Arc;
use vectordb::server::{grpc_service, router, Registry};
use vectordb::wal::SyncMode;

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    let data = arg(&args, "--data", "./data");
    let sync = match arg(&args, "--sync", "always").as_str() {
        "never" => SyncMode::Never,
        _ => SyncMode::Always,
    };
    match cmd {
        "serve" => {
            let reg = Registry::open(data.into(), sync)?;
            let http: std::net::SocketAddr = arg(&args, "--http", "127.0.0.1:6333").parse()?;
            let grpc: std::net::SocketAddr = arg(&args, "--grpc", "127.0.0.1:6334").parse()?;
            let g = tonic::transport::Server::builder().add_service(grpc_service(Arc::clone(&reg))).serve_with_shutdown(grpc, shutdown());
            let listener = tokio::net::TcpListener::bind(http).await?;
            tracing::info!(%http, %grpc, "listening");
            let h = axum::serve(listener, router(Arc::clone(&reg))).with_graceful_shutdown(shutdown());
            let (a, b) = tokio::join!(h, g);
            a?;
            b?;
            Ok(())
        }
        "check" => {
            let reg = Registry::open(data.into(), sync)?;
            for name in std::fs::read_dir(&reg.dir)?.flatten().map(|e| e.file_name().to_string_lossy().to_string()) {
                if let Ok(c) = reg.get(&name) {
                    match c.check() {
                        Ok(()) => println!("{name}: ok {:?}", c.stats().live),
                        Err(e) => {
                            println!("{name}: FAILED {e}");
                            std::process::exit(1);
                        }
                    }
                }
            }
            Ok(())
        }
        _ => {
            println!("usage: vectordb serve --data DIR [--http 127.0.0.1:6333] [--grpc 127.0.0.1:6334] [--sync always|never]\n       vectordb check --data DIR");
            Ok(())
        }
    }
}

/// Resolves on SIGINT or SIGTERM; servers then stop accepting and finish
/// in-flight requests. Every acknowledged write is already in the WAL.
async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
    }
}
