//! Authentication layer for incoming bridge connections.
//! Uses constant-time comparison of a pre-shared static token.

use anyhow::{bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const AUTH_PREAMBLE: &[u8; 4] = b"AUTH";

/// Reads the AUTH preamble + length-prefixed token from the stream,
/// verifies it against expected_token using constant-time comparison,
/// and sends OK\n or ERR ...\n.
pub async fn authenticate(
    stream: &mut (impl AsyncReadExt + AsyncWriteExt + Unpin),
    expected_token: &str,
) -> Result<()> {
    let mut preamble = [0u8; 4];
    stream.read_exact(&mut preamble).await?;

    if &preamble != AUTH_PREAMBLE {
        stream.write_all(b"ERR invalid auth preamble\n").await?;
        bail!("invalid auth preamble: {:?}", preamble);
    }

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let token_len = u32::from_le_bytes(len_buf) as usize;

    if token_len > 1024 {
        stream.write_all(b"ERR token too long\n").await?;
        bail!("token too long: {}", token_len);
    }

    let mut token_buf = vec![0u8; token_len];
    stream.read_exact(&mut token_buf).await?;
    let received_token = std::str::from_utf8(&token_buf)?;

    if !constant_time_eq(received_token.as_bytes(), expected_token.as_bytes()) {
        stream.write_all(b"ERR auth failed\n").await?;
        bail!("authentication failed");
    }

    stream.write_all(b"OK\n").await?;
    tracing::info!("client authenticated successfully");
    Ok(())
}

/// QUIC version: authenticate via bidi stream.
pub async fn authenticate_quic(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    expected_token: &str,
) -> anyhow::Result<()> {
    let mut preamble = [0u8; 4];
    recv.read_exact(&mut preamble).await?;
    if &preamble != b"AUTH" {
        send.write_all(b"ERR invalid auth preamble\n").await?;
        anyhow::bail!("invalid auth preamble: {:?}", preamble);
    }

    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let token_len = u32::from_le_bytes(len_buf) as usize;
    if token_len > 1024 {
        send.write_all(b"ERR token too long\n").await?;
        anyhow::bail!("token too long: {}", token_len);
    }

    let mut token_buf = vec![0u8; token_len];
    recv.read_exact(&mut token_buf).await?;
    let received_token = std::str::from_utf8(&token_buf)?;

    if !constant_time_eq(received_token.as_bytes(), expected_token.as_bytes()) {
        send.write_all(b"ERR auth failed\n").await?;
        anyhow::bail!("authentication failed");
    }

    send.write_all(b"OK\n").await?;
    tracing::info!("QUIC client authenticated successfully");
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"a", b"a"));
    }

    #[test]
    fn test_constant_time_eq_unequal() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
        assert!(!constant_time_eq(b"hello", b""));
    }
}
