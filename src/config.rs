//! Configuration — parsed once at startup from environment variables if exist.
//!
//! Variables:
//! - XOA_PROXY_PORT: TCP port to listen on (default: 9001)
//! - XOA_PROXY_BIND: Address to bind to (default: 127.0.0.1)

use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub bind: String,
}

impl Config {
    /// Load configuration from environment variables.
    pub fn load() -> Self {
        let port = env::var("XOA_PROXY_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(9001);

        let bind = env::var("XOA_PROXY_BIND")
            .unwrap_or_else(|_| "127.0.0.1".to_string());

        Self { port, bind }
    }
}
