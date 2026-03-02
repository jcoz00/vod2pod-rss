use log::{debug, info};
use simple_logger::SimpleLogger;
use std::{env, net::TcpListener, process::exit};
use vod2pod_rss::server;

fn install_rustls_provider() {
    // Choose ONE provider (aws-lc-rs is a good default)
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("install rustls crypto provider");
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Must be done early (before any TLS usage)
    install_rustls_provider();

    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("--version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        exit(0);
    }

    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .env()
        .init()
        .unwrap();

    if let Err(err) = flush_redis_on_new_version().await {
        panic!("Error interacting with Redis (redis is required): {:?}", err);
    }

    let listener = TcpListener::bind("0.0.0.0:8080").expect("Failed to bind");
    info!("listening on http://{}", listener.local_addr().unwrap());

    server::spawn_server(listener)
        .expect("could not setup server")
        .await?;

    Ok(())
}

async fn flush_redis_on_new_version() -> eyre::Result<()> {
    let app_version = env!("CARGO_PKG_VERSION");
    info!("app version {app_version}");

    let mut con = vod2pod_rss::get_redis_client().await?;

    let cached_version: Option<String> = redis::cmd("GET")
        .arg("version")
        .query_async(&mut con)
        .await?;
    debug!("cached app version {:?}", cached_version);

    if let Some(ref cached_version) = cached_version {
        if cached_version != app_version {
            info!("detected version change ({cached_version} != {app_version}) flushing redis DB");
            redis::cmd("FLUSHDB")
            .query_async::<()>(&mut con)
            .await?;
        }
    }

    redis::cmd("SET")
        .arg("version")
        .arg(app_version)
        .query_async::<()>(&mut con)
        .await?;
    debug!("set cached app version to {app_version}");

    Ok(())
}
