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
    println!("Scanning...");
    Ok(())
}

async fn handle_mode1_fetch(mut socket: TcpStream, addr: SocketAddr) -> io::Result<()> {
    println!("Fetching...");
    Ok(())
}
