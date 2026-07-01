use anyhow::Result;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_yamux::StreamHandle;

const CHUNK_SIZE: usize = 1024 * 1024; // 1 MB pipeline buffer

pub async fn handle_upload_stream(mut stream: StreamHandle) -> Result<()> {
    let mut mode_buf = [0u8; 1];
    stream.read_exact(&mut mode_buf).await?;
    let mode = mode_buf[0];

    let mut path_len_buf = [0u8; 2];
    stream.read_exact(&mut path_len_buf).await?;
    let path_len = u16::from_le_bytes(path_len_buf) as usize;

    let mut path = vec![0u8; path_len];
    stream.read_exact(&mut path).await?;
    let remote_path = String::from_utf8_lossy(&path).to_string();

    let mut size_buf = [0u8; 8];
    stream.read_exact(&mut size_buf).await?;
    let _declared_size = u64::from_le_bytes(size_buf);

    if let Some(parent) = std::path::Path::new(&remote_path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    if mode == 0x02 && tokio::fs::metadata(&remote_path).await.is_ok() {
        send_upload_response(&mut stream, 0x01, 0, &[0u8; 32]).await?;
        return Ok(());
    }

    let tmp_path = format!("{}.tmp.{}", remote_path, std::process::id());
    let mut file = tokio::fs::File::create(&tmp_path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total: u64 = 0;

    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).await?;
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    file.flush().await?;
    drop(file);

    let hash: [u8; 32] = hasher.finalize().into();
    tokio::fs::rename(&tmp_path, &remote_path).await?;

    send_upload_response(&mut stream, 0x00, total, &hash).await?;
    tracing::info!("uploaded {} ({} bytes)", remote_path, total);
    Ok(())
}

pub async fn handle_download_stream(mut stream: StreamHandle) -> Result<()> {
    let mut path_len_buf = [0u8; 2];
    stream.read_exact(&mut path_len_buf).await?;
    let path_len = u16::from_le_bytes(path_len_buf) as usize;

    let mut path = vec![0u8; path_len];
    stream.read_exact(&mut path).await?;
    let remote_path = String::from_utf8_lossy(&path).to_string();

    let mut file = match tokio::fs::File::open(&remote_path).await {
        Ok(f) => f,
        Err(e) => {
            let msg = format!("failed to open: {}", e);
            stream.write_all(&[0x02]).await?;
            let msg_len = (msg.len() as u16).to_le_bytes();
            stream.write_all(&msg_len).await?;
            stream.write_all(msg.as_bytes()).await?;
            stream.shutdown().await?;
            return Ok(());
        }
    };

    let meta = file.metadata().await?;
    let file_size = meta.len();

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let hash: [u8; 32] = hasher.finalize().into();

    stream.write_all(&[0x00]).await?;
    stream.write_all(&file_size.to_le_bytes()).await?;
    stream.write_all(&hash).await?;

    let mut file = tokio::fs::File::open(&remote_path).await?;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n]).await?;
    }
    stream.shutdown().await?;

    tracing::info!("downloaded {} ({} bytes)", remote_path, file_size);
    Ok(())
}

async fn send_upload_response(
    stream: &mut StreamHandle,
    code: u8,
    written: u64,
    sha256: &[u8; 32],
) -> Result<()> {
    stream.write_all(&[code]).await?;
    stream.write_all(&written.to_le_bytes()).await?;
    stream.write_all(sha256).await?;
    stream.shutdown().await?;
    Ok(())
}

pub async fn handle_batch_upload_stream(mut stream: StreamHandle) -> Result<()> {
    let mut mode_buf = [0u8; 1];
    stream.read_exact(&mut mode_buf).await?;
    let mode = mode_buf[0];

    let mut count_buf = [0u8; 2];
    stream.read_exact(&mut count_buf).await?;
    let file_count = u16::from_le_bytes(count_buf) as usize;

    let mut results: Vec<(u8, u64, [u8; 32])> = Vec::with_capacity(file_count);

    for _ in 0..file_count {
        let (status, written, hash) = batch_upload_one(&mut stream, mode).await;
        results.push((status, written, hash));
    }

    let n = results.len() as u16;
    stream.write_all(&n.to_le_bytes()).await?;
    for (status, written, hash) in &results {
        stream.write_all(&[*status]).await?;
        stream.write_all(&written.to_le_bytes()).await?;
        stream.write_all(hash).await?;
    }
    stream.shutdown().await?;
    Ok(())
}

async fn batch_upload_one(stream: &mut StreamHandle, mode: u8) -> (u8, u64, [u8; 32]) {
    let mut path_len_buf = [0u8; 2];
    if stream.read_exact(&mut path_len_buf).await.is_err() {
        return (0x02, 0, [0u8; 32]);
    }
    let path_len = u16::from_le_bytes(path_len_buf) as usize;

    let mut path = vec![0u8; path_len];
    if stream.read_exact(&mut path).await.is_err() {
        return (0x02, 0, [0u8; 32]);
    }
    let remote_path = String::from_utf8_lossy(&path).to_string();

    let mut size_buf = [0u8; 8];
    if stream.read_exact(&mut size_buf).await.is_err() {
        return (0x02, 0, [0u8; 32]);
    }
    let file_size = u64::from_le_bytes(size_buf) as usize;

    if let Some(parent) = std::path::Path::new(&remote_path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
    }

    if mode == 0x02 && tokio::fs::metadata(&remote_path).await.is_ok() {
        return drain_exact(stream, file_size).await.map_or(
            (0x02, 0, [0u8; 32]),
            |_| (0x01, 0, [0u8; 32]),
        );
    }

    let tmp_path = format!("{}.tmp.{}", remote_path, std::process::id());
    let mut file = match tokio::fs::File::create(&tmp_path).await {
        Ok(f) => f,
        Err(_) => {
            let _ = drain_exact(stream, file_size).await;
            return (0x02, 0, [0u8; 32]);
        }
    };

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut remaining = file_size;

    while remaining > 0 {
        let n = remaining.min(CHUNK_SIZE);
        if stream.read_exact(&mut buf[..n]).await.is_err() {
            return (0x02, 0, [0u8; 32]);
        }
        let _ = file.write_all(&buf[..n]).await;
        hasher.update(&buf[..n]);
        remaining -= n;
    }
    let _ = file.flush().await;
    drop(file);

    let hash: [u8; 32] = hasher.finalize().into();
    let _ = tokio::fs::rename(&tmp_path, &remote_path).await;
    tracing::info!("batch uploaded {} ({} bytes)", remote_path, file_size);
    (0x00, file_size as u64, hash)
}

async fn drain_exact(stream: &mut StreamHandle, size: usize) -> Result<()> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut remaining = size;
    while remaining > 0 {
        let n = remaining.min(CHUNK_SIZE);
        stream.read_exact(&mut buf[..n]).await?;
        remaining -= n;
    }
    Ok(())
}

async fn drain_file_data(stream: &mut StreamHandle) -> Result<()> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return Ok(()),
            _ => {}
        }
    }
}

// ─── QUIC stream handlers ───

/// QUIC stream dispatcher: read stream type byte, route to handler.
pub async fn handle_quic_stream(
    send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> anyhow::Result<()> {
    let mut type_buf = [0u8; 1];
    recv.read_exact(&mut type_buf).await?;
    match type_buf[0] {
        0x02 => handle_upload_quic(send, recv).await,
        0x03 => handle_download_quic(send, recv).await,
        t => {
            tracing::warn!("unknown QUIC stream type: 0x{:02x}", t);
            Ok(())
        }
    }
}

async fn handle_upload_quic(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> anyhow::Result<()> {
    let mut mode_buf = [0u8; 1];
    recv.read_exact(&mut mode_buf).await?;
    let mode = mode_buf[0];

    let mut path_len_buf = [0u8; 2];
    recv.read_exact(&mut path_len_buf).await?;
    let path_len = u16::from_le_bytes(path_len_buf) as usize;
    let mut path = vec![0u8; path_len];
    recv.read_exact(&mut path).await?;
    let remote_path = String::from_utf8_lossy(&path).to_string();

    let mut size_buf = [0u8; 8];
    recv.read_exact(&mut size_buf).await?;
    let _declared_size = u64::from_le_bytes(size_buf);

    if let Some(parent) = std::path::Path::new(&remote_path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    // skip mode: file exists → skip
    if mode == 0x02 && tokio::fs::metadata(&remote_path).await.is_ok() {
        send.write_all(&[0x01]).await?;
        send.write_all(&0u64.to_le_bytes()).await?;
        send.write_all(&[0u8; 32]).await?;
        send.finish()?;
        return Ok(());
    }

    let tmp_path = format!("{}.tmp.{}", remote_path, std::process::id());
    let mut file = tokio::fs::File::create(&tmp_path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total: u64 = 0;

    loop {
        let n = match recv.read(&mut buf).await? {
            Some(0) | None => break,
            Some(n) => n,
        };
        file.write_all(&buf[..n]).await?;
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    file.flush().await?;
    drop(file);

    let hash: [u8; 32] = hasher.finalize().into();
    tokio::fs::rename(&tmp_path, &remote_path).await?;

    send.write_all(&[0x00]).await?;
    send.write_all(&total.to_le_bytes()).await?;
    send.write_all(&hash).await?;
    send.finish()?;
    tracing::info!("QUIC uploaded {} ({} bytes)", remote_path, total);
    Ok(())
}

async fn handle_download_quic(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> anyhow::Result<()> {
    let mut path_len_buf = [0u8; 2];
    recv.read_exact(&mut path_len_buf).await?;
    let path_len = u16::from_le_bytes(path_len_buf) as usize;
    let mut path = vec![0u8; path_len];
    recv.read_exact(&mut path).await?;
    let remote_path = String::from_utf8_lossy(&path).to_string();

    let mut file = match tokio::fs::File::open(&remote_path).await {
        Ok(f) => f,
        Err(e) => {
            let msg = format!("failed to open: {}", e);
            send.write_all(&[0x02]).await?;
            let msg_len = (msg.len() as u16).to_le_bytes();
            send.write_all(&msg_len).await?;
            send.write_all(msg.as_bytes()).await?;
            send.finish()?;
            return Ok(());
        }
    };

    let meta = file.metadata().await?;
    let file_size = meta.len();

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    let hash: [u8; 32] = hasher.finalize().into();

    send.write_all(&[0x00]).await?;
    send.write_all(&file_size.to_le_bytes()).await?;
    send.write_all(&hash).await?;

    let mut file = tokio::fs::File::open(&remote_path).await?;
    tokio::io::copy(&mut file, &mut send).await?;
    send.finish()?;
    tracing::info!("QUIC downloaded {} ({} bytes)", remote_path, file_size);
    Ok(())
}
