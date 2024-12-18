use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::{env, fs, process};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time;

mod common;

static SERVER_MODE: OnceLock<common::Mode> = OnceLock::new();
static TARGET_DIR: OnceLock<PathBuf> = OnceLock::new();

const DEFAULT_CHUNK_SIZE: u64 = 1024 * 32;
static CHUNK_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_CHUNK_SIZE as usize);
static PREHASH: AtomicBool = AtomicBool::new(false);

fn prompt_mode() -> common::Mode {
    let mode = env::args().nth(1).unwrap_or_else(|| {
        print!("Enter mode (scan/fetch): ");

        io::stdout().flush().expect("Failed to flush stdout");

        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .expect("Failed to read line");

        line
    });

    let mode = mode.trim().to_lowercase();
    let mode = match common::Mode::from_str(&mode) {
        Ok(mode) => mode,
        Err(()) => {
            eprintln!("Invalid mode: {}", mode);
            process::exit(1);
        }
    };

    let _ = SERVER_MODE.set(mode.clone());
    mode
}

fn prompt_output_path() {
    let path = env::args().nth(2).unwrap_or_else(|| {
        print!("Enter path: ");
        io::stdout().flush().expect("Failed to flush stdout");

        let mut path = String::new();
        io::stdin()
            .read_line(&mut path)
            .expect("Failed to read line");

        path
    });

    let path = PathBuf::from(path.trim());
    if !path.exists() {
        eprintln!("Path does not exist: {:?}", path);
        process::exit(1);
    }

    let _ = TARGET_DIR.set(path);
}

fn storage_unit(bytes: u64) -> (f64, &'static str) {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    let mut bytes = bytes as f64;

    while bytes >= 1024.0 {
        bytes /= 1024.0;
        unit += 1;
    }

    (bytes, units[unit])
}

fn get_host() -> String {
    let host = match env::var("HOST") {
        Ok(host) => host,
        Err(_) => {
            log::debug!("HOST not set, using");
            String::from("0.0.0.0")
        }
    };

    match SocketAddrV4::from_str(&host) {
        Ok(_) => return host,
        Err(_) => {
            log::debug!("Provided HOST is not a valid SocketAddrV4, trying with Ipv4Addr");
        }
    };

    if let Err(err) = host.parse::<Ipv4Addr>() {
        log::error!("Invalid HOST: {}", err);
        process::exit(1);
    }

    let port = match env::var("PORT") {
        Ok(port) => port,
        Err(_) => {
            log::debug!("PORT not set, using {}", common::DEFAULT_PORT);
            common::DEFAULT_PORT.to_string()
        }
    };

    match port.parse::<u16>() {
        Ok(_) => {
            log::debug!("PORT set to {}", port);
            format!("{}:{}", host, port)
        }
        Err(_) => {
            log::warn!(
                "Invalid port, falling back to default ({})",
                common::DEFAULT_PORT
            );
            format!("{}:{}", host, common::DEFAULT_PORT)
        }
    }
}

#[tokio::main]
async fn main() {
    common::rust_log_init();

    let mode = prompt_mode();
    if mode.is_fetch() {
        prompt_output_path();
    }

    if let Ok(chunk_size) = env::var("CHUNK_SIZE") {
        if let Ok(chunk_size) = chunk_size.parse::<u64>() {
            CHUNK_SIZE.store(chunk_size as usize, Ordering::Relaxed);
        } else {
            log::warn!("Invalid CHUNK_SIZE, using default ({})", DEFAULT_CHUNK_SIZE);
        }
    }

    if env::var("USE_PREHASH").is_ok() {
        PREHASH.store(true, Ordering::Relaxed);
    }

    let addr = get_host();
    let listener = match TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(err) => {
            log::error!("Failed to bind to {}: {}", addr, err);
            process::exit(1);
        }
    };

    log::debug!("Chunk size: {}", CHUNK_SIZE.load(Ordering::Relaxed));
    log::debug!("Prehash: {}", PREHASH.load(Ordering::Relaxed));

    log::info!(
        "Server started on {} in {} mode{}",
        addr,
        mode,
        if mode.is_fetch() {
            format!(" with output path {:?}", TARGET_DIR.get().unwrap())
        } else {
            "".to_string()
        }
    );

    loop {
        let (socket, addr) = match listener.accept().await {
            Ok((socket, addr)) => {
                log::debug!("Accepted connection from {}", addr);
                (socket, addr)
            }
            Err(err) => {
                log::error!("Failed to accept connection: {}", err);
                continue;
            }
        };

        tokio::spawn(async move {
            match handle_connection(socket, addr).await {
                Ok(_) => log::debug!("Connection with {} closed", addr),
                Err(_) => log::error!("Connection with {} unexpectedly closed", addr),
            };
        });
    }
}

async fn handle_connection(mut socket: TcpStream, addr: SocketAddr) -> io::Result<()> {
    socket
        .write_all(common::VERIFY_MESSAGE_SERVER.as_bytes())
        .await?;

    let mut buf = [0; common::VERIFY_MESSAGE_CLIENT.len()];
    match time::timeout(common::CONNECTION_TIMEOUT, socket.read_exact(&mut buf)).await {
        Ok(Err(err)) => {
            log::error!("Failed to read verification message from {}: {}", addr, err);
            return Err(err);
        }
        Err(_) => {
            log::error!("Connection with {} timed out", addr);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection timed out",
            ));
        }
        _ => {}
    };

    if buf != common::VERIFY_MESSAGE_CLIENT.as_bytes() {
        log::error!("Invalid verification message from {}", addr);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid data"));
    }

    let mode = SERVER_MODE.get().expect("mode not set");
    if let Err(err) = socket.write_u8(mode.into()).await {
        log::error!("Failed to send mode to {}: {}", addr, err);
        return Err(err);
    }

    let os = match socket.read_u8().await {
        Ok(os) => os,
        Err(err) => {
            log::error!("Failed to read OS from {}: {}", addr, err);
            return Err(err);
        }
    };

    if os != common::LINUX_ID && os != common::WINDOWS_ID {
        log::error!("Invalid OS from {}", addr);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid data"));
    }

    let version = env!("CARGO_PKG_VERSION");
    socket.write_u8(version.len() as u8).await?;
    socket.write_all(version.as_bytes()).await?;

    let ack = match socket.read_u8().await {
        Ok(ack) => ack,
        Err(err) => {
            log::error!("Failed to read ack from {}: {}", addr, err);
            return Err(err);
        }
    };

    if ack != 1 {
        log::error!("Invalid ack from {}", addr);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid data"));
    }

    log::info!(
        "Verified connection with {} as {}",
        addr,
        if os == 0 { "Windows" } else { "Linux" }
    );

    match mode {
        common::Mode::Scan => handle_mode0_scan(socket, addr).await,
        common::Mode::Fetch => handle_mode1_fetch(socket, addr, os).await,
    }
}

async fn handle_mode0_scan(mut socket: TcpStream, addr: SocketAddr) -> io::Result<()> {
    let mut extensions: HashMap<String, (u64, u64)> = HashMap::new();
    let mut ext_buf = [0; u8::MAX as usize];

    loop {
        let ext_length = match socket.read_u8().await {
            Ok(ext_length) => ext_length,
            Err(err) => {
                log::error!("Failed to read extension length from {}: {}", addr, err);
                return Err(err);
            }
        };

        if ext_length == 0 {
            break;
        }

        if let Err(err) = socket.read_exact(&mut ext_buf[..ext_length as usize]).await {
            log::error!("Failed to read extension from {}: {}", addr, err);
            return Err(err);
        }

        let ext = match String::from_utf8(ext_buf[..ext_length as usize].to_vec()) {
            Ok(ext) => ext,
            Err(err) => {
                log::error!("Failed to parse extension from {}: {}", addr, err);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid extension: {}", err),
                ));
            }
        };

        let count = match socket.read_u64().await {
            Ok(count) => count,
            Err(err) => {
                log::error!("Failed to read count from {}: {}", addr, err);
                return Err(err);
            }
        };

        let size = match socket.read_u64().await {
            Ok(size) => size,
            Err(err) => {
                log::error!("Failed to read size from {}: {}", addr, err);
                return Err(err);
            }
        };

        let entry = extensions.entry(ext).or_insert((0, 0));
        entry.0 += count;
        entry.1 += size;
    }

    log::info!("Scan completed for {}", addr);

    let padding = extensions
        .iter()
        .fold((0, 0, 0), |acc, (ext, (count, size))| {
            let (size, unit) = storage_unit(*size);

            (
                acc.0.max(ext.len()),
                acc.1.max(count.to_string().len()),
                acc.2.max(format!("{:.2} {}", size, unit).len()),
            )
        });

    let (total_count, total_size) = extensions
        .values()
        .fold((0, 0), |acc, (count, size)| (acc.0 + count, acc.1 + size));

    let (total_size, total_size_unit) = storage_unit(total_size);
    let padding = (
        padding.0.max("Extension".len()).max("Total".len()),
        padding
            .1
            .max("Count".len())
            .max(total_count.to_string().len()),
        padding
            .2
            .max(format!("{:.2} {}", total_size, total_size_unit).len()),
    );

    println!(
        "\n {:<padding0$} | {:<padding1$} | {:<padding2$}",
        "Extension",
        "Count",
        "Size",
        padding0 = padding.0,
        padding1 = padding.1,
        padding2 = padding.2
    );

    println!(
        "-{:-<padding0$}---{:-<padding1$}---{:-<padding2$}-",
        "",
        "",
        "",
        padding0 = padding.0,
        padding1 = padding.1,
        padding2 = padding.2
    );

    let mut sorted = extensions.into_iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| b.1.0.cmp(&a.1.0));

    println!(
        " {:<padding0$} | {:<padding1$} | {:<padding2$}",
        "Total",
        total_count,
        format!("{:.2} {}", total_size, total_size_unit),
        padding0 = padding.0,
        padding1 = padding.1,
        padding2 = padding.2
    );

    for (ext, (count, size)) in sorted {
        let (size, unit) = storage_unit(size);
        println!(
            " {:<padding0$} | {:<padding1$} | {:<padding2$}",
            ext,
            count,
            format!("{:.2} {}", size, unit),
            padding0 = padding.0,
            padding1 = padding.1,
            padding2 = padding.2
        );
    }

    println!();
    match socket.shutdown().await {
        Ok(_) => {
            log::debug!("Connection with {} shutdown", addr);
            Ok(())
        }
        Err(err) => {
            log::error!("Failed to shutdown connection with {}: {}", addr, err);
            Err(err)
        }
    }
}

fn trim_root_and_format(mut path: String, os: u8) -> PathBuf {
    if os == common::WINDOWS_ID {
        let idx = path.find(':');
        if idx.is_some() {
            path = path.split_off(idx.unwrap() + 1);
        }
    }

    if cfg!(windows) && os == 0 {
        path = path.replace("/", "\\");
    } else if cfg!(unix) && os == 1 {
        path = path.replace("\\", "/");
    }

    PathBuf::from(path.trim_start_matches(std::path::MAIN_SEPARATOR))
}

async fn prehash_chech(
    socket: &mut TcpStream,
    path: &PathBuf,
    addr: SocketAddr,
) -> io::Result<bool> {
    let exists = fs::metadata(path).is_ok();
    socket.write_u8(if exists { 1 } else { 0 }).await?;

    if !exists {
        return Ok(false);
    }

    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            log::error!("Failed to open file {}: {}", path.display(), err);
            return Err(err);
        }
    };

    let mut hasher = blake3::Hasher::new();
    let mut buf = [0; common::PREHASH_CHUNK_SIZE];

    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }

        hasher.update(&buf[..read]);
    }

    let mut client_hash = [0; blake3::OUT_LEN];
    if let Err(err) = socket.read_exact(&mut client_hash).await {
        log::error!("Failed to read hash from {}: {}", addr, err);
        return Err(err);
    }

    let hash = hasher.finalize();
    let hash_match = *hash.as_bytes() == client_hash;

    socket.write_u8(if hash_match { 1 } else { 0 }).await?;
    log::debug!(
        "Prehash check for {} from {}: {}",
        path.display(),
        addr,
        if hash_match { "OK" } else { "FAIL" }
    );

    drop(file);
    if !hash_match {
        fs::remove_file(path)?;
    }

    Ok(hash_match)
}

async fn handle_mode1_fetch(mut socket: TcpStream, addr: SocketAddr, os: u8) -> io::Result<()> {
    let chunk_size = CHUNK_SIZE.load(Ordering::Relaxed);
    let prehash = PREHASH.load(Ordering::Relaxed);

    socket.write_u64(chunk_size as u64).await?;
    socket.write_u8(if prehash { 1 } else { 0 }).await?;

    let ack = match socket.read_u8().await {
        Ok(ack) => ack,
        Err(err) => {
            log::error!("Failed to read ack from {}: {}", addr, err);
            return Err(err);
        }
    };

    if ack != 1 {
        log::error!("Invalid ack from {}", addr);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid data"));
    }

    let mut buf = vec![0; chunk_size];

    loop {
        let size = match socket.read_u64().await {
            Ok(size) => size,
            Err(err) => {
                log::error!("Failed to read size from {}: {}", addr, err);
                return Err(err);
            }
        };

        if size == 0 {
            break;
        }

        let path_len = match socket.read_u16().await {
            Ok(path_len) => path_len,
            Err(err) => {
                log::error!("Failed to read path length from {}: {}", addr, err);
                return Err(err);
            }
        };

        let mut path_buf = vec![0; path_len as usize];
        if let Err(err) = socket.read_exact(&mut path_buf).await {
            log::error!("Failed to read path from {}: {}", addr, err);
            return Err(err);
        }

        let path = match String::from_utf8(path_buf) {
            Ok(path) => path,
            Err(err) => {
                log::error!("Failed to parse path from {}: {}", addr, err);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid path: {}", err),
                ));
            }
        };

        let path = TARGET_DIR
            .get()
            .unwrap()
            .join(trim_root_and_format(path, os));

        if let Err(err) = fs::create_dir_all(path.parent().unwrap()) {
            log::error!("Failed to create directory for {}: {}", path.display(), err);
            return Err(err);
        }

        if prehash {
            if prehash_chech(&mut socket, &path, addr).await? {
                continue;
            }
        }

        log::debug!("Receiving file {} from {}", path.display(), addr);
        let mut file = match fs::File::create(&path) {
            Ok(file) => file,
            Err(err) => {
                log::error!("Failed to create file {}: {}", path.display(), err);
                return Err(err);
            }
        };

        let mut hasher = blake3::Hasher::new();
        let mut total_read = 0;

        loop {
            let to_read = match socket.read_u64().await {
                Ok(to_read) => to_read,
                Err(err) => {
                    log::error!("Failed to read chunk size from {}: {}", addr, err);
                    return Err(err);
                }
            };

            if to_read == 0 {
                break;
            }

            if let Err(err) = socket.read_exact(&mut buf[..to_read as usize]).await {
                log::error!("Failed to read chunk from {}: {}", addr, err);
                return Err(err);
            }

            total_read += to_read;
            hasher.update(&buf[..to_read as usize]);

            if let Err(err) = file.write_all(&buf[..to_read as usize]) {
                log::error!("Failed to write chunk to {}: {}", path.display(), err);
                return Err(err);
            }

            if let Err(err) = socket.write_all(&[0]).await {
                log::error!("Failed to send ack to {}: {}", addr, err);
                return Err(err);
            }

            if total_read == size {
                break;
            }
        }

        if total_read != size {
            log::error!("Size mismatch for {}", path.display());
            return Err(io::Error::new(io::ErrorKind::InvalidData, "size mismatch"));
        }

        let mut client_hash = [0; blake3::OUT_LEN];
        if let Err(err) = socket.read_exact(&mut client_hash).await {
            log::error!("Failed to read hash from {}: {}", addr, err);
            return Err(err);
        }

        let hash = hasher.finalize();
        if *hash.as_bytes() != client_hash {
            log::error!("Hash mismatch for {}", path.display());
            return Err(io::Error::new(io::ErrorKind::InvalidData, "hash mismatch"));
        }
    }

    match socket.shutdown().await {
        Ok(_) => {
            log::debug!("Connection with {} shutdown", addr);
            Ok(())
        }
        Err(err) => {
            log::error!("Failed to shutdown connection with {}: {}", addr, err);
            Err(err)
        }
    }
}
