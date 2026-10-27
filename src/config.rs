//! Configuration — parsed once at startup from environment variables if exist.
//!
//! Variables:
//! - XOA_PROXY_PORT: TCP port to listen on (default: 9001)
//! - XOA_PROXY_BIND: Address to bind to (default: 127.0.0.1)
//! - XOA_PROXY_IDLE_SECONDS: idle exit delay in socket mode (default: 60, minimum: 1)

use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub bind: String,
    pub idle_timeout: std::time::Duration,
}

impl Config {
    /// Load configuration from environment variables.
    pub fn load() -> Self {
        let port = env::var("XOA_PROXY_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(9001);

        let bind = env::var("XOA_PROXY_BIND").unwrap_or_else(|_| "127.0.0.1".to_string());

        let idle_seconds = env::var("XOA_PROXY_IDLE_SECONDS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(60);

        Self {
            port,
            bind,
            idle_timeout: std::time::Duration::from_secs(idle_seconds),
        }
    }
}
