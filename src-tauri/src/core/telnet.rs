//! Telnet session: raw TCP with basic IAC negotiation, bridged to the session manager.

use super::session::{
    SessionCommand, SessionHandle, SessionInfo, SessionManager, SessionType, SharedCwd,
};
use super::zmodem::{
    ZmodemAction, ZmodemDetectResult, ZmodemDetector, ZmodemDirection, ZmodemEvent, ZmodemTransfer,
    start_zmodem_transfer,
};
use crate::app_event::emit_session_closed;
use crate::config::AiExecutionProfile;
use crate::core::capture::OutputCaptureProcessor;
use crate::core::input::remap_del_to_bs;
use crate::core::{RecordingManager, SessionOutputCoalescer};
use crate::error::AppResult;
use crate::observability::{StructuredLog, StructuredLogLevel, log_event, log_rate_limited};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex as TokioMutex, mpsc};

const IAC: u8 = 255;
const WILL: u8 = 251;
const WONT: u8 = 252;
const DO: u8 = 253;
const DONT: u8 = 254;
const SB: u8 = 250;
const SE: u8 = 240;

const OPT_ECHO: u8 = 1;
const OPT_SUPPRESS_GO_AHEAD: u8 = 3;
const OPT_NAWS: u8 = 31;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelnetEnterMode {
    Crlf,
    Cr,
    Lf,
}

impl Default for TelnetEnterMode {
    fn default() -> Self {
        Self::Cr
    }
}

impl TelnetEnterMode {
    pub fn from_config_value(value: &str) -> Self {
        match value {
            "crlf" => Self::Crlf,
            "lf" => Self::Lf,
            _ => Self::Cr,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TelnetSessionConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub backspace_mode: String,
    pub raw_tcp_cli: bool,
    pub enter_mode: TelnetEnterMode,
    pub local_echo: bool,
    pub local_line_edit: bool,
    pub force_character_at_a_time: bool,
    pub send_naws: bool,
    pub send_sga: bool,
}

impl Default for TelnetSessionConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 23,
            name: "Telnet".to_string(),
            backspace_mode: "del".to_string(),
            raw_tcp_cli: false,
            enter_mode: TelnetEnterMode::Cr,
            local_echo: false,
            local_line_edit: false,
            force_character_at_a_time: false,
            send_naws: true,
            send_sga: true,
        }
    }
}

/// Respond to a Telnet option negotiation request.
fn negotiate_response(command: u8, option: u8, send_naws: bool, send_sga: bool) -> Vec<u8> {
    match command {
        WILL => {
            if option == OPT_ECHO || (send_sga && option == OPT_SUPPRESS_GO_AHEAD) {
                vec![IAC, DO, option]
            } else {
                vec![IAC, DONT, option]
            }
        }
        DO => {
            if send_naws && option == OPT_NAWS {
                vec![IAC, WILL, option]
            } else {
                vec![IAC, WONT, option]
            }
        }
        WONT => vec![IAC, DONT, option],
        DONT => vec![IAC, WONT, option],
        _ => vec![],
    }
}

/// Build a NAWS (Negotiate About Window Size) subnegotiation sequence.
fn build_naws(cols: u16, rows: u16) -> Vec<u8> {
    vec![
        IAC,
        SB,
        OPT_NAWS,
        (cols >> 8) as u8,
        (cols & 0xff) as u8,
        (rows >> 8) as u8,
        (rows & 0xff) as u8,
        IAC,
        SE,
    ]
}

fn maybe_build_naws(cols: u16, rows: u16, config: &TelnetSessionConfig) -> Option<Vec<u8>> {
    if config.raw_tcp_cli || !config.send_naws {
        None
    } else {
        Some(build_naws(cols, rows))
    }
}

fn unescape_iac_iac(data: &[u8]) -> Vec<u8> {
    let mut visible = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == IAC && i + 1 < data.len() && data[i + 1] == IAC {
            visible.push(IAC);
            i += 2;
        } else {
            visible.push(data[i]);
            i += 1;
        }
    }
    visible
}

/// Strip IAC sequences from raw data, returning only user-visible bytes.
/// Calls `on_negotiate` for each IAC command/option pair encountered.
fn strip_telnet_commands(data: &[u8], on_negotiate: &mut impl FnMut(u8, u8)) -> Vec<u8> {
    let mut visible = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == IAC && i + 1 < data.len() {
            let cmd = data[i + 1];
            match cmd {
                IAC => {
                    visible.push(IAC);
                    i += 2;
                }
                WILL | WONT | DO | DONT => {
                    if i + 2 < data.len() {
                        on_negotiate(cmd, data[i + 2]);
                        i += 3;
                    } else {
                        i += 2;
                    }
                }
                SB => {
                    // Skip subnegotiation until IAC SE
                    i += 2;
                    while i < data.len() {
                        if data[i] == IAC && i + 1 < data.len() && data[i + 1] == SE {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    i += 2;
                }
            }
        } else {
            visible.push(data[i]);
            i += 1;
        }
    }
    visible
}

fn normalize_enter_bytes(data: &[u8], enter_mode: TelnetEnterMode) -> Vec<u8> {
    let replacement: &[u8] = match enter_mode {
        TelnetEnterMode::Crlf => b"\r\n",
        TelnetEnterMode::Cr => b"\r",
        TelnetEnterMode::Lf => b"\n",
    };
    let mut normalized = Vec::with_capacity(data.len());
    for byte in data {
        if *byte == b'\r' {
            normalized.extend_from_slice(replacement);
        } else {
            normalized.push(*byte);
        }
    }
    normalized
}

fn enter_bytes(enter_mode: TelnetEnterMode) -> &'static [u8] {
    match enter_mode {
        TelnetEnterMode::Crlf => b"\r\n",
        TelnetEnterMode::Cr => b"\r",
        TelnetEnterMode::Lf => b"\n",
    }
}

fn split_write_chunks(data: &[u8], force_character_at_a_time: bool) -> Vec<Vec<u8>> {
    if !force_character_at_a_time {
        return vec![data.to_vec()];
    }

    String::from_utf8_lossy(data)
        .chars()
        .map(|ch| {
            let mut buf = [0u8; 4];
            ch.encode_utf8(&mut buf).as_bytes().to_vec()
        })
        .collect()
}

fn local_echo_text(data: &[u8]) -> String {
    let mut visible = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            0x1b => {
                i += 1;
                if i < data.len() && data[i] == b'[' {
                    i += 1;
                    while i < data.len() && !(0x40..=0x7e).contains(&data[i]) {
                        i += 1;
                    }
                } else if i < data.len() && data[i] == b']' {
                    i += 1;
                    while i < data.len() {
                        if data[i] == 0x07 {
                            break;
                        }
                        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                }
            }
            b'\r' => {
                if i + 1 < data.len() && data[i + 1] == b'\n' {
                    i += 1;
                }
                visible.extend_from_slice(b"\r\n");
            }
            b'\n' => visible.extend_from_slice(b"\r\n"),
            0x20..=0x7e | b'\t' => visible.push(data[i]),
            byte if byte >= 0x80 => visible.push(byte),
            _ => {}
        }
        i += 1;
    }
    String::from_utf8_lossy(&visible).to_string()
}

#[derive(Debug, Default)]
struct TelnetLineEditor {
    buffer: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TelnetLineEditResult {
    display: String,
    writes: Vec<Vec<u8>>,
}

impl TelnetLineEditor {
    #[cfg(test)]
    fn buffer(&self) -> &str {
        &self.buffer
    }

    fn process(&mut self, data: &[u8], enter_mode: TelnetEnterMode) -> TelnetLineEditResult {
        let input = String::from_utf8_lossy(data);
        let mut result = TelnetLineEditResult::default();
        let mut chars = input.char_indices().peekable();

        while let Some((idx, ch)) = chars.next() {
            match ch {
                '\r' | '\n' => {
                    if ch == '\r' {
                        if let Some((_, '\n')) = chars.peek().copied() {
                            chars.next();
                        }
                    }

                    let mut line = self.buffer.as_bytes().to_vec();
                    line.extend_from_slice(enter_bytes(enter_mode));
                    result.writes.push(line);
                    result.display.push_str("\r\n");
                    self.buffer.clear();
                }
                '\u{7f}' | '\u{8}' => {
                    self.backspace(&mut result.display);
                }
                '\u{1b}' => {
                    let end = consume_escape_sequence_end(idx, &mut chars);
                    let sequence = &input.as_bytes()[idx..end];
                    if sequence == b"\x1b[3~" {
                        self.backspace(&mut result.display);
                    } else {
                        result.writes.push(sequence.to_vec());
                    }
                }
                '\t' | ' '..='\u{7e}' if !ch.is_control() => {
                    self.buffer.push(ch);
                    result.display.push(ch);
                }
                ch if !ch.is_control() => {
                    self.buffer.push(ch);
                    result.display.push(ch);
                }
                _ => {
                    let mut bytes = [0u8; 4];
                    result
                        .writes
                        .push(ch.encode_utf8(&mut bytes).as_bytes().to_vec());
                }
            }
        }

        result
    }

    fn backspace(&mut self, display: &mut String) {
        if self.buffer.pop().is_some() {
            display.push_str("\x08 \x08");
        }
    }
}

fn consume_escape_sequence_end(
    start: usize,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> usize {
    let Some((_, next)) = chars.peek().copied() else {
        return start + 1;
    };

    match next {
        '[' => {
            let mut end = start + 1;
            while let Some((idx, ch)) = chars.next() {
                end = idx + ch.len_utf8();
                if (('\u{40}'..='\u{7e}').contains(&ch)) && ch != '[' {
                    break;
                }
            }
            end
        }
        ']' => {
            let mut end = start + 1;
            while let Some((idx, ch)) = chars.next() {
                end = idx + ch.len_utf8();
                if ch == '\u{7}' {
                    break;
                }
                if ch == '\u{1b}' {
                    if let Some((_, '\\')) = chars.peek().copied() {
                        let (esc_end_idx, esc_end_ch) = chars.next().expect("peeked char");
                        end = esc_end_idx + esc_end_ch.len_utf8();
                        break;
                    }
                }
            }
            end
        }
        _ => {
            let (idx, ch) = chars.next().expect("peeked char");
            idx + ch.len_utf8()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DO, IAC, OPT_NAWS, OPT_SUPPRESS_GO_AHEAD, TelnetEnterMode, TelnetLineEditor,
        TelnetSessionConfig, WILL, maybe_build_naws, negotiate_response, normalize_enter_bytes,
        split_write_chunks, strip_telnet_commands,
    };

    #[test]
    fn standard_negotiation_responds_by_default() {
        assert_eq!(
            negotiate_response(WILL, OPT_SUPPRESS_GO_AHEAD, true, true),
            vec![IAC, DO, OPT_SUPPRESS_GO_AHEAD]
        );
    }

    #[test]
    fn send_sga_false_rejects_sga() {
        assert_ne!(
            negotiate_response(WILL, OPT_SUPPRESS_GO_AHEAD, true, false),
            vec![IAC, DO, OPT_SUPPRESS_GO_AHEAD]
        );
    }

    #[test]
    fn send_naws_false_rejects_naws_negotiation() {
        assert_ne!(
            negotiate_response(DO, OPT_NAWS, false, true),
            vec![IAC, WILL, OPT_NAWS]
        );
    }

    #[test]
    fn raw_mode_can_suppress_negotiation_responses() {
        let config = TelnetSessionConfig {
            raw_tcp_cli: true,
            ..Default::default()
        };
        let mut responses = Vec::new();
        if !config.raw_tcp_cli {
            let _ = strip_telnet_commands(&[IAC, WILL, OPT_SUPPRESS_GO_AHEAD], &mut |cmd, opt| {
                responses.push(negotiate_response(
                    cmd,
                    opt,
                    config.send_naws,
                    config.send_sga,
                ));
            });
        }
        assert!(responses.is_empty());
    }

    #[test]
    fn send_naws_false_prevents_naws_resize_payload() {
        let config = TelnetSessionConfig {
            send_naws: false,
            ..Default::default()
        };
        assert!(maybe_build_naws(80, 24, &config).is_none());
    }

    #[test]
    fn raw_mode_prevents_naws_resize_payload() {
        let config = TelnetSessionConfig {
            raw_tcp_cli: true,
            ..Default::default()
        };
        assert!(maybe_build_naws(80, 24, &config).is_none());
    }

    #[test]
    fn enter_conversion_maps_carriage_return() {
        assert_eq!(
            normalize_enter_bytes(b"show\r", TelnetEnterMode::Crlf),
            b"show\r\n"
        );
        assert_eq!(
            normalize_enter_bytes(b"show\r", TelnetEnterMode::Cr),
            b"show\r"
        );
        assert_eq!(
            normalize_enter_bytes(b"show\r", TelnetEnterMode::Lf),
            b"show\n"
        );
    }

    #[test]
    fn force_character_at_a_time_preserves_utf8_order() {
        let chunks = split_write_chunks("a中\r".as_bytes(), true);
        assert_eq!(
            chunks,
            vec![b"a".to_vec(), "中".as_bytes().to_vec(), b"\r".to_vec()]
        );
        let joined: Vec<u8> = chunks.into_iter().flatten().collect();
        assert_eq!(joined, "a中\r".as_bytes());
    }

    #[test]
    fn strip_telnet_commands_emits_naws_response_request() {
        let mut seen = Vec::new();
        let visible = strip_telnet_commands(b"hi\xff\xfd\x1f", &mut |cmd, opt| {
            seen.push((cmd, opt));
        });
        assert_eq!(visible, b"hi");
        assert_eq!(seen, vec![(DO, OPT_NAWS)]);
    }

    #[test]
    fn local_line_editor_backspace_updates_buffer() {
        let mut editor = TelnetLineEditor::default();
        let result = editor.process(b"abc\x7f", TelnetEnterMode::Cr);

        assert_eq!(editor.buffer(), "ab");
        assert_eq!(result.display, "abc\x08 \x08");
        assert!(result.writes.is_empty());
    }

    #[test]
    fn local_line_editor_sends_buffer_on_enter() {
        let mut editor = TelnetLineEditor::default();
        let result = editor.process(b"abc\x7fd\r", TelnetEnterMode::Cr);

        assert_eq!(editor.buffer(), "");
        assert_eq!(result.writes, vec![b"abd\r".to_vec()]);
        assert_eq!(result.display, "abc\x08 \x08d\r\n");

        let mut editor = TelnetLineEditor::default();
        let result = editor.process(b"abc\x7fd\r", TelnetEnterMode::Crlf);
        assert_eq!(result.writes, vec![b"abd\r\n".to_vec()]);

        let mut editor = TelnetLineEditor::default();
        let result = editor.process(b"abc\x7fd\r", TelnetEnterMode::Lf);
        assert_eq!(result.writes, vec![b"abd\n".to_vec()]);
    }

    #[test]
    fn local_line_editor_backspace_removes_one_utf8_char() {
        let mut editor = TelnetLineEditor::default();
        let result = editor.process("中a\u{7f}".as_bytes(), TelnetEnterMode::Cr);

        assert_eq!(editor.buffer(), "中");
        assert_eq!(result.display, "中a\x08 \x08");

        let result = editor.process(b"\x7f", TelnetEnterMode::Cr);
        assert_eq!(editor.buffer(), "");
        assert_eq!(result.display, "\x08 \x08");
    }

    #[test]
    fn local_line_editor_passes_controls_without_buffering() {
        let mut editor = TelnetLineEditor::default();
        let result = editor.process(b"a\x03\x04\x1b[A\x1b[3~", TelnetEnterMode::Cr);

        assert_eq!(editor.buffer(), "");
        assert_eq!(
            result.writes,
            vec![vec![0x03], vec![0x04], b"\x1b[A".to_vec()]
        );
        assert_eq!(result.display, "a\x08 \x08");
    }
}

pub async fn create_telnet_session(
    app: AppHandle,
    manager: Arc<SessionManager>,
    config: TelnetSessionConfig,
    connection_id: Option<String>,
    owner_window_label: Option<String>,
) -> AppResult<String> {
    let host = config.host.clone();
    let port = config.port;
    log_event(StructuredLog {
        level: StructuredLogLevel::Info,
        domain: "session.lifecycle".to_string(),
        event: "session.create_start".to_string(),
        message: "Creating Telnet session".to_string(),
        ids: connection_id
            .as_ref()
            .map(|value| serde_json::json!({ "connection_id": value })),
        data: Some(serde_json::json!({
            "session_type": "Telnet",
            "host": host,
            "port": port,
        })),
        error: None,
        client_timestamp: None,
    });
    let session_id = uuid::Uuid::new_v4().to_string();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();

    let session_info = SessionInfo {
        id: session_id.clone(),
        name: config.name.clone(),
        session_type: SessionType::Telnet,
        connected: true,
        owner_window_label,
        ai_execution_profile: AiExecutionProfile::SendOnly,
        injection_active: false,
    };

    let cwd: SharedCwd = Arc::new(tokio::sync::Mutex::new(None));
    let session_handle = SessionHandle {
        info: session_info,
        cmd_tx,
        ssh_config: None,
        ssh_handle: None,
        cwd,
        remote_fs: None,
    };
    manager.add_session(session_handle).await;

    let sid = session_id.clone();
    let mgr = manager.clone();

    tokio::spawn(async move {
        telnet_session_task(app, sid, mgr, cmd_rx, config, connection_id).await;
    });

    Ok(session_id)
}

async fn telnet_session_task(
    app: AppHandle,
    session_id: String,
    manager: Arc<SessionManager>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
    config: TelnetSessionConfig,
    connection_id: Option<String>,
) {
    let backspace_as_bs = config.backspace_mode == "ctrl_h";
    let host = config.host.clone();
    let port = config.port;
    let addr = format!("{}:{}", host, port);
    let stream = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            log_event(StructuredLog {
                level: StructuredLogLevel::Error,
                domain: "session.lifecycle".to_string(),
                event: "session.connection_failed".to_string(),
                message: "Telnet connection failed".to_string(),
                ids: Some(serde_json::json!({
                    "session_id": session_id.clone(),
                    "connection_id": connection_id.clone(),
                })),
                data: Some(serde_json::json!({
                    "session_type": "Telnet",
                    "host": host,
                    "port": port,
                })),
                error: Some(serde_json::json!({ "message": e.to_string() })),
                client_timestamp: None,
            });
            let _ = app.emit(
                &format!("session-error-{}", session_id),
                format!("Connection failed: {}", e),
            );
            emit_session_closed(&app, &session_id);
            manager.remove_session(&session_id).await;
            return;
        }
    };

    let (mut reader, mut writer) = stream.into_split();
    let output_event = format!("terminal-output-{}", session_id);
    let recording_mgr: Option<Arc<RecordingManager>> = app
        .try_state::<Arc<RecordingManager>>()
        .map(|state| state.inner().clone());
    let output = SessionOutputCoalescer::for_app(app.clone(), output_event.clone());

    let capture_processor = Arc::new(TokioMutex::new(OutputCaptureProcessor::new()));
    let capture_for_reader = capture_processor.clone();

    let zmodem_state: Arc<TokioMutex<Option<ZmodemTransfer>>> = Arc::new(TokioMutex::new(None));
    let zmodem_state_reader = zmodem_state.clone();
    let zmodem_event_name = format!("zmodem-event-{session_id}");
    let zmodem_event_reader = zmodem_event_name.clone();
    let (zmodem_out_tx, mut zmodem_out_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let app_reader = app.clone();
    let sid_reader = session_id.clone();
    let manager_reader = manager.clone();
    let output_reader = output.clone();
    let reader_connection_id = connection_id.clone();
    let recording_mgr_reader = recording_mgr.clone();
    let (pause_tx, mut pause_rx) = tokio::sync::watch::channel(false);

    let (negotiate_tx, mut negotiate_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (reader_done_tx, mut reader_done_rx) = mpsc::unbounded_channel::<()>();

    let reader_config = config.clone();
    let reader_handle = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let mut zmodem_detector = ZmodemDetector::new();
        'reader: loop {
            while *pause_rx.borrow() {
                if pause_rx.changed().await.is_err() {
                    break 'reader;
                }
            }
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let visible = if reader_config.raw_tcp_cli {
                        unescape_iac_iac(&buf[..n])
                    } else {
                        let neg_tx = negotiate_tx.clone();
                        strip_telnet_commands(&buf[..n], &mut |cmd, opt| {
                            let resp = negotiate_response(
                                cmd,
                                opt,
                                reader_config.send_naws,
                                reader_config.send_sga,
                            );
                            if !resp.is_empty() {
                                let _ = neg_tx.send(resp);
                            }
                        })
                    };
                    if visible.is_empty() {
                        continue;
                    }

                    // ZMODEM: if active, route to transfer.
                    {
                        let mut zm = zmodem_state_reader.lock().await;
                        if let Some(ref mut transfer) = *zm {
                            let actions = transfer.feed_incoming(&visible);
                            for action in actions {
                                match action {
                                    ZmodemAction::SendToRemote(data) => {
                                        let _ = zmodem_out_tx.send(data);
                                    }
                                    ZmodemAction::EmitEvent(event) => {
                                        let _ = app_reader.emit(&zmodem_event_reader, &event);
                                    }
                                }
                            }
                            if transfer.is_done() {
                                *zm = None;
                                zmodem_detector.reset();
                            }
                            continue;
                        }
                    }

                    // ZMODEM: detect header.
                    let process_visible = match zmodem_detector.feed(&visible) {
                        ZmodemDetectResult::Detected {
                            direction,
                            passthrough,
                            initial_bytes,
                        } => {
                            if !passthrough.is_empty() {
                                let pre = String::from_utf8_lossy(&passthrough).to_string();
                                if !pre.is_empty() {
                                    if let Some(ref recorder) = recording_mgr_reader {
                                        recorder.write_output(&sid_reader, &pre);
                                    }
                                    output_reader.push_owned(pre);
                                }
                            }
                            let prepared_upload = if direction == ZmodemDirection::Upload {
                                manager_reader.take_pending_zmodem_upload(&sid_reader).await
                            } else {
                                None
                            };
                            let (transfer, bootstrap_actions) =
                                start_zmodem_transfer(direction, &initial_bytes, prepared_upload);
                            for action in bootstrap_actions {
                                match action {
                                    ZmodemAction::SendToRemote(data) => {
                                        let _ = zmodem_out_tx.send(data);
                                    }
                                    ZmodemAction::EmitEvent(event) => {
                                        let _ = app_reader.emit(&zmodem_event_reader, &event);
                                    }
                                }
                            }
                            *zmodem_state_reader.lock().await = Some(transfer);
                            let _ = app_reader
                                .emit(&zmodem_event_reader, &ZmodemEvent::Detected { direction });
                            continue;
                        }
                        ZmodemDetectResult::NoMatch { passthrough } => {
                            if passthrough.is_empty() {
                                continue;
                            }
                            passthrough
                        }
                    };

                    let mut text = String::from_utf8_lossy(&process_visible).to_string();
                    let mut proc = capture_for_reader.lock().await;
                    if proc.has_active() {
                        text = proc.process(&text);
                    }
                    drop(proc);
                    if !text.is_empty() {
                        if let Some(ref recorder) = recording_mgr_reader {
                            recorder.write_output(&sid_reader, &text);
                        }
                        output_reader.push_owned(text);
                    }
                }
                Err(e) => {
                    log_rate_limited(StructuredLog {
                        level: StructuredLogLevel::Warn,
                        domain: "session.lifecycle".to_string(),
                        event: "telnet.read_error".to_string(),
                        message: "Telnet read error".to_string(),
                        ids: Some(serde_json::json!({
                            "session_id": sid_reader.clone(),
                            "connection_id": reader_connection_id.clone(),
                        })),
                        data: Some(serde_json::json!({
                            "session_type": "Telnet",
                        })),
                        error: Some(serde_json::json!({ "message": e.to_string() })),
                        client_timestamp: None,
                    });
                    break;
                }
            }
        }
        output_reader.close();
        let _ = reader_done_tx.send(());
    });

    let line_edit_active = config.raw_tcp_cli && config.local_line_edit;
    let mut line_editor = TelnetLineEditor::default();

    loop {
        tokio::select! {
            Some(neg_data) = negotiate_rx.recv() => {
                if let Err(e) = writer.write_all(&neg_data).await {
                    log_rate_limited(StructuredLog {
                        level: StructuredLogLevel::Warn,
                        domain: "session.lifecycle".to_string(),
                        event: "telnet.negotiate_write_error".to_string(),
                        message: "Telnet negotiate write error".to_string(),
                        ids: Some(serde_json::json!({
                            "session_id": session_id.clone(),
                            "connection_id": connection_id.clone(),
                        })),
                        data: Some(serde_json::json!({
                            "session_type": "Telnet",
                        })),
                        error: Some(serde_json::json!({ "message": e.to_string() })),
                        client_timestamp: None,
                    });
                    break;
                }
            }
            Some(zdata) = zmodem_out_rx.recv() => {
                let _ = writer.write_all(&zdata).await;
            }
            reader_done = reader_done_rx.recv() => {
                let _ = reader_done;
                break;
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(SessionCommand::Attach) => {
                        output.attach();
                    }
                    Some(SessionCommand::Write(mut data)) => {
                        if zmodem_state.lock().await.is_some() {
                            continue;
                        }

                        let mut write_failed = None;
                        if line_edit_active {
                            let edit_result = line_editor.process(&data, config.enter_mode);
                            if !edit_result.display.is_empty() {
                                output.push_owned(edit_result.display);
                            }

                            for data in edit_result.writes {
                                if let Some(ref recorder) = recording_mgr {
                                    recorder.write_input(&session_id, &data);
                                }
                                if let Err(e) = writer.write_all(&data).await {
                                    write_failed = Some(e);
                                    break;
                                }
                            }
                        } else {
                            if backspace_as_bs {
                                remap_del_to_bs(&mut data);
                            }
                            let data = normalize_enter_bytes(&data, config.enter_mode);
                            if config.local_echo {
                                let echoed = local_echo_text(&data);
                                if !echoed.is_empty() {
                                    output.push_owned(echoed);
                                }
                            }
                            if let Some(ref recorder) = recording_mgr {
                                recorder.write_input(&session_id, &data);
                            }
                            for chunk in split_write_chunks(&data, config.force_character_at_a_time) {
                                if let Err(e) = writer.write_all(&chunk).await {
                                    write_failed = Some(e);
                                    break;
                                }
                            }
                        }

                        if let Some(e) = write_failed {
                            log_rate_limited(StructuredLog {
                                level: StructuredLogLevel::Warn,
                                domain: "session.lifecycle".to_string(),
                                event: "telnet.write_error".to_string(),
                                message: "Telnet write error".to_string(),
                                ids: Some(serde_json::json!({
                                    "session_id": session_id.clone(),
                                    "connection_id": connection_id.clone(),
                                })),
                                data: Some(serde_json::json!({
                                    "session_type": "Telnet",
                                })),
                                error: Some(serde_json::json!({ "message": e.to_string() })),
                                client_timestamp: None,
                            });
                            break;
                        }
                    }
                    Some(SessionCommand::CaptureExec { marker_id, wrapped_command, result_tx }) => {
                        capture_processor.lock().await.register(marker_id, result_tx);
                        if let Err(e) = writer.write_all(&wrapped_command).await {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %e,
                                "Failed to write capture command to Telnet"
                            );
                        }
                    }
                    Some(SessionCommand::Resize { cols, rows }) => {
                        if let Some(naws) = maybe_build_naws(cols as u16, rows as u16, &config) {
                            let _ = writer.write_all(&naws).await;
                        }
                    }
                    Some(SessionCommand::PauseOutput) => {
                        let _ = pause_tx.send(true);
                    }
                    Some(SessionCommand::ResumeOutput) => {
                        let _ = pause_tx.send(false);
                    }
                    Some(SessionCommand::ZmodemAcceptDownload { save_dir }) => {
                        let mut zm = zmodem_state.lock().await;
                        if let Some(ref mut transfer) = *zm {
                            let actions = transfer.accept_download(save_dir);
                            for action in actions {
                                match action {
                                    ZmodemAction::SendToRemote(data) => { let _ = writer.write_all(&data).await; }
                                    ZmodemAction::EmitEvent(event) => { let _ = app.emit(&zmodem_event_name, &event); }
                                }
                            }
                            if transfer.is_done() { *zm = None; }
                        }
                    }
                    Some(SessionCommand::ZmodemAcceptUpload { files }) => {
                        let mut zm = zmodem_state.lock().await;
                        if let Some(ref mut transfer) = *zm {
                            let actions = transfer.accept_upload(files);
                            for action in actions {
                                match action {
                                    ZmodemAction::SendToRemote(data) => { let _ = writer.write_all(&data).await; }
                                    ZmodemAction::EmitEvent(event) => { let _ = app.emit(&zmodem_event_name, &event); }
                                }
                            }
                            if transfer.is_done() { *zm = None; }
                        }
                    }
                    Some(SessionCommand::ZmodemCancel) => {
                        manager.clear_pending_zmodem_upload(&session_id).await;
                        let mut zm = zmodem_state.lock().await;
                        if let Some(ref mut transfer) = *zm {
                            let actions = transfer.cancel();
                            for action in actions {
                                match action {
                                    ZmodemAction::SendToRemote(data) => { let _ = writer.write_all(&data).await; }
                                    ZmodemAction::EmitEvent(event) => { let _ = app.emit(&zmodem_event_name, &event); }
                                }
                            }
                        }
                        *zm = None;
                    }
                    Some(SessionCommand::Close) | None => {
                        break;
                    }
                }
            }
        }
    }

    output.close();
    reader_handle.abort();
    if let Some(ref recorder) = recording_mgr {
        recorder.cleanup_session(&session_id);
    }
    manager.remove_session(&session_id).await;
    emit_session_closed(&app, &session_id);
}
