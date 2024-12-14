use std::env;

use tokio::time::Duration;

pub const DEFAULT_PORT: u16 = 2137;
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
pub const VERIFY_MESSAGE_SERVER: &str = "syncer-verify-server";
pub const VERIFY_MESSAGE_CLIENT: &str = "syncer-verify-client";

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

impl Into<u8> for Mode {
    fn into(self) -> u8 {
        match self {
            Self::Scan => 0,
            Self::Fetch => 1,
        }
    }
}

impl Into<u8> for &Mode {
    fn into(self) -> u8 {
        match self {
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

pub fn rust_log_init() {
    if env::var("RUST_LOG").is_err() {
        unsafe {
            env::set_var("RUST_LOG", "info");
        }
    }

    pretty_env_logger::init();
}
