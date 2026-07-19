pub mod ai_panel;
pub mod keymap;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use std::os::unix::io::AsRawFd;

use agent_ops_core::HostConfig;
use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::Terminal;
use ratatui_crossterm::CrosstermBackend;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::connect::{connect_to_bridge_quic, translate_key_to_bytes};
use crate::protocol::{
    read_attached_response, recv_json_frame, send_json_frame, write_attach_request, write_detach,
    write_resize,
};

use self::ai_panel::{AiPanel, Message, Role};
use self::keymap::Action;

// ── Helper functions ──

async fn capture_pane(
    send: &Arc<Mutex<quinn::SendStream>>,
    recv: &Arc<Mutex<quinn::RecvStream>>,
    session: &str,
    pane: &str,
    max_lines: usize,
) -> Result<String> {
    let mut s = send.lock().await;
    send_json_frame(
        &mut s,
        &serde_json::json!({
            "type": "capture_pane",
            "session_name": session,
            "pane_id": pane,
            "max_lines": max_lines,
        }),
    )
    .await?;
    drop(s);
    let mut r = recv.lock().await;
    let resp = recv_json_frame(&mut r).await?;
    Ok(resp["text"].as_str().unwrap_or("").to_string())
}

// ── AI handlers ──

async fn handle_report(
    json_send: &Arc<Mutex<quinn::SendStream>>,
    json_recv: &Arc<Mutex<quinn::RecvStream>>,
    session_name: &str,
    ai_panel: &AiPanel,
) -> Result<()> {
    let ctx = capture_pane(json_send, json_recv, session_name, "%0", 50).await?;
    *ai_panel.thinking.lock().await = true;

    let prompt = format!(
        "Analyze this terminal output and provide insights:\n```\n{}\n```",
        ctx
    );
    let ai = ai_panel.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::ai::ask_opencode(&prompt, &ai).await {
            ai.add_message(Message {
                role: Role::System,
                content: format!("AI error: {}", e),
                code_blocks: vec![],
            })
            .await;
        }
        *ai.thinking.lock().await = false;
    });

    Ok(())
}

async fn handle_clear(ai_panel: &AiPanel) {
    ai_panel.clear().await;
    crate::ai::reset_session().await;
    ai_panel
        .add_message(Message {
            role: Role::System,
            content: "Conversation cleared.".to_string(),
            code_blocks: vec![],
        })
        .await;
}

// ── Ratatui rendering ──

// ── AI Mode (Alternate Screen) ──

async fn ai_loop(
    json_send: &Arc<Mutex<quinn::SendStream>>,
    json_recv: &Arc<Mutex<quinn::RecvStream>>,
    _pty_buffer: &Arc<Mutex<Vec<String>>>,
    ai_panel: &AiPanel,
    session_name: &str,
) -> Result<()> {
    let mut stdout = std::io::stdout();
    stdout.execute(crossterm::terminal::EnterAlternateScreen)?;
    stdout.execute(crossterm::event::EnableMouseCapture)?;

    // Suppress stderr during AI panel to prevent SDK internal logs from
    // bleeding into the alternate screen TUI.
    let saved_stderr = unsafe { libc::dup(2) };
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
    unsafe { libc::dup2(null.as_raw_fd(), 2) };

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut event_stream = EventStream::new();
    let mut msg_scroll: usize = 0;

    loop {
        // Redraw
        let draw_result = terminal.draw(|f| {
            ai_panel.render(f, f.area(), true, msg_scroll);
        });

        if let Err(e) = draw_result {
            tracing::warn!("draw error: {}", e);
        }

        // Wait for event (with timeout to allow background updates to show)
        let event_opt = tokio::time::timeout(Duration::from_millis(100), event_stream.next()).await;
        let event = match event_opt {
            Ok(Some(Ok(e))) => e,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => continue,
        };

        match event {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => {
                        stdout.execute(crossterm::terminal::LeaveAlternateScreen)?;
                        stdout.execute(crossterm::event::DisableMouseCapture)?;
                        unsafe {
                            libc::dup2(saved_stderr, 2);
                            libc::close(saved_stderr);
                        }
                        return Ok(());
                    }
                    KeyCode::Char('g') if ctrl => {
                        stdout.execute(crossterm::terminal::LeaveAlternateScreen)?;
                        stdout.execute(crossterm::event::DisableMouseCapture)?;
                        unsafe {
                            libc::dup2(saved_stderr, 2);
                            libc::close(saved_stderr);
                        }
                        return Ok(());
                    }
                    KeyCode::Enter => {
                        if *ai_panel.thinking.lock().await {
                            continue;
                        }
                        let text = ai_panel.input.lock().await.clone();
                        if !text.is_empty() {
                            ai_panel.input.lock().await.clear();
                            drop(ai_panel.input.lock().await);

                            let cmd = text.clone();

                            // 有待回答的问题 → 回复 AI
                            if ai_panel.pending_question().await.is_some() {
                                let a = ai_panel.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = crate::ai::answer_question(&a, &cmd).await {
                                        a.add_message(Message {
                                            role: Role::System,
                                            content: format!("回复失败: {}", e),
                                            code_blocks: vec![],
                                        })
                                        .await;
                                    }
                                });
                                continue;
                            }

                            if cmd.starts_with("@analyze") {
                                handle_report(json_send, json_recv, session_name, ai_panel)
                                    .await
                                    .ok();
                            } else if cmd.starts_with("@clear") {
                                handle_clear(ai_panel).await;
                            } else {
                                ai_panel
                                    .add_message(Message {
                                        role: Role::User,
                                        content: cmd.clone(),
                                        code_blocks: vec![],
                                    })
                                    .await;
                                *ai_panel.thinking.lock().await = true;

                                let a = ai_panel.clone();
                                let task = cmd;
                                tokio::spawn(async move {
                                    if let Err(e) = crate::ai::ask_opencode(&task, &a).await {
                                        a.add_message(Message {
                                            role: Role::System,
                                            content: format!("AI error: {}", e),
                                            code_blocks: vec![],
                                        })
                                        .await;
                                    }
                                    *a.thinking.lock().await = false;
                                });
                            }
                        }
                    }
                    KeyCode::Char(c) => {
                        ai_panel.input.lock().await.push(c);
                    }
                    KeyCode::Backspace => {
                        ai_panel.input.lock().await.pop();
                    }
                    KeyCode::PageUp | KeyCode::Up => {
                        msg_scroll = msg_scroll.saturating_sub(3);
                    }
                    KeyCode::PageDown | KeyCode::Down => {
                        msg_scroll = msg_scroll.saturating_add(3);
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => msg_scroll = msg_scroll.saturating_add(3),
                MouseEventKind::ScrollUp => msg_scroll = msg_scroll.saturating_sub(3),
                _ => {}
            },
            Event::Resize(_, _) => {
                // Terminal will adjust on next draw
            }
            _ => {}
        }
    }
}

// ── PTY Mode (Main Screen — raw passthrough) ──

pub async fn run_connect_with_ai(
    config: &HostConfig,
    ca_cert_path: &str,
    session_name: &str,
    pane_id: &str,
    readonly: bool,
) -> Result<()> {
    let conn =
        connect_to_bridge_quic(&config.bridge_addr, &config.bridge_token, ca_cert_path).await?;

    // JSON channel
    let (mut json_send_raw, json_recv_raw) = conn.open_bi().await?;
    json_send_raw.write_all(&[0x01]).await?;
    let json_send = Arc::new(Mutex::new(json_send_raw));
    let json_recv = Arc::new(Mutex::new(json_recv_raw));

    // PTY attach (ctrl stream)
    let (cols, rows) = crossterm::terminal::size()?;
    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;
    ctrl_send.write_all(&[0x06]).await?;
    write_attach_request(&mut ctrl_send, session_name, pane_id, cols, rows).await?;
    let _scrollback = read_attached_response(&mut ctrl_recv).await?;
    let ctrl_send = Arc::new(Mutex::new(ctrl_send));

    enable_raw_mode()?;

    // PTY data stream
    let (mut pty_send_raw, mut pty_recv_raw) = conn.open_bi().await?;
    pty_send_raw.write_all(&[0x07]).await?;
    let pty_send = Arc::new(Mutex::new(pty_send_raw));

    // AI panel (shared between modes)
    let ai = AiPanel::new();
    ai.add_message(Message {
        role: Role::System,
        content: "Ctrl+G AI | @analyze | @clear | Esc back".to_string(),
        code_blocks: vec![],
    })
    .await;

    // Shared state between PTY mode and AI mode
    let is_ai_mode = Arc::new(AtomicBool::new(false));
    let pty_buffer: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // PTY reader task: reads PTY output continuously
    // In PTY mode: writes to stdout (raw passthrough) + updates buffer
    // In AI mode: only updates buffer (ratatui handles display)
    let pty_reader = {
        let mode_flag = is_ai_mode.clone();
        let buffer = pty_buffer.clone();
        tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            let mut buf = [0u8; 4096];
            let mut pending = String::new();
            while let Ok(Some(n)) = pty_recv_raw.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                // Update line buffer
                let text = String::from_utf8_lossy(&buf[..n]);
                pending.push_str(&text);
                {
                    let mut lines = buffer.lock().await;
                    while let Some(pos) = pending.find('\n') {
                        let line = pending[..=pos].to_string();
                        pending = pending[pos + 1..].to_string();
                        if lines.len() >= 2000 {
                            lines.remove(0);
                        }
                        lines.push(line);
                    }
                }
                // Write to stdout in PTY mode only
                if !mode_flag.load(Ordering::Relaxed) {
                    stdout.write_all(&buf[..n]).await?;
                    stdout.flush().await?;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
    };

    let mut event_stream = EventStream::new();

    // PTY mode event loop
    loop {
        let event_opt = if is_ai_mode.load(Ordering::Relaxed) {
            // AI mode — enter alternate screen
            tokio::io::stdout().flush().await.ok();
            let result = ai_loop(&json_send, &json_recv, &pty_buffer, &ai, session_name).await;
            is_ai_mode.store(false, Ordering::Relaxed);
            if result.is_err() {
                break;
            }
            // Back from AI mode — just continue in PTY mode
            continue;
        } else {
            match event_stream.next().await {
                Some(Ok(event)) => Some(event),
                _ => break,
            }
        };

        if let Some(event) = event_opt {
            match event {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    let action = keymap::classify(&key);

                    match action {
                        Action::AskQuestion => {
                            // Enter AI mode
                            is_ai_mode.store(true, Ordering::Relaxed);
                            // loop will handle it on next iteration
                        }
                        Action::Detach => break,
                        Action::ClearHistory => {
                            handle_clear(&ai).await;
                        }
                        Action::Noop | Action::ForwardToPty(_) => {
                            if !readonly {
                                let bytes = translate_key_to_bytes(key);
                                if !bytes.is_empty() {
                                    let mut s = pty_send.lock().await;
                                    if s.write_all(&bytes).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                Event::Resize(cols, rows) => {
                    let mut cs = ctrl_send.lock().await;
                    write_resize(&mut cs, cols, rows).await.ok();
                }
                _ => {}
            }
        }
    }

    // Cleanup
    disable_raw_mode()?;
    pty_reader.abort();
    write_detach(&mut *ctrl_send.lock().await).await.ok();
    Ok(())
}
