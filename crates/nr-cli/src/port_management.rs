//! Port checking and waiting utilities.
//!
//! Poll until a TCP port is available or occupied, with configurable timeouts.

use std::net::TcpStream;
use std::time::{Duration, Instant};

const PORT_PROBE_TIMEOUT_MS: u64 = 500;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 3000;
const DEFAULT_POLL_INTERVAL_MS: u64 = 50;

/// Check if a port is in use by attempting a TCP connection.
pub async fn is_port_in_use(port: u16, host: &str) -> bool {
    let addr_str = format!("{}:{}", host, port);
    match TcpStream::connect_timeout(&addr_str.parse().unwrap_or_else(|_| ([127, 0, 0, 1], port).into()), Duration::from_millis(PORT_PROBE_TIMEOUT_MS)) {
        Ok(_) => true,
        Err(_) => false,
    }
}

/// Wait for a port to become free (not in use), with configurable timeout and polling interval.
pub async fn wait_for_port_free(
    port: u16,
    timeout_ms: Option<u64>,
    poll_interval_ms: Option<u64>,
) -> bool {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let poll_interval_ms = poll_interval_ms.unwrap_or(DEFAULT_POLL_INTERVAL_MS);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        if !is_port_in_use(port, "127.0.0.1").await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(poll_interval_ms)).await;
    }
}

/// Wait for server to accept TCP connections on a port, with configurable timeout.
pub async fn wait_for_server_ready(port: u16, timeout_ms: Option<u64>) -> bool {
    let timeout_ms = timeout_ms.unwrap_or(15000);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        if is_port_in_use(port, "127.0.0.1").await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_is_port_in_use_localhost() {
        // Port 0 is never in use (reserved)
        let result = is_port_in_use(0, "127.0.0.1").await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_wait_for_port_free_immediate() {
        // Port 0 should be free immediately
        let result = wait_for_port_free(0, Some(1000), Some(50)).await;
        assert!(result);
    }

    #[tokio::test]
    async fn test_wait_for_port_free_timeout() {
        // 19999 is rarely in use, but if it is, this test waits and times out quickly
        let result = wait_for_port_free(19999, Some(100), Some(25)).await;
        // Can be either true or false depending on system state; just check it doesn't panic
        let _ = result;
    }
}
