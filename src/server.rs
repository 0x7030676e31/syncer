use std::collections::HashMap;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;
use std::{env, process};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time;

mod common;

static SERVER_MODE: OnceLock<common::Mode> = OnceLock::new();
static TARGET_DIR: OnceLock<PathBuf> = OnceLock::new();

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

#[tokio::main]
async fn main() {
    common::rust_log_init();

    let mode = prompt_mode();
    if mode.is_fetch() {
        prompt_output_path();
    }

    let port = env::var("PORT").unwrap_or(common::DEFAULT_PORT.to_string());
    if port.parse::<u16>().is_err() {
        eprintln!("Invalid port: {}", port);
        process::exit(1);
    }

    let addr = format!("0.0.0.0:{}", port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(err) => {
            log::error!("Failed to bind to {}: {}", addr, err);
            process::exit(1);
        }
    };

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

    log::info!("Verified connection with {}", addr);

    let mode = SERVER_MODE.get().expect("mode not set");
    if let Err(err) = socket.write_u8(mode.into()).await {
        log::error!("Failed to send mode to {}: {}", addr, err);
        return Err(err);
    }

    match mode {
        common::Mode::Scan => handle_mode0_scan(socket, addr).await,
        common::Mode::Fetch => handle_mode1_fetch(socket, addr).await,
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

    print!(
        "\n {:<padding0$} | {:<padding1$} | {:<padding2$}\n",
        "Extension",
        "Count",
        "Size",
        padding0 = padding.0,
        padding1 = padding.1,
        padding2 = padding.2
    );

    print!(
        "-{:-<padding0$}---{:-<padding1$}---{:-<padding2$}-\n",
        "",
        "",
        "",
        padding0 = padding.0,
        padding1 = padding.1,
        padding2 = padding.2
    );

    let mut sorted = extensions.into_iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| b.1.0.cmp(&a.1.0));

    print!(
        " {:<padding0$} | {:<padding1$} | {:<padding2$}\n",
        "Total",
        total_count,
        format!("{:.2} {}", total_size, total_size_unit),
        padding0 = padding.0,
        padding1 = padding.1,
        padding2 = padding.2
    );

    for (ext, (count, size)) in sorted {
        let (size, unit) = storage_unit(size);
        print!(
            " {:<padding0$} | {:<padding1$} | {:<padding2$}\n",
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
            return Ok(());
        }
        Err(err) => {
            log::error!("Failed to shutdown connection with {}: {}", addr, err);
            return Err(err);
        }
    }
}

async fn handle_mode1_fetch(mut _socket: TcpStream, _addr: SocketAddr) -> io::Result<()> {
    println!("Fetching...");
    Ok(())
}
