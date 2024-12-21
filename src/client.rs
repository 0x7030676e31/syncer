use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek};
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

// TODO: Make it more configurable in the future
const IMAGE_EXT: [&str; 6] = ["jpg", "jpeg", "png", "gif", "bmp", "webp"];
const VIDEO_EXT: [&str; 8] = ["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v"];

const DEFAULT_SCAN_LIMIT: usize = 32;

async fn join_n_ok<T, E>(
    futures: impl IntoIterator<Item = impl future::Future<Output = Result<T, E>>>,
    n: usize,
) -> Option<T> {
    let semaphore = Arc::new(Semaphore::new(n));
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

async fn join_all_ok<T, E>(
    futures: impl IntoIterator<Item = impl future::Future<Output = Result<T, E>>>,
) -> Option<T> {
    let mut futures = futures.into_iter().collect::<FuturesUnordered<_>>();

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

    let mode_numeric = match socket.read_u8().await {
        Ok(mode) => mode,
        Err(err) => {
            log::debug!("Failed to read mode from {}: {}", addr, err);
            return Err(err);
        }
    };

    let mode: common::Mode = match mode_numeric.try_into() {
        Ok(mode) => mode,
        Err(_) => {
            log::debug!("Invalid mode from {}", addr);
            return Err(io::ErrorKind::InvalidData.into());
        }
    };

    if cfg!(windows) {
        socket.write_u8(common::WINDOWS_ID).await?;
    } else {
        socket.write_u8(common::LINUX_ID).await?;
    }

    let version_len = match socket.read_u8().await {
        Ok(version_len) => version_len as usize,
        Err(err) => {
            log::debug!("Failed to read version length from {}: {}", addr, err);
            return Err(err);
        }
    };

    let mut version = vec![0; version_len];
    socket.read_exact(&mut version).await.inspect_err(|err| {
        log::debug!("Failed to read version from {}: {}", addr, err);
    })?;

    let version = match String::from_utf8(version) {
        Ok(version) => version,
        Err(err) => {
            log::debug!("Failed to convert version to string from {}: {}", addr, err);
            return Err(io::ErrorKind::InvalidData.into());
        }
    };

    log::debug!("Server at {} is running version {}", addr, version);
    if env!("CARGO_PKG_VERSION") != version {
        log::error!(
            "Version mismatch ({}): client version {} != server version {}",
            addr,
            env!("CARGO_PKG_VERSION"),
            version
        );
    }

    socket.write_u8(1).await.inspect_err(|err| {
        log::debug!("Failed to write ack to {}: {}", addr, err);
    })?;

    log::debug!(
        "Connection with {} verified: mode {} ({}), version {}",
        addr,
        mode.to_string(),
        mode_numeric,
        version
    );
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

    let scan_limit = match env::var("SCAN_LIMIT") {
        Ok(scan_limit) => scan_limit,
        Err(_) => DEFAULT_SCAN_LIMIT.to_string(),
    };

    let scan_limit = match scan_limit.parse::<usize>() {
        Ok(scan_limit) => scan_limit,
        Err(_) => {
            eprintln!("Invalid scan limit: {}", scan_limit);
            process::exit(1);
        }
    };

    let interface = match netdev::get_default_interface() {
        Ok(interface) => interface,
        Err(err) => {
            log::error!("Failed to get default interface: {}", err);
            process::exit(1);
        }
    };

    log::debug!("Default interface: {:?}", interface);
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

    log::debug!("Scanning {} hosts", futures.len());
    let scan_result = if futures.len() <= scan_limit || scan_limit == 0 {
        join_all_ok(futures).await
    } else {
        join_n_ok(futures, scan_limit).await
    };

    let (socket, mode) = match scan_result {
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
    let mut extensions: HashMap<String, (u64, u64)> = HashMap::new();
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
        log::debug!("Scanning directory {}", path.display());
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(err) => {
                log::warn!("Failed to read directory {}: {}", path.display(), err);
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    log::warn!("Failed to read directory entry: {}", err);
                    continue;
                }
            };

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) => {
                    log::warn!(
                        "Failed to read metadata for {}: {}",
                        entry.path().display(),
                        err
                    );
                    continue;
                }
            };

            if metadata.is_symlink() {
                log::debug!("Skipping symlink {}", entry.path().display());
                continue;
            }

            let path = entry.path();
            if metadata.is_dir() {
                log::debug!("Adding directory {} to queue", path.display());
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

            if !IMAGE_EXT.contains(&ext) && !VIDEO_EXT.contains(&ext) {
                log::debug!("Skipping non-image/video file {}", path.display());
                continue;
            }

            log::debug!("Found file {}", path.display());
            let entry = extensions.entry(ext.to_string()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += metadata.len();
        }
    }

    log::debug!("Sending scan results to {}", addr);
    for (ext, (count, size)) in extensions {
        let ext_len = ext.len() as u8;
        let ext = ext.as_bytes();

        log::debug!(
            "Sending extension {:?} ({} files, {} bytes)",
            ext,
            count,
            size
        );
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

async fn prehash_check(
    socket: &mut TcpStream,
    file: &mut fs::File,
    addr: SocketAddr,
) -> io::Result<bool> {
    log::debug!("Reading whether file exists from {}", addr);
    let exists = match socket.read_u8().await {
        Ok(exists) => exists != 0,
        Err(err) => {
            log::error!("Failed to read exists from {}: {}", addr, err);
            return Err(err);
        }
    };

    if !exists {
        log::debug!("File does not exist on {}", addr);
        return Ok(false);
    }

    log::debug!("Reading whether file size matches from {}", addr);
    let size_match = match socket.read_u8().await {
        Ok(size_match) => size_match != 0,
        Err(err) => {
            log::error!("Failed to read size match from {}: {}", addr, err);
            return Err(err);
        }
    };

    if !size_match {
        log::debug!("File size does not match on {}", addr);
        return Ok(false);
    }

    let mut buffer = vec![0; common::PREHASH_CHUNK_SIZE];
    let mut hasher = blake3::Hasher::new();

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);
    }

    let hash = *hasher.finalize().as_bytes();

    log::debug!("Sending hash {:?} to {}", hash, addr);
    socket.write_all(&hash).await?;

    log::debug!("Reading whether hash matches from {}", addr);
    let hash_match = match socket.read_u8().await {
        Ok(hash_match) => hash_match != 0,
        Err(err) => {
            log::error!("Failed to read hash match from {}: {}", addr, err);
            return Err(err);
        }
    };

    Ok(hash_match)
}

async fn handle_mode1_fetch(mut socket: TcpStream, addr: SocketAddr) -> io::Result<()> {
    log::debug!("Reading chunk size from {}", addr);
    let chunk_size = match socket.read_u64().await {
        Ok(chunk_size) => chunk_size as usize,
        Err(err) => {
            log::error!("Failed to read chunk size from {}: {}", addr, err);
            return Err(err);
        }
    };

    log::debug!("Chunk size: {}; reading prehash from {}", chunk_size, addr);
    let prehash = match socket.read_u8().await {
        Ok(prehash) => prehash != 0,
        Err(err) => {
            log::error!("Failed to read prehash from {}: {}", addr, err);
            return Err(err);
        }
    };

    log::debug!(
        "Prehash: {}; reading prehash threshold from {}",
        prehash,
        addr
    );
    let prehash_threshold = match socket.read_u64().await {
        Ok(prehash_threshold) => prehash_threshold,
        Err(err) => {
            log::error!("Failed to read prehash threshold from {}: {}", addr, err);
            return Err(err);
        }
    };

    log::debug!(
        "Prehash threshold: {}; reading checksum from {}",
        prehash_threshold,
        addr
    );
    let do_checksum = match socket.read_u8().await {
        Ok(do_checksum) => do_checksum != 0,
        Err(err) => {
            log::error!("Failed to read checksum from {}: {}", addr, err);
            return Err(err);
        }
    };

    log::debug!("Checksum: {}; sending ack to {}", do_checksum, addr);
    socket.write_u8(1).await.inspect_err(|err| {
        log::error!("Failed to write ack to {}: {}", addr, err);
    })?;

    let mut buffer = vec![0; chunk_size];
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
        log::debug!("Reading directory {}", path.display());
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(err) => {
                log::warn!("Failed to read directory {}: {}", path.display(), err);
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    log::warn!("Failed to read directory entry: {}", err);
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
                log::debug!("Skipping symlink {}", entry.path().display());
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

            if !IMAGE_EXT.contains(&ext) && !VIDEO_EXT.contains(&ext) {
                continue;
            }

            log::debug!("Sending file {} to {}", path.display(), addr);
            let size = metadata.len();

            log::debug!("Sending size {} to {}", size, addr);
            socket.write_u64(size).await?;

            let path = path.to_string_lossy().to_string();
            let path_len = path.len() as u16;

            log::debug!("Sending path size {} to {}", path_len, addr);
            socket.write_u16(path_len).await?;

            log::debug!("Sending path {} to {}", path, addr);
            socket.write_all(path.as_bytes()).await?;

            let mut file = match fs::File::open(&path) {
                Ok(file) => file,
                Err(err) => {
                    log::error!("Failed to open file {}: {}", path, err);
                    continue;
                }
            };

            if prehash && size >= prehash_threshold {
                let hash_match = prehash_check(&mut socket, &mut file, addr).await?;
                if hash_match {
                    continue;
                }

                file.seek(io::SeekFrom::Start(0))?;
            }

            let mut hasher = do_checksum.then(|| blake3::Hasher::new());
            let mut total_read = 0;

            loop {
                let to_read = chunk_size.min((size - total_read) as usize);
                log::debug!("Reading {} bytes from {}", to_read, path);

                if to_read == 0 {
                    log::debug!("Finished reading file {}", path);
                    break;
                }

                if let Err(err) = file.read_exact(&mut buffer[..to_read]) {
                    log::error!("Failed to read file {}: {}", path, err);
                    break;
                };

                total_read += to_read as u64;
                log::debug!("Sending {} bytes to {}", to_read, addr);

                socket.write_all(&buffer[..to_read]).await?;
                hasher
                    .as_mut()
                    .map(|hasher| hasher.update(&buffer[..to_read]));

                log::debug!("Reading ack from {}", addr);
                let ack = match socket.read_u8().await {
                    Ok(ack) => ack,
                    Err(err) => {
                        log::error!("Failed to read ack from {}: {}", addr, err);
                        break;
                    }
                };

                if ack != 1 {
                    log::error!("Failed to read file {}: invalid ack", path);
                    break;
                }
            }

            if let Some(hasher) = hasher {
                let hash = *hasher.finalize().as_bytes();

                log::debug!("Sending hash {:?} to {}", hash, addr);
                socket.write_all(&hash).await?;
            }
        }
    }

    match cleanup(socket, addr).await {
        Ok(_) => Ok(()),
        Err(err) => {
            log::error!("Failed to cleanup connection with {}: {}", addr, err);
            Err(err)
        }
    }
}

async fn cleanup(mut socket: TcpStream, addr: SocketAddr) -> io::Result<()> {
    socket.write_u64(0).await?;
    log::debug!("Connection with {} closed", addr);
    Ok(())
}
