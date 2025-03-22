#![feature(ptr_as_ref_unchecked)]

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
static PREHASH_THRESHOLD: AtomicU64 = AtomicU64::new(1024 * 1024 * 32);
static CHECKSUM: AtomicBool = AtomicBool::new(false);
static CHECKSUM_MODE: OnceLock<common::ChecksumMode> = OnceLock::new();

static IS_DEBUG_ENABLED: AtomicBool = AtomicBool::new(false);

// This struct is shitty af and there is for sure a better way to do this
// but as of now it's 3AM and I'm too tired to think of a better way
// so it's probably going to stay like this :((
struct ProgressiveLine {
    line: UnsafeCell<String>,
    stdout: UnsafeCell<Option<io::Stdout>>,
}

impl ProgressiveLine {
    pub const fn new() -> Self {
        Self {
            line: UnsafeCell::new(String::new()),
            stdout: UnsafeCell::new(None),
        }
    }

    pub fn init(&self) {
        let stdout = io::stdout();
        unsafe {
            *self.stdout.get() = Some(stdout);
        }
    }

    pub fn set(&self, line: String) {
        unsafe {
            if let Some(stdout) = &mut *self.stdout.get() {
                let _ = write!(stdout, "\x1B[K{}\n", line);
                let _ = stdout.flush();
            }

            *self.line.get() = line;
        }
    }

    pub fn update(&self, line: String) {
        unsafe {
            if let Some(stdout) = &mut *self.stdout.get() {
                let _ = write!(stdout, "\x1B[A\x1B[K{}\n", line);
                let _ = stdout.flush();
            }

            *self.line.get() = line;
        }
    }

    pub fn clear(&self) {
        unsafe {
            if let Some(stdout) = &mut *self.stdout.get() {
                let _ = write!(stdout, "\x1B[A\x1B[K");
                let _ = stdout.flush();
            }

            *self.line.get() = String::new();
        }
    }

    pub fn exec_and_print<F>(&self, f: F)
    where
        F: FnOnce() -> (),
    {
        unsafe {
            if let Some(stdout) = &mut *self.stdout.get() {
                let _ = write!(stdout, "\x1B[A\x1B[K");
                let _ = stdout.flush();
                f();

                if !self.line.get().as_ref_unchecked().is_empty() {
                    let _ = write!(stdout, "{}\n", *self.line.get());
                    let _ = stdout.flush();
                }
            }
        }
    }
}

unsafe impl Sync for ProgressiveLine {}
unsafe impl Send for ProgressiveLine {}

static PROGRESSIVE_LINE: ProgressiveLine = ProgressiveLine::new();

mod progressive {
    use super::*;

    macro_rules! info {
        ($($arg:tt)*) => {
            println!("Info");
            PROGRESSIVE_LINE.exec_and_print(|| {
                log::info!($($arg)*);
            });
        };
    }

    macro_rules! debug {
        ($($arg:tt)*) => {
            if IS_DEBUG_ENABLED.load(Ordering::Relaxed) {
                PROGRESSIVE_LINE.exec_and_print(|| {
                    log::debug!($($arg)*);
                });
            }
        };
    }

    macro_rules! warn_ {
        ($($arg:tt)*) => {
            PROGRESSIVE_LINE.exec_and_print(|| {
                log::warn!($($arg)*);
            });
        };
    }

    macro_rules! error {
        ($($arg:tt)*) => {
            PROGRESSIVE_LINE.exec_and_print(|| {
                log::error!($($arg)*);
            })
        };
    }

    macro_rules! set {
        ($($arg:tt)*) => {
            PROGRESSIVE_LINE.set(format!($($arg)*));
        };
    }

    macro_rules! update {
        ($($arg:tt)*) => {
            PROGRESSIVE_LINE.update(format!($($arg)*));
        };
    }

    pub fn clear() {
        PROGRESSIVE_LINE.clear();
    }

    pub(super) use debug;
    pub(super) use error;
    pub(super) use info;
    pub(super) use set;
    pub(super) use update;
    pub(super) use warn_ as warn;
}

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
            progressive::debug!("HOST not set, using");
            String::from("0.0.0.0")
        }
    };

    match SocketAddrV4::from_str(&host) {
        Ok(_) => return host,
        Err(_) => {
            progressive::debug!("Provided HOST is not a valid SocketAddrV4, trying with Ipv4Addr");
        }
    };

    if let Err(err) = host.parse::<Ipv4Addr>() {
        progressive::error!("Invalid HOST: {}", err);
        process::exit(1);
    }

    let port = match env::var("PORT") {
        Ok(port) => port,
        Err(_) => {
            progressive::debug!("PORT not set, using {}", common::DEFAULT_PORT);
            common::DEFAULT_PORT.to_string()
        }
    };

    match port.parse::<u16>() {
        Ok(_) => {
            progressive::debug!("PORT set to {}", port);
            format!("{}:{}", host, port)
        }
        Err(_) => {
            progressive::warn!(
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

    IS_DEBUG_ENABLED.store(log::log_enabled!(log::Level::Debug), Ordering::Relaxed);
    PROGRESSIVE_LINE.init();

    let mode = prompt_mode();
    if mode.is_fetch() {
        prompt_output_path();
    }

    let checksum_mode = match env::var("CHECKSUM_MODE") {
        Ok(checksum_mode) => match common::ChecksumMode::from_str(&checksum_mode) {
            Ok(checksum_mode) => checksum_mode,
            Err(()) => {
                progressive::warn!(
                    "Invalid CHECKSUM_MODE, using default ({})",
                    common::ChecksumMode::default()
                );
                common::ChecksumMode::default()
            }
        },
        Err(_) => {
            progressive::debug!(
                "CHECKSUM_MODE not set, using default ({})",
                common::ChecksumMode::default()
            );
            common::ChecksumMode::default()
        }
    };

    let _ = CHECKSUM_MODE.set(checksum_mode);

    if let Ok(chunk_size) = env::var("CHUNK_SIZE") {
        if let Ok(chunk_size) = chunk_size.parse::<u64>() {
            CHUNK_SIZE.store(chunk_size as usize, Ordering::Relaxed);
        } else {
            progressive::warn!("Invalid CHUNK_SIZE, using default ({})", DEFAULT_CHUNK_SIZE);
        }
    }

    if env::var("USE_PREHASH").is_ok() {
        PREHASH.store(true, Ordering::Relaxed);
    }

    if let Ok(prehash_threshold) = env::var("PREHASH_THRESHOLD") {
        if let Ok(prehash_threshold) = prehash_threshold.parse::<u64>() {
            PREHASH_THRESHOLD.store(prehash_threshold, Ordering::Relaxed);
        } else {
            progressive::warn!(
                "Invalid PREHASH_THRESHOLD, using default ({})",
                PREHASH_THRESHOLD.load(Ordering::Relaxed)
            );
        }
    }

    if env::var("CHECKSUM").is_ok() {
        CHECKSUM.store(true, Ordering::Relaxed);
    }
    let addr = get_host();
    let listener = match TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(err) => {
            progressive::error!("Failed to bind to {}: {}", addr, err);
            process::exit(1);
        }
    };

    progressive::debug!("Chunk size: {}", CHUNK_SIZE.load(Ordering::Relaxed));
    progressive::debug!("Prehash: {}", PREHASH.load(Ordering::Relaxed));
    progressive::debug!(
        "Prehash threshold: {}",
        PREHASH_THRESHOLD.load(Ordering::Relaxed)
    );
    progressive::debug!("Checksum: {}", CHECKSUM.load(Ordering::Relaxed));
    progressive::debug!("Checksum mode: {}", CHECKSUM_MODE.get().unwrap());

    progressive::info!(
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
                progressive::debug!("Accepted connection from {}", addr);
                (socket, addr)
            }
            Err(err) => {
                progressive::error!("Failed to accept connection: {}", err);
                continue;
            }
        };

        tokio::spawn(async move {
            match handle_connection(socket, addr).await {
                Ok(_) => progressive::debug!("Connection with {} closed", addr),
                Err(_) => progressive::error!("Connection with {} unexpectedly closed", addr),
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
            progressive::error!("Failed to read verification message from {}: {}", addr, err);
            return Err(err);
        }
        Err(_) => {
            progressive::error!("Connection with {} timed out", addr);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection timed out",
            ));
        }
        _ => {}
    };

    if buf != common::VERIFY_MESSAGE_CLIENT.as_bytes() {
        progressive::error!("Invalid verification message from {}", addr);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid data"));
    }

    let mode = SERVER_MODE.get().expect("mode not set");
    if let Err(err) = socket.write_u8(mode.into()).await {
        progressive::error!("Failed to send mode to {}: {}", addr, err);
        return Err(err);
    }

    let os = match socket.read_u8().await {
        Ok(os) => os,
        Err(err) => {
            progressive::error!("Failed to read OS from {}: {}", addr, err);
            return Err(err);
        }
    };

    if os != common::LINUX_ID && os != common::WINDOWS_ID {
        progressive::error!("Invalid OS from {}", addr);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid data"));
    }

    let version = env!("CARGO_PKG_VERSION");
    socket.write_u8(version.len() as u8).await?;
    socket.write_all(version.as_bytes()).await?;

    let ack = match socket.read_u8().await {
        Ok(ack) => ack,
        Err(err) => {
            progressive::error!("Failed to read ack from {}: {}", addr, err);
            return Err(err);
        }
    };

    if ack != 1 {
        progressive::error!("Invalid ack from {}", addr);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid data"));
    }

    progressive::info!(
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
        progressive::debug!("Reading extension length from {}", addr);
        let ext_length = match socket.read_u8().await {
            Ok(ext_length) => ext_length,
            Err(err) => {
                progressive::error!("Failed to read extension length from {}: {}", addr, err);
                return Err(err);
            }
        };

        if ext_length == 0 {
            progressive::debug!("Received end of scan from {}", addr);
            break;
        }

        progressive::debug!("Reading extension ({} bytes) from {}", ext_length, addr);
        if let Err(err) = socket.read_exact(&mut ext_buf[..ext_length as usize]).await {
            progressive::error!("Failed to read extension from {}: {}", addr, err);
            return Err(err);
        }

        let ext = match String::from_utf8(ext_buf[..ext_length as usize].to_vec()) {
            Ok(ext) => ext,
            Err(err) => {
                progressive::error!("Failed to parse extension from {}: {}", addr, err);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid extension: {}", err),
                ));
            }
        };

        progressive::debug!("Reading count from {}", addr);
        let count = match socket.read_u64().await {
            Ok(count) => count,
            Err(err) => {
                progressive::error!("Failed to read count from {}: {}", addr, err);
                return Err(err);
            }
        };

        progressive::debug!("Reading size from {}", addr);
        let size = match socket.read_u64().await {
            Ok(size) => size,
            Err(err) => {
                progressive::error!("Failed to read size from {}: {}", addr, err);
                return Err(err);
            }
        };

        let entry = extensions.entry(ext).or_insert((0, 0));
        entry.0 += count;
        entry.1 += size;
    }

    progressive::info!("Scan completed for {}", addr);

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
            progressive::debug!("Connection with {} shutdown", addr);
            Ok(())
        }
        Err(err) => {
            progressive::error!("Failed to shutdown connection with {}: {}", addr, err);
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

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut hex = String::new();
    for byte in bytes {
        hex.push_str(&format!("{:02x}", byte));
    }

    hex
}

async fn prehash_check(
    socket: &mut TcpStream,
    path: &PathBuf,
    size: u64,
    mode: &common::ChecksumMode,
    addr: SocketAddr,
) -> io::Result<bool> {
    let exists = fs::metadata(path).is_ok();
    progressive::debug!(
        "Sending exists ({} - {}) to {}",
        exists,
        if exists { 1 } else { 0 },
        addr
    );

    socket.write_u8(if exists { 1 } else { 0 }).await?;

    if !exists {
        progressive::debug!("File does not exist, skipping prehash check");
        return Ok(false);
    }

    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            progressive::error!("Failed to open file {}: {}", path.display(), err);
            return Err(err);
        }
    };

    let metadata = file.metadata()?;
    let is_size_equal = metadata.len() == size;

    progressive::debug!(
        "Sending size equal ({} - {}) to {}",
        is_size_equal,
        if is_size_equal { 1 } else { 0 },
        addr
    );

    socket.write_u8(if is_size_equal { 1 } else { 0 }).await?;

    if !is_size_equal {
        progressive::debug!("File size does not match, skipping prehash check");
        fs::remove_file(path)?;
        return Ok(false);
    }

    if mode.is_none() {
        progressive::debug!("No checksum mode set, skipping prehash check");
        return Ok(true);
    }

    let mut hasher = common::Hasher::new(mode);
    let mut buf = vec![0; common::PREHASH_CHUNK_SIZE];

    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }

        hasher.update(&buf[..read]);
    }

    progressive::debug!("Reading client hash from {}", addr);
    let mut client_hash = vec![0; mode.hash_size()];
    if let Err(err) = socket.read_exact(&mut client_hash).await {
        progressive::error!("Failed to read hash from {}: {}", addr, err);
        return Err(err);
    }

    let hash = hasher.finalize();
    progressive::debug!(
        "Server hash: {}, Client hash: {}",
        bytes_to_hex(&hash),
        bytes_to_hex(&client_hash)
    );

    let hash_match = hash == client_hash;
    progressive::debug!(
        "Sending hash match ({} - {}) to {}",
        hash_match,
        if hash_match { 1 } else { 0 },
        addr
    );

    socket.write_u8(if hash_match { 1 } else { 0 }).await?;
    drop(file);

    if !hash_match {
        progressive::error!("Hash mismatch for {}", path.display());
        fs::remove_file(path)?;
    }

    Ok(hash_match)
}

async fn handle_mode1_fetch(mut socket: TcpStream, addr: SocketAddr, os: u8) -> io::Result<()> {
    let chunk_size = CHUNK_SIZE.load(Ordering::Relaxed);
    let prehash = PREHASH.load(Ordering::Relaxed);
    let prehash_threshold = PREHASH_THRESHOLD.load(Ordering::Relaxed);
    let do_checksum = CHECKSUM.load(Ordering::Relaxed);
    let checksum_mode = CHECKSUM_MODE.get().unwrap();

    progressive::debug!("Sending config to {}", addr);
    socket.write_u64(chunk_size as u64).await?;
    socket.write_u8(if prehash { 1 } else { 0 }).await?;
    socket.write_u64(prehash_threshold).await?;
    socket.write_u8(if do_checksum { 1 } else { 0 }).await?;
    socket.write_u8(checksum_mode.into()).await?;

    progressive::debug!("Reading ack from {}", addr);
    let ack = match socket.read_u8().await {
        Ok(ack) => ack,
        Err(err) => {
            progressive::error!("Failed to read ack from {}: {}", addr, err);
            return Err(err);
        }
    };

    if ack != 1 {
        progressive::error!("Invalid ack from {}", addr);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid data"));
    }

    let mut buf = vec![0; chunk_size];

    loop {
        progressive::debug!("Reading size from {}", addr);
        let size = match socket.read_u64().await {
            Ok(size) => size,
            Err(err) => {
                progressive::error!("Failed to read size from {}: {}", addr, err);
                return Err(err);
            }
        };

        if size == 0 {
            progressive::debug!("Received end of fetch from {}", addr);
            break;
        }

        progressive::debug!("Size: {}", size);
        progressive::debug!("Reading path length from {}", addr);
        let path_len = match socket.read_u16().await {
            Ok(path_len) => path_len,
            Err(err) => {
                progressive::error!("Failed to read path length from {}: {}", addr, err);
                return Err(err);
            }
        };

        progressive::debug!("Reading path ({} bytes) from {}", path_len, addr);
        let mut path_buf = vec![0; path_len as usize];
        if let Err(err) = socket.read_exact(&mut path_buf).await {
            progressive::error!("Failed to read path from {}: {}", addr, err);
            return Err(err);
        }

        let path = match String::from_utf8(path_buf) {
            Ok(path) => path,
            Err(err) => {
                progressive::error!("Failed to parse path from {}: {}", addr, err);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid path: {}", err),
                ));
            }
        };

        progressive::debug!("Path: {}", path);
        let dest = TARGET_DIR
            .get()
            .unwrap()
            .join(trim_root_and_format(path.clone(), os));

        progressive::debug!("Creating directory for {}", dest.display());
        if let Err(err) = fs::create_dir_all(dest.parent().unwrap()) {
            progressive::error!("Failed to create directory for {}: {}", dest.display(), err);
            return Err(err);
        }

        if prehash && size >= prehash_threshold || checksum_mode.is_none() {
            progressive::set!("Checking prehash for {}", path);
            let prehash_result =
                prehash_check(&mut socket, &dest, size, &checksum_mode, addr).await?;

            progressive::clear();
            if prehash_result {
                progressive::info!("Prehash check passed for {}", path);
                continue;
            }
        }

        if fs::metadata(&dest).is_ok() {
            progressive::debug!("Removing existing file {}", dest.display());
            if let Err(err) = fs::remove_file(&dest) {
                progressive::error!("Failed to remove existing file {}: {}", dest.display(), err);
                return Err(err);
            }
        }

        let (f64_size, unit) = storage_unit(size);
        progressive::set!("Downloading: 0B / {:.2}{} - {}", f64_size, unit, path);

        let mut file = match fs::File::create(&dest) {
            Ok(file) => file,
            Err(err) => {
                progressive::error!("Failed to create file {}: {}", dest.display(), err);
                return Err(err);
            }
        };

        let doing_checksum = do_checksum && !checksum_mode.is_none();
        progressive::debug!("Doing checksum: {}", doing_checksum);

        let mut hasher = doing_checksum.then(|| common::Hasher::new(&checksum_mode));
        let mut total_read = 0;

        loop {
            let to_read = chunk_size.min((size - total_read) as usize);
            progressive::debug!("Reading chunk ({} bytes) from {}", to_read, addr);

            if to_read == 0 {
                progressive::debug!("End of file reached (0 bytes to read) from {}", addr);
                break;
            }

            if let Err(err) = socket.read_exact(&mut buf[..to_read]).await {
                progressive::error!("Failed to read chunk from {}: {}", addr, err);
                return Err(err);
            }

            total_read += to_read as u64;
            hasher.as_mut().map(|hasher| hasher.update(&buf[..to_read]));

            if let Err(err) = file.write_all(&buf[..to_read]) {
                progressive::error!("Failed to write chunk to {}: {}", dest.display(), err);
                return Err(err);
            }

            progressive::debug!("Sending ack to {}", addr);
            if let Err(err) = socket.write_u8(1).await {
                progressive::error!("Failed to send ack to {}: {}", addr, err);
                return Err(err);
            }

            let (f64_total_read, unit_total_read) = storage_unit(total_read);
            progressive::update!(
                "Downloading: {:.2}{} / {:.2}{} - {}",
                f64_total_read,
                unit_total_read,
                f64_size,
                unit,
                path
            );
        }

        if let Some(hasher) = hasher.take() {
            progressive::debug!("Reading client hash from {}", addr);
            let mut client_hash = vec![0; checksum_mode.hash_size()];
            if let Err(err) = socket.read_exact(&mut client_hash).await {
                progressive::error!("Failed to read hash from {}: {}", addr, err);
                return Err(err);
            }

            let hash = hasher.finalize();
            progressive::debug!(
                "Server hash: {}, Client hash: {}",
                bytes_to_hex(&hash),
                bytes_to_hex(&client_hash)
            );

            if hash != client_hash {
                progressive::error!("Hash mismatch for {}", dest.display());
                return Err(io::Error::new(io::ErrorKind::InvalidData, "hash mismatch"));
            }

            progressive::debug!("Hash match for {}", dest.display());
        }

        progressive::clear();
        progressive::info!("Downloaded {} from {}", path, addr);
    }

    match socket.shutdown().await {
        Ok(_) => {
            progressive::debug!("Connection with {} shutdown", addr);
            Ok(())
        }
        Err(err) => {
            progressive::error!("Failed to shutdown connection with {}: {}", addr, err);
            Err(err)
        }
    }
}
