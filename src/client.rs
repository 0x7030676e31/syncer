use std::sync::Arc;
use std::{env, process};

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;

mod common;

pub async fn join_all_ok<T, E>(
    futures: impl IntoIterator<Item = impl std::future::Future<Output = Result<T, E>>>,
) -> Option<T> {
    let semaphore = Arc::new(Semaphore::new(32));
    let futures = futures.into_iter().map(|future| {
        let semaphore = semaphore.clone();
        async move {
            let _permit = semaphore.acquire().await;
            future.await
        }
    });

    let mut futures = futures.collect::<FuturesUnordered<_>>();
    while let Some(result) = futures.next().await {
        match result {
            Ok(value) => return Some(value),
            Err(_) => continue,
        }
    }

    None
}

#[tokio::main]
async fn main() {
    common::rust_log_init();

    let port = env::var("PORT").unwrap_or(common::DEFAULT_PORT.to_string());
    if port.parse::<u16>().is_err() {
        eprintln!("Invalid port: {}", port);
        process::exit(1);
    }
}
