use std::io::Write;
use std::{env, fmt};

use tokio::time::Duration;

pub const DEFAULT_PORT: u16 = 2137;
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
pub const VERIFY_MESSAGE_SERVER: &str = "syncer-verify-server";
pub const VERIFY_MESSAGE_CLIENT: &str = "syncer-verify-client";
pub const PREHASH_CHUNK_SIZE: usize = 1024 * 1024;
pub const LINUX_ID: u8 = 0;
pub const WINDOWS_ID: u8 = 1;

#[cfg(unix)]
pub fn get_terminal_size() -> Option<(usize, usize)> {
    use libc::{TIOCGWINSZ, ioctl, winsize};
    use std::os::unix::io::AsRawFd;

    let fd = std::io::stdout().as_raw_fd();
    let mut ws: winsize = unsafe { std::mem::zeroed() };

    let result = unsafe { ioctl(fd, TIOCGWINSZ, &mut ws) };
    if result == 0 {
        Some((ws.ws_col as usize, ws.ws_row as usize))
    } else {
        None
    }
}

#[cfg(windows)]
pub fn get_terminal_size() -> Option<(usize, usize)> {
    use std::ptr;
    use winapi::um::processenv::GetStdHandle;
    use winapi::um::winbase::STD_OUTPUT_HANDLE;
    use winapi::um::wincon::{CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo};

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut csbi: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();

        if GetConsoleScreenBufferInfo(handle, &mut csbi) != 0 {
            let cols = (csbi.srWindow.Right - csbi.srWindow.Left + 1) as usize;
            let rows = (csbi.srWindow.Bottom - csbi.srWindow.Top + 1) as usize;
            Some((cols, rows))
        } else {
            None
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum Mode {
    Scan,
    Fetch,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Scan => write!(f, "scan"),
            Self::Fetch => write!(f, "fetch"),
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "scan" => Ok(Self::Scan),
            "fetch" => Ok(Self::Fetch),
            _ => Err(()),
        }
    }
}

impl Mode {
    pub fn is_scan(&self) -> bool {
        *self == Self::Scan
    }

    pub fn is_fetch(&self) -> bool {
        *self == Self::Fetch
    }
}

impl From<Mode> for u8 {
    fn from(mode: Mode) -> u8 {
        match mode {
            Mode::Scan => 0,
            Mode::Fetch => 1,
        }
    }
}

impl From<&Mode> for u8 {
    fn from(mode: &Mode) -> u8 {
        match mode {
            Mode::Scan => 0,
            Mode::Fetch => 1,
        }
    }
}

impl TryFrom<u8> for Mode {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Scan),
            1 => Ok(Self::Fetch),
            _ => Err(format!("Invalid mode value: {}", value)),
        }
    }
}

#[derive(PartialEq)]
pub enum ChecksumMode {
    None,
    Blake3,
    Crc32,
}

impl Default for ChecksumMode {
    fn default() -> Self {
        Self::Blake3
    }
}

impl std::fmt::Display for ChecksumMode {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Blake3 => write!(f, "blake3"),
            Self::Crc32 => write!(f, "crc32"),
        }
    }
}

impl std::str::FromStr for ChecksumMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.to_lowercase();
        match s.as_str() {
            "none" => Ok(Self::None),
            "blake3" | "blake" => Ok(Self::Blake3),
            "crc32" | "crc-32" => Ok(Self::Crc32),
            _ => Err(()),
        }
    }
}

impl From<ChecksumMode> for u8 {
    fn from(mode: ChecksumMode) -> u8 {
        match mode {
            ChecksumMode::None => 0,
            ChecksumMode::Blake3 => 1,
            ChecksumMode::Crc32 => 2,
        }
    }
}

impl From<&ChecksumMode> for u8 {
    fn from(mode: &ChecksumMode) -> u8 {
        match mode {
            ChecksumMode::None => 0,
            ChecksumMode::Blake3 => 1,
            ChecksumMode::Crc32 => 2,
        }
    }
}

impl TryFrom<u8> for ChecksumMode {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Blake3),
            2 => Ok(Self::Crc32),
            _ => Err(format!("Invalid precheck mode value: {}", value)),
        }
    }
}

impl ChecksumMode {
    pub fn is_none(&self) -> bool {
        *self == Self::None
    }

    pub fn hash_size(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Blake3 => 32,
            Self::Crc32 => 4,
        }
    }
}

pub enum Hasher {
    Blake3(blake3::Hasher),
    Crc32(crc32fast::Hasher),
}

impl Hasher {
    pub fn new(mode: &ChecksumMode) -> Self {
        match mode {
            ChecksumMode::None => panic!("Checksum mode cannot be None"),
            ChecksumMode::Blake3 => Self::Blake3(blake3::Hasher::new()),
            ChecksumMode::Crc32 => Self::Crc32(crc32fast::Hasher::new()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Self::Blake3(hasher) => {
                hasher.update(data);
            }
            Self::Crc32(hasher) => {
                hasher.update(data);
            }
        };
    }

    pub fn finalize(self) -> Vec<u8> {
        match self {
            Self::Blake3(hasher) => hasher.finalize().as_bytes().to_vec(),
            Self::Crc32(hasher) => hasher.finalize().to_be_bytes().to_vec(),
        }
    }
}

struct Padded<T> {
    value: T,
    width: usize,
}

impl<T: fmt::Display> fmt::Display for Padded<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{: <width$}", self.value, width = self.width)
    }
}

pub fn rust_log_init() {
    if env::var("RUST_LOG").is_err() {
        unsafe {
            env::set_var("RUST_LOG", "info");
        }
    }

    let mut builder = pretty_env_logger::formatted_builder();
    let _ = builder
        .parse_filters(&env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .format(|buf, record| {
            let target = record.target();
            let max_width = target.len();

            let style = buf.default_level_style(record.level());
            let level = style.value(Padded {
                value: record.level(),
                width: 5,
            });

            let mut style = buf.style();
            let target = style.set_bold(true).value(Padded {
                value: target,
                width: max_width,
            });

            let message = record.args().to_string();
            let width = level.to_string().len() + target.to_string().len() + 4;
            let message_width = message.len();
            let total_width = width + message_width;

            let terminal_width = match get_terminal_size() {
                Some((width, _)) => width,
                None => return Ok(()),
            };

            let message = if total_width > terminal_width {
                message[..terminal_width - width - 3].to_string() + "..."
            } else {
                message
            };

            writeln!(buf, "{} {} > {}", level, target, message)
        })
        .try_init();
}
