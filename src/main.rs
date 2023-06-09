use std::net::SocketAddr;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::EnvFilter;
use warp::Filter;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::DEBUG)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let addr: SocketAddr = ([127, 0, 0, 1], 3000).into();
    let routes = warp::get()
        .and(warp::path::full())
        .and(rdir::conditionals())
        .and_then(rdir::reply)
        .with(warp::log::log("rdir"))
        .recover(rdir::error_recover);

    warp::serve(routes).run(addr).await;
    Ok(())
}
