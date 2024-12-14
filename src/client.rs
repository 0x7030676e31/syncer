use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::{env, fs, future, io, process};

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time;

mod common;

async fn join_all_ok<T, E>(
    futures: impl IntoIterator<Item = impl future::Future<Output = Result<T, E>>>,
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

async fn wrap_timeout<T>(future: impl future::Future<Output = io::Result<T>>) -> io::Result<T> {
    time::timeout(common::CONNECTION_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
}

async fn verify_host(host: String) -> io::Result<(TcpStream, common::Mode)> {
    log::debug!("Connecting to server at {}", host);
    let mut socket = wrap_timeout(TcpStream::connect(&host)).await?;
    let addr = socket.peer_addr()?;

    log::debug!("Connected to server at {}", addr);

    let mut buffer = [0; common::VERIFY_MESSAGE_SERVER.len()];
    socket.read_exact(&mut buffer).await.inspect_err(|err| {
        log::debug!("Failed to read verification message from {}: {}", addr, err);
    })?;

    if buffer != common::VERIFY_MESSAGE_SERVER.as_bytes() {
        log::debug!("Invalid verification message from {}", addr);
        return Err(io::ErrorKind::InvalidData.into());
    }

    if let Err(err) = socket
        .write_all(common::VERIFY_MESSAGE_CLIENT.as_bytes())
        .await
    {
        log::debug!("Failed to write verification message to {}: {}", addr, err);
        return Err(err);
    }

    let mode = match socket.read_u8().await {
        Ok(mode) => mode,
        Err(err) => {
            log::debug!("Failed to read mode from {}: {}", addr, err);
            return Err(err);
        }
    };

    let mode = match mode.try_into() {
        Ok(mode) => mode,
        Err(_) => {
            log::debug!("Invalid mode from {}", addr);
            return Err(io::ErrorKind::InvalidData.into());
        }
    };

    Ok((socket, mode))
}

fn get_fs_root() -> io::Result<PathBuf> {
    if !cfg!(windows) {
        return Ok(PathBuf::from("/"));
    }

    let current_exe = env::current_exe()?;
    Ok(current_exe.components().take(2).collect::<PathBuf>())
}

#[tokio::main]
async fn main() {
    common::rust_log_init();

    let port = env::var("PORT").unwrap_or(common::DEFAULT_PORT.to_string());
    if port.parse::<u16>().is_err() {
        eprintln!("Invalid port: {}", port);
        process::exit(1);
    }

    let interface = match netdev::get_default_interface() {
        Ok(interface) => interface,
        Err(err) => {
            log::error!("Failed to get default interface: {}", err);
            process::exit(1);
        }
    };

    if interface.ipv4.is_empty() {
        log::error!("No IPv4 address found on default interface");
        process::exit(1);
    }

    let mut futures = Vec::new();
    for ipv4 in interface.ipv4 {
        let subnets = 2u32.pow((ipv4.max_prefix_len() - ipv4.prefix_len()) as u32);
        let mask = (subnets - 1) ^ u32::MAX;
        let addr = ipv4.addr().to_bits() & mask;

        for i in 0..subnets {
            let subnet = addr | i;
            let octets = subnet.to_be_bytes();

            let host = format!(
                "{}.{}.{}.{}:{}",
                octets[0], octets[1], octets[2], octets[3], port
            );

            futures.push(verify_host(host));
        }
    }

    let (socket, mode) = match join_all_ok(futures).await {
        Some(socket) => socket,
        None => {
            log::error!("Failed to connect to any host");
            process::exit(1);
        }
    };

    let addr = match socket.peer_addr() {
        Ok(addr) => addr,
        Err(err) => {
            log::error!("Failed to get peer address: {}", err);
            process::exit(1);
        }
    };

    log::info!("Connected to server at {} in {} mode", addr, mode);
    let result = match mode {
        common::Mode::Scan => handle_mode0_scan(socket, addr).await,
        common::Mode::Fetch => handle_mode1_fetch(socket, addr).await,
    };

    match result {
        Ok(_) => log::info!("Job finished, closing connection with {}", addr),
        Err(_) => log::error!("Connection with {} unexpectedly closed", addr),
    }
}

async fn handle_mode0_scan(mut socket: TcpStream, addr: SocketAddr) -> io::Result<()> {
    let mut files: HashMap<String, (u64, u64)> = HashMap::new();
    let mut queue = VecDeque::new();

    let root = match get_fs_root() {
        Ok(root) => root,
        Err(err) => {
            log::error!("Failed to get filesystem root: {}", err);
            return Err(err);
        }
    };

    queue.push_back(root);
    while let Some(path) = queue.pop_front() {
        let mut entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(err) => {
                log::error!("Failed to read directory {}: {}", path.display(), err);
                continue;
            }
        };

        while let Some(entry) = entries.next() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    log::error!("Failed to read directory entry: {}", err);
                    continue;
                }
            };

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) => {
                    log::error!(
                        "Failed to read metadata for {}: {}",
                        entry.path().display(),
                        err
                    );
                    continue;
                }
            };

            if metadata.is_symlink() {
                continue;
            }

            let path = entry.path();
            if metadata.is_dir() {
                queue.push_back(path);
                continue;
            }

            let ext = match path.extension() {
                Some(ext) => ext,
                None => continue,
            };

            let ext = match ext.to_str() {
                Some(ext) => ext,
                None => {
                    log::error!("Failed to convert extension to string");
                    continue;
                }
            };

            if !common::IMAGE_EXT.contains(&ext) && !common::VIDEO_EXT.contains(&ext) {
                continue;
            }

            let entry = files.entry(ext.to_string()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += metadata.len();
        }
    }

    for (ext, (count, size)) in files {
        let ext_len = ext.len() as u8;
        let ext = ext.as_bytes();

        socket.write_u8(ext_len).await?;
        socket.write_all(ext).await?;
        socket.write_u64(count).await?;
        socket.write_u64(size).await?;
    }

    match cleanup(socket, addr).await {
        Ok(_) => Ok(()),
        Err(err) => {
            log::error!("Failed to cleanup connection with {}: {}", addr, err);
            Err(err)
        }
    }
}

async fn handle_mode1_fetch(mut socket: TcpStream, addr: SocketAddr) -> io::Result<()> {
    println!("Fetching from {}", addr);

    match cleanup(socket, addr).await {
        Ok(_) => Ok(()),
        Err(err) => {
            log::error!("Failed to cleanup connection with {}: {}", addr, err);
            Err(err)
        }
    }
}

async fn cleanup(mut socket: TcpStream, addr: SocketAddr) -> io::Result<()> {
    socket.write_u8(0).await?;
    log::debug!("Connection with {} closed", addr);
    Ok(())
}
