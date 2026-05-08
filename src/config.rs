//! Configuration — parsed once at startup from CLI flags and environment variables.
//! Priority (highest to lowest): CLI flag → env var → compiled-in default.

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "xoa-proxy",
    version,
    after_help = "\
Environment variables (override defaults; CLI flags take precedence):
  XOA_PROXY_VERIFY_SSL=1      Enable (value = 1) / Disable (value = 0 ) SSL certificate verification (default: 1)"
)]
pub struct Config {
    /// TCP port to listen on.
    /// 9001 avoids clashes with Vite dev (3000) and XAPI (443/80).
    #[arg(long, env = "XOA_PROXY_PORT", default_value_t = 9001)]
    pub port: u16,

    /// Address to bind to. Loopback-only by default: XAPI is co-located,
    /// so there is no reason to expose this to the network.
    #[arg(long, env = "XOA_PROXY_BIND", default_value = "127.0.0.1")]
    pub bind: String,
}
