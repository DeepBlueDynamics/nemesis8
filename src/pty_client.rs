//! Remote `n8 shell <agent>` / `n8 attach <agent>` — a terminal over the
//! gateway's PTY WebSocket.
//!
//! Pure gateway client (no Docker here). Opens
//! `GET /agents/{agent}/pty?mode=shell|attach&cols=N&rows=N` (bearer on the
//! upgrade), puts this terminal in raw mode, and pumps bytes:
//!
//! - WS **Binary** = terminal bytes both ways (keystrokes → container stdin,
//!   container output → this stdout).
//! - WS **Text**, client→server `{"resize":{"cols":N,"rows":N}}` when the
//!   terminal size changes (polled every 500 ms); server→client
//!   `{"exit":<int|null>}` before it closes.
//!
//! Detach: `Ctrl-]` then `q`. `Ctrl-]` followed by anything else is forwarded
//! verbatim (both bytes), so the sequence can't swallow real input.
//!
//! The gateway execs `bash -l`/`sh` for `shell`, and attaches to the agent's
//! TTY (or execs the provider's interactive command) for `attach`. Closing the
//! WebSocket ends an exec session; an attach to the main TTY only detaches.

use anyhow::{Context, Result};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::io::Write;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyMode {
    Shell,
    Attach,
}

impl PtyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PtyMode::Shell => "shell",
            PtyMode::Attach => "attach",
        }
    }
}

/// `Ctrl-]` — the detach prefix.
pub const DETACH_PREFIX: u8 = 0x1D;
/// How often the terminal size is polled for a resize.
const SIZE_POLL: Duration = Duration::from_millis(500);

/// Detach-key filter over the raw stdin byte stream. Carries a lone trailing
/// `Ctrl-]` across chunks so the sequence works even when the two keys arrive
/// in separate reads.
#[derive(Debug, Default)]
pub struct DetachFilter {
    pending: bool,
}

impl DetachFilter {
    /// Filter one chunk. Returns the bytes to forward and whether the detach
    /// sequence completed (bytes before it are still forwarded).
    pub fn feed(&mut self, chunk: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(chunk.len() + 1);
        let mut i = 0;
        while i < chunk.len() {
            let b = chunk[i];
            if self.pending {
                self.pending = false;
                if b == b'q' {
                    return (out, true);
                }
                out.push(DETACH_PREFIX);
                out.push(b);
                i += 1;
                continue;
            }
            if b == DETACH_PREFIX {
                match chunk.get(i + 1) {
                    Some(b'q') => return (out, true),
                    Some(&next) => {
                        out.push(b);
                        out.push(next);
                        i += 2;
                    }
                    None => {
                        self.pending = true;
                        i += 1;
                    }
                }
                continue;
            }
            out.push(b);
            i += 1;
        }
        (out, false)
    }
}

/// Client→server resize control frame.
pub fn resize_frame(cols: u16, rows: u16) -> String {
    serde_json::json!({ "resize": { "cols": cols, "rows": rows } }).to_string()
}

/// Server→client exit frame: `Some(Some(code))` for `{"exit":7}`,
/// `Some(None)` for `{"exit":null}`, `None` if this isn't an exit frame.
pub fn parse_exit(text: &str) -> Option<Option<i32>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = v.as_object()?;
    if !obj.contains_key("exit") {
        return None;
    }
    match &obj["exit"] {
        serde_json::Value::Null => Some(None),
        serde_json::Value::Number(n) => Some(n.as_i64().map(|c| c as i32)),
        _ => None,
    }
}

/// Raw-mode (and, for a TUI, alternate-screen) guard. Restores on every exit
/// path — normal return, `?` errors, and unwinding — via `Drop`.
struct TermGuard {
    alt: bool,
}

impl TermGuard {
    fn enter(alt: bool) -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("putting the terminal in raw mode")?;
        if alt {
            if let Err(e) = crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen) {
                let _ = crossterm::terminal::disable_raw_mode();
                return Err(e).context("entering the alternate screen");
            }
        }
        Ok(Self { alt })
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        if self.alt {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        }
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = std::io::stdout().flush();
    }
}

/// How a pump ended.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PumpOutcome {
    /// `Some(code)` when the server sent `{"exit":code}`; `None` for
    /// `{"exit":null}` or no exit frame at all.
    pub exit: Option<i32>,
    /// The user pressed the detach sequence.
    pub detached: bool,
}

/// Pump one PTY WebSocket against a stdin byte channel and a stdout writer
/// until either side ends or the user detaches. `size_of()` is polled every
/// 500 ms; a change is sent as a resize control frame. Generic over the
/// socket so a test can drive it against a local server.
pub async fn pump<S, W, F>(
    ws: S,
    mut stdin_rx: mpsc::Receiver<Vec<u8>>,
    mut size: (u16, u16),
    size_of: F,
    out: &mut W,
) -> PumpOutcome
where
    S: Stream<Item = std::result::Result<Message, WsError>>
        + Sink<Message, Error = WsError>
        + Unpin
        + Send
        + 'static,
    W: Write,
    F: Fn() -> Option<(u16, u16)>,
{
    let (mut sink, mut stream) = ws.split();
    // One writer owns the sink; keystrokes, resizes, Pongs and Close all go
    // through it, so the Pong reply never contends with a keystroke.
    let (tx, mut rx) = mpsc::channel::<Message>(64);
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let closing = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() || closing {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut filter = DetachFilter::default();
    let mut outcome = PumpOutcome::default();
    let mut ticker = tokio::time::interval(SIZE_POLL);
    ticker.tick().await; // first tick fires immediately — skip it

    loop {
        tokio::select! {
            chunk = stdin_rx.recv() => match chunk {
                Some(bytes) => {
                    let (fwd, detach) = filter.feed(&bytes);
                    if !fwd.is_empty() && tx.send(Message::Binary(fwd.into())).await.is_err() {
                        break;
                    }
                    if detach {
                        outcome.detached = true;
                        let _ = tx.send(Message::Close(None)).await;
                        break;
                    }
                }
                None => {
                    // stdin closed (EOF / pipe) — end the session.
                    let _ = tx.send(Message::Close(None)).await;
                    break;
                }
            },
            item = stream.next() => match item {
                Some(Ok(Message::Binary(b))) => {
                    if out.write_all(&b).is_err() {
                        break;
                    }
                    let _ = out.flush();
                }
                Some(Ok(Message::Text(t))) => {
                    if let Some(code) = parse_exit(t.as_str()) {
                        outcome.exit = code;
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    if tx.send(Message::Pong(p)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {} // Pong / raw frames
            },
            _ = ticker.tick() => {
                if let Some(now) = size_of() {
                    if now != size {
                        size = now;
                        if tx.send(Message::Text(resize_frame(now.0, now.1).into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
    drop(tx);
    // Give the writer a moment to flush the Close, then make sure it's gone.
    let _ = tokio::time::timeout(Duration::from_millis(300), writer).await;
    outcome
}

/// Run a remote shell/attach session. Returns the process exit code: the
/// server-reported exit code, 0 on detach, 2 when the session could not be
/// opened (unreachable gateway, rejected token, unknown agent).
pub async fn run(remote: &str, token: Option<&str>, agent: &str, mode: PtyMode) -> Result<i32> {
    let remote = remote.trim().trim_end_matches('/');
    let size = crossterm::terminal::size().unwrap_or((80, 24));
    let url = format!(
        "{}/agents/{}/pty?mode={}&cols={}&rows={}",
        crate::connect::ws_base(remote),
        agent,
        mode.as_str(),
        size.0,
        size.1
    );
    let mut request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("building the WebSocket request for {url}"))?;
    if let Some(t) = token {
        let v = HeaderValue::from_str(&format!("Bearer {t}")).context("bearer token header")?;
        request.headers_mut().insert(AUTHORIZATION, v);
    }

    let (ws, _resp) = match tokio_tungstenite::connect_async(request).await {
        Ok(v) => v,
        Err(WsError::Http(resp)) => {
            let status = resp.status().as_u16();
            match status {
                401 => eprintln!("gateway rejected the token (check --token / NEMESIS8_TOKEN)"),
                404 => eprintln!(
                    "no running agent '{agent}' on {remote} (see `n8 agents list --remote {remote}`)"
                ),
                400 => eprintln!("gateway refused the PTY request (HTTP 400 — bad mode?)"),
                s => eprintln!("gateway refused the PTY WebSocket (HTTP {s})"),
            }
            return Ok(2);
        }
        Err(e) => {
            eprintln!("cannot open the PTY WebSocket to {remote}: {e}");
            eprintln!("  is `n8 serve` running there, and is --remote / NEMESIS8_REMOTE right?");
            return Ok(2);
        }
    };

    println!("[remote {} on {agent} via {remote} — Ctrl-] then q to detach]", mode.as_str());
    let _ = std::io::stdout().flush();
    let guard = TermGuard::enter(mode == PtyMode::Attach)?;

    // stdin on a plain thread: a blocking read can't be cancelled, and tokio's
    // runtime shutdown would wait on a spawn_blocking task forever. A detached
    // thread just dies with the process.
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut stdout = std::io::stdout();
    let outcome = pump(ws, stdin_rx, size, || crossterm::terminal::size().ok(), &mut stdout).await;
    drop(guard);

    if outcome.detached {
        println!("\r\n[detached]");
        return Ok(0);
    }
    Ok(outcome.exit.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn detach_sequence_in_one_chunk() {
        let mut f = DetachFilter::default();
        assert_eq!(f.feed(b"ab\x1dq"), (b"ab".to_vec(), true));
        // Prefix followed by something else forwards both bytes.
        let mut f = DetachFilter::default();
        assert_eq!(f.feed(b"\x1dxyz"), (b"\x1dxyz".to_vec(), false));
        // Plain bytes untouched.
        let mut f = DetachFilter::default();
        assert_eq!(f.feed(b"hello"), (b"hello".to_vec(), false));
    }

    #[test]
    fn detach_prefix_held_across_chunks() {
        let mut f = DetachFilter::default();
        assert_eq!(f.feed(b"ls\x1d"), (b"ls".to_vec(), false), "lone prefix is held");
        assert_eq!(f.feed(b"q"), (Vec::new(), true), "completes on the next chunk");
        let mut f = DetachFilter::default();
        assert_eq!(f.feed(b"\x1d"), (Vec::new(), false));
        assert_eq!(f.feed(b"x"), (b"\x1dx".to_vec(), false), "held prefix + other byte forwarded");
        assert_eq!(f.feed(b"q"), (b"q".to_vec(), false), "a plain q afterwards is just a q");
    }

    #[test]
    fn control_frames_encode_and_decode() {
        assert_eq!(resize_frame(120, 40), r#"{"resize":{"cols":120,"rows":40}}"#);
        assert_eq!(parse_exit(r#"{"exit":7}"#), Some(Some(7)));
        assert_eq!(parse_exit(r#"{"exit":null}"#), Some(None));
        assert_eq!(parse_exit(r#"{"resize":{"cols":1,"rows":1}}"#), None);
        assert_eq!(parse_exit("not json"), None);
        assert_eq!(parse_exit(r#"{"exit":"7"}"#), None);
    }

    #[tokio::test]
    async fn pump_round_trips_bytes_and_returns_server_exit_code() {
        // "Gateway": echoes Binary frames; after the first one it reports
        // exit 7 and closes.
        let srv = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = srv.local_addr().unwrap();
        tokio::spawn(async move {
            let (s, _) = srv.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(s).await.unwrap();
            let (mut sink, mut stream) = ws.split();
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Binary(b) = msg {
                    sink.send(Message::Binary(b)).await.unwrap();
                    sink.send(Message::Text(r#"{"exit":7}"#.into())).await.unwrap();
                    sink.send(Message::Close(None)).await.unwrap();
                    break;
                }
            }
        });

        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/agents/x/pty?mode=shell"))
            .await
            .unwrap();
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(4);
        stdin_tx.send(b"echo hi\r".to_vec()).await.unwrap();
        let mut out: Vec<u8> = Vec::new();
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            pump(ws, stdin_rx, (80, 24), || Some((80, 24)), &mut out),
        )
        .await
        .expect("pump finishes within 5s");
        assert_eq!(out, b"echo hi\r".to_vec(), "container output reached stdout");
        assert_eq!(outcome, PumpOutcome { exit: Some(7), detached: false });
    }

    #[tokio::test]
    async fn pump_detaches_on_ctrl_bracket_q_and_sends_close() {
        let srv = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = srv.local_addr().unwrap();
        let got_close = tokio::spawn(async move {
            let (s, _) = srv.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
            let mut saw_bytes = Vec::new();
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    Message::Binary(b) => saw_bytes.extend_from_slice(&b),
                    Message::Close(_) => return (saw_bytes, true),
                    _ => {}
                }
            }
            (saw_bytes, false)
        });

        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/agents/x/pty?mode=attach"))
            .await
            .unwrap();
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(4);
        stdin_tx.send(b"ab\x1dq".to_vec()).await.unwrap();
        let mut out: Vec<u8> = Vec::new();
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            pump(ws, stdin_rx, (80, 24), || Some((80, 24)), &mut out),
        )
        .await
        .expect("pump finishes within 5s");
        assert!(outcome.detached);
        assert_eq!(outcome.exit, None);
        let (bytes, closed) = tokio::time::timeout(Duration::from_secs(5), got_close)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"ab".to_vec(), "bytes before the detach sequence are forwarded");
        assert!(closed, "the server saw a clean Close");
    }
}
