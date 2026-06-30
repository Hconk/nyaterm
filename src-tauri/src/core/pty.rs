//! Local PTY (pseudo-terminal) session creation and management.
//!
//! Spawns the user's shell (PowerShell on Windows, $SHELL elsewhere) and bridges I/O to Tauri.

use super::recording::RecordingManager;
use super::session::{
    SessionCommand, SessionHandle, SessionInfo, SessionManager, SessionType, SharedCwd,
};
use super::update_cwd_if_changed;
use super::zmodem::{
    ZmodemAction, ZmodemDetectResult, ZmodemDetector, ZmodemDirection, ZmodemEvent, ZmodemTransfer,
    start_zmodem_transfer,
};
use crate::app_event::{emit_cwd_changed, emit_session_closed};
use crate::config::AiExecutionProfile;
use crate::core::SessionOutputCoalescer;
use crate::core::capture::OutputCaptureProcessor;
use crate::core::ssh::osc::{OscStripper, build_ready_marker};
use crate::error::AppResult;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, mpsc as std_mpsc};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

/// Per-connection local terminal config.
pub struct LocalSessionConfig {
    pub shell_path: String,
    pub shell_args: String,
    pub working_dir: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellCommandSpec {
    program: String,
    args: Vec<String>,
}

fn build_shell_command(
    shell_path: &str,
    shell_args: &str,
) -> Result<(CommandBuilder, String), String> {
    let spec = resolve_shell_command(shell_path, shell_args)?;
    let mut builder = CommandBuilder::new(&spec.program);
    if !spec.args.is_empty() {
        builder.args(spec.args.iter().map(String::as_str));
    }
    Ok((builder, spec.program))
}

fn default_local_shell_args(program: &str) -> Vec<String> {
    if cfg!(windows) {
        return vec![];
    }

    let shell_name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();

    match shell_name.as_str() {
        "bash" | "zsh" | "fish" => vec!["--login".to_string(), "-i".to_string()],
        _ => vec![],
    }
}

fn resolve_shell_command(shell_path: &str, shell_args: &str) -> Result<ShellCommandSpec, String> {
    let raw_program = shell_path.trim();
    let program = trim_wrapping_quotes(raw_program);
    if program.is_empty() {
        let (_, shell_name) = platform_default_shell();
        let args = parse_shell_args(shell_args)?;
        return Ok(ShellCommandSpec {
            args: if args.is_empty() {
                default_local_shell_args(&shell_name)
            } else {
                args
            },
            program: shell_name,
        });
    }

    let args = parse_shell_args(shell_args)?;
    #[cfg(target_os = "windows")]
    if is_windows_terminal_alias(program) {
        return Ok(resolve_windows_terminal_default_profile_shell(args.clone())
            .unwrap_or_else(|| fallback_windows_terminal_shell(args)));
    }

    if !args.is_empty() {
        return Ok(ShellCommandSpec {
            program: resolve_program_for_spawn(program),
            args,
        });
    }

    if should_treat_as_literal_program(raw_program) {
        return Ok(ShellCommandSpec {
            program: resolve_program_for_spawn(program),
            args: default_local_shell_args(program),
        });
    }

    let mut legacy_parts = parse_shell_args(program)?;
    if legacy_parts.is_empty() {
        return Err("Shell path is required".to_string());
    }
    let legacy_program = legacy_parts.remove(0);
    Ok(ShellCommandSpec {
        program: resolve_program_for_spawn(&legacy_program),
        args: legacy_parts,
    })
}

#[cfg(target_os = "windows")]
fn is_windows_terminal_alias(program: &str) -> bool {
    matches!(program.to_ascii_lowercase().as_str(), "wt" | "wt.exe")
}

#[cfg(target_os = "windows")]
fn resolve_windows_terminal_default_profile_shell(
    extra_args: Vec<String>,
) -> Option<ShellCommandSpec> {
    for settings_path in windows_terminal_settings_paths() {
        let Ok(raw_settings) = std::fs::read_to_string(settings_path) else {
            continue;
        };
        let Ok(settings) = serde_json::from_str::<serde_json::Value>(&raw_settings) else {
            continue;
        };
        let Some(commandline) = windows_terminal_default_profile_commandline(&settings) else {
            continue;
        };
        if let Some(spec) = shell_spec_from_windows_commandline(&commandline, extra_args.clone()) {
            return Some(spec);
        }
    }

    None
}

#[cfg(target_os = "windows")]
fn fallback_windows_terminal_shell(args: Vec<String>) -> ShellCommandSpec {
    ShellCommandSpec {
        program: resolve_program_for_spawn("powershell.exe"),
        args,
    }
}

#[cfg(target_os = "windows")]
fn windows_terminal_settings_paths() -> Vec<PathBuf> {
    let Some(local_data_dir) = dirs::data_local_dir() else {
        return Vec::new();
    };

    vec![
        local_data_dir
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json"),
        local_data_dir
            .join("Packages")
            .join("Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json"),
        local_data_dir
            .join("Microsoft")
            .join("Windows Terminal")
            .join("settings.json"),
        local_data_dir
            .join("Microsoft")
            .join("Windows Terminal Preview")
            .join("settings.json"),
    ]
}

#[cfg(target_os = "windows")]
fn windows_terminal_default_profile_commandline(settings: &serde_json::Value) -> Option<String> {
    let default_profile = settings.get("defaultProfile")?.as_str()?;
    let profiles = settings.get("profiles")?.get("list")?.as_array()?;

    profiles
        .iter()
        .find(|profile| {
            profile
                .get("guid")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|guid| guid.eq_ignore_ascii_case(default_profile))
        })
        .and_then(windows_terminal_profile_commandline)
}

#[cfg(target_os = "windows")]
fn windows_terminal_profile_commandline(profile: &serde_json::Value) -> Option<String> {
    if let Some(commandline) = profile
        .get("commandline")
        .and_then(serde_json::Value::as_str)
        .map(expand_windows_env_vars)
    {
        return Some(commandline);
    }

    let name = profile
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let source = profile
        .get("source")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    if name.contains("powershell") {
        Some("powershell.exe".to_string())
    } else if name.contains("command prompt") || name.contains("cmd") || name.contains("命令提示符")
    {
        Some("cmd.exe".to_string())
    } else if source.contains("wsl") || name.contains("ubuntu") || name.contains("debian") {
        Some("wsl.exe".to_string())
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn shell_spec_from_windows_commandline(
    commandline: &str,
    extra_args: Vec<String>,
) -> Option<ShellCommandSpec> {
    let mut parts = parse_shell_args(commandline).ok()?;
    if parts.is_empty() {
        return None;
    }

    let program = parts.remove(0);
    parts.extend(extra_args);

    Some(ShellCommandSpec {
        program: resolve_program_for_spawn(&program),
        args: parts,
    })
}

#[cfg(target_os = "windows")]
fn expand_windows_env_vars(value: &str) -> String {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find('%') {
        expanded.push_str(&rest[..start]);
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('%') else {
            expanded.push_str(&rest[start..]);
            return expanded;
        };

        let name = &after_start[..end];
        if name.is_empty() {
            expanded.push_str("%%");
        } else if let Ok(env_value) = std::env::var(name) {
            expanded.push_str(&env_value);
        } else {
            expanded.push('%');
            expanded.push_str(name);
            expanded.push('%');
        }
        rest = &after_start[end + 1..];
    }

    expanded.push_str(rest);
    expanded
}

fn resolve_program_for_spawn(program: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        resolve_windows_program_for_spawn(program).unwrap_or_else(|| program.to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        program.to_string()
    }
}

#[cfg(target_os = "windows")]
fn resolve_windows_program_for_spawn(program: &str) -> Option<String> {
    let program = trim_wrapping_quotes(program).trim();
    if program.is_empty() || looks_like_path(program) {
        return None;
    }

    resolve_windows_builtin_shell(program).or_else(|| find_windows_program_on_search_path(program))
}

#[cfg(target_os = "windows")]
fn resolve_windows_builtin_shell(program: &str) -> Option<String> {
    let lower = program.to_ascii_lowercase();
    match lower.as_str() {
        "cmd" | "cmd.exe" => {
            let mut candidates = Vec::new();
            if let Some(comspec) = std::env::var_os("COMSPEC").map(PathBuf::from) {
                candidates.push(comspec);
            }
            for system_dir in windows_system_dirs() {
                candidates.push(system_dir.join("cmd.exe"));
            }
            first_existing_file(candidates)
        }
        "powershell" | "powershell.exe" => {
            let mut candidates = Vec::new();
            for system_dir in windows_system_dirs() {
                candidates.push(
                    system_dir
                        .join("WindowsPowerShell")
                        .join("v1.0")
                        .join("powershell.exe"),
                );
            }
            first_existing_file(candidates)
        }
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn find_windows_program_on_search_path(program: &str) -> Option<String> {
    let names = windows_program_candidate_names(program);
    let mut dirs = windows_default_search_dirs();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }

    for dir in dirs {
        for name in &names {
            if let Some(path) = first_existing_file([dir.join(name)]) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn windows_program_candidate_names(program: &str) -> Vec<String> {
    if Path::new(program).extension().is_some() {
        return vec![program.to_string()];
    }

    let mut names = vec![format!("{program}.exe")];
    if let Some(pathext) = std::env::var_os("PATHEXT") {
        for ext in pathext.to_string_lossy().split(';') {
            let ext = ext.trim();
            if ext.is_empty() {
                continue;
            }
            let normalized_ext = if ext.starts_with('.') {
                ext.to_string()
            } else {
                format!(".{ext}")
            };
            let candidate = format!("{program}{normalized_ext}");
            if !names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&candidate))
            {
                names.push(candidate);
            }
        }
    }
    names
}

#[cfg(target_os = "windows")]
fn windows_default_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    dirs.extend(windows_system_dirs());
    if let Some(windows_dir) = windows_dir() {
        dirs.push(windows_dir);
    }
    dirs
}

#[cfg(target_os = "windows")]
fn windows_system_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(windows_dir) = windows_dir() {
        dirs.push(windows_dir.join("System32"));
        dirs.push(windows_dir.join("Sysnative"));
        dirs.push(windows_dir.join("SysWOW64"));
    }
    dirs
}

#[cfg(target_os = "windows")]
fn windows_dir() -> Option<PathBuf> {
    std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| {
            let fallback = PathBuf::from(r"C:\Windows");
            fallback.is_dir().then_some(fallback)
        })
}

#[cfg(target_os = "windows")]
fn first_existing_file<I>(paths: I) -> Option<String>
where
    I: IntoIterator<Item = PathBuf>,
{
    paths
        .into_iter()
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().to_string())
}

fn should_treat_as_literal_program(value: &str) -> bool {
    !value.chars().any(char::is_whitespace)
        || path_exists(value)
        || looks_like_path(value)
        || is_quoted(value)
}

fn path_exists(value: &str) -> bool {
    Path::new(trim_wrapping_quotes(value)).exists()
}

fn looks_like_path(value: &str) -> bool {
    value.contains('\\') || value.contains('/') || Path::new(value).is_absolute()
}

fn is_quoted(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
}

fn trim_wrapping_quotes(value: &str) -> &str {
    let trimmed = value.trim();
    if is_quoted(trimmed) {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

pub(crate) fn parse_shell_args(input: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = input.trim().chars().peekable();
    let mut quote: Option<char> = None;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        if ch == '\\' {
            let escapes_next = chars.peek().is_some_and(|next| match quote {
                Some(active_quote) => *next == active_quote || *next == '\\',
                None => next.is_whitespace() || *next == '"' || *next == '\'' || *next == '\\',
            });
            if escapes_next {
                escaped = true;
            } else {
                current.push(ch);
            }
            continue;
        }

        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }

        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }

        if ch.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
            while chars.peek().is_some_and(|next| next.is_whitespace()) {
                let _ = chars.next();
            }
            continue;
        }

        current.push(ch);
    }

    if escaped {
        current.push('\\');
    }
    if quote.is_some() {
        return Err("Unclosed quote in shell arguments".to_string());
    }
    if !current.is_empty() {
        args.push(current);
    }

    Ok(args)
}

fn platform_default_shell() -> (CommandBuilder, String) {
    #[cfg(target_os = "windows")]
    {
        let shell = resolve_program_for_spawn("powershell.exe");
        (CommandBuilder::new(&shell), shell)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut builder = CommandBuilder::new(&shell);
        let default_args = default_local_shell_args(&shell);
        if !default_args.is_empty() {
            builder.args(default_args.iter().map(String::as_str));
        }
        (builder, shell)
    }
}

#[cfg(target_os = "macos")]
fn ensure_macos_interactive_path(cmd: &mut CommandBuilder) {
    let current_path = std::env::var("PATH").unwrap_or_default();
    let mut paths: Vec<String> = current_path
        .split(':')
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect();

    for path in ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"] {
        if !paths.iter().any(|existing| existing == path) && Path::new(path).exists() {
            paths.push(path.to_string());
        }
    }

    if !paths.is_empty() {
        cmd.env("PATH", paths.join(":"));
    }
}

#[cfg(target_os = "macos")]
fn configure_local_pty_environment(cmd: &mut CommandBuilder) {
    cmd.env("TERM", "xterm-256color");
    set_utf8_env_if_missing_or_non_utf8(cmd, "LANG", "en_US.UTF-8");
    set_utf8_env_if_missing_or_non_utf8(cmd, "LC_CTYPE", "UTF-8");
}

#[cfg(target_os = "macos")]
fn set_utf8_env_if_missing_or_non_utf8(cmd: &mut CommandBuilder, key: &str, default_value: &str) {
    let value = std::env::var(key)
        .ok()
        .filter(|value| is_utf8_locale(value))
        .unwrap_or_else(|| default_value.to_string());
    cmd.env(key, value);
}

#[cfg(target_os = "macos")]
fn is_utf8_locale(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase().replace('_', "-");
    normalized.contains("utf-8") || normalized.contains("utf8")
}

fn infer_local_ai_execution_profile(shell_name: &str) -> AiExecutionProfile {
    let shell = shell_name.to_ascii_lowercase();
    if shell.contains("powershell") || shell.contains("pwsh") {
        AiExecutionProfile::Powershell
    } else if shell.contains("cmd") {
        AiExecutionProfile::Cmd
    } else if shell.contains("bash")
        || shell.contains("zsh")
        || shell.contains("fish")
        || shell.contains("wsl")
        || shell.ends_with("sh")
        || shell.contains("/sh")
        || shell.contains("\\sh")
    {
        AiExecutionProfile::Posix
    } else {
        AiExecutionProfile::SendOnly
    }
}

struct LocalStartupScript {
    script: Option<String>,
    shell_integration_active: bool,
}

fn build_local_startup_script(_shell_name: &str, _ready_marker: &str) -> LocalStartupScript {
    build_local_startup_script_for_platform(
        _shell_name,
        _ready_marker,
        cfg!(not(target_os = "windows")),
    )
}

fn build_local_startup_script_for_platform(
    _shell_name: &str,
    _ready_marker: &str,
    _allow_unix_prelude: bool,
) -> LocalStartupScript {
    LocalStartupScript {
        script: None,
        shell_integration_active: false,
    }
}

fn should_emit_visible_output(suppress_visible: &mut bool, ready: bool) -> bool {
    if !*suppress_visible {
        return true;
    }

    if !ready {
        return false;
    }

    *suppress_visible = false;
    true
}

fn write_to_pty(writer: &mut dyn Write, data: &[u8]) -> std::io::Result<()> {
    writer.write_all(data)?;
    writer.flush()
}

/// Spawns a local shell in a PTY and registers the session with the manager.
/// If `config` is provided, uses the shell path and working dir from it.
pub async fn create_local_session(
    app: AppHandle,
    manager: Arc<SessionManager>,
    config: Option<LocalSessionConfig>,
    owner_window_label: Option<String>,
) -> AppResult<String> {
    tracing::info!("Creating local PTY session");
    let session_id = uuid::Uuid::new_v4().to_string();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();

    let session_name = config
        .as_ref()
        .map_or("Local Terminal".to_string(), |c| c.name.clone());

    let (_, shell_name) = match &config {
        Some(cfg) if !cfg.shell_path.trim().is_empty() => {
            build_shell_command(&cfg.shell_path, &cfg.shell_args)
                .map_err(crate::error::AppError::Config)?
        }
        _ => platform_default_shell(),
    };
    let ai_execution_profile = infer_local_ai_execution_profile(&shell_name);
    let ready_marker = build_ready_marker(&session_id);
    let startup_script = build_local_startup_script(&shell_name, &ready_marker);
    let injection_active = startup_script.shell_integration_active;

    let session_info = SessionInfo {
        id: session_id.clone(),
        name: session_name,
        session_type: SessionType::Local,
        connected: true,
        owner_window_label,
        ai_execution_profile,
        injection_active,
    };

    let cwd: SharedCwd = Arc::new(tokio::sync::Mutex::new(None));
    let session_handle = SessionHandle {
        info: session_info,
        cmd_tx,
        ssh_config: None,
        ssh_handle: None,
        cwd: cwd.clone(),
        remote_fs: None,
    };
    manager.add_session(session_handle).await;

    let sid = session_id.clone();
    let mgr = manager.clone();
    let rt_handle = tokio::runtime::Handle::current();

    std::thread::spawn(move || {
        pty_session_thread(
            app,
            sid,
            mgr,
            cmd_rx,
            rt_handle,
            cwd,
            config,
            startup_script.script,
            ready_marker,
        );
    });

    Ok(session_id)
}

fn pty_session_thread(
    app: AppHandle,
    session_id: String,
    manager: Arc<SessionManager>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
    rt_handle: tokio::runtime::Handle,
    cwd: SharedCwd,
    config: Option<LocalSessionConfig>,
    startup_script: Option<String>,
    ready_marker: String,
) {
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Failed to open PTY: {}", e);
            let _ = app.emit(
                &format!("session-error-{}", session_id),
                format!("Failed to open PTY: {}", e),
            );
            emit_session_closed(&app, &session_id);
            rt_handle.block_on(async {
                manager.remove_session(&session_id).await;
            });
            return;
        }
    };

    let (mut cmd, _) = match &config {
        Some(cfg) if !cfg.shell_path.trim().is_empty() => {
            match build_shell_command(&cfg.shell_path, &cfg.shell_args) {
                Ok(command) => command,
                Err(error) => {
                    tracing::error!("Failed to build shell command: {}", error);
                    let _ = app.emit(
                        &format!("session-error-{}", session_id),
                        format!("Failed to build shell command: {}", error),
                    );
                    emit_session_closed(&app, &session_id);
                    rt_handle.block_on(async {
                        manager.remove_session(&session_id).await;
                    });
                    return;
                }
            }
        }
        _ => platform_default_shell(),
    };

    if let Some(ref cfg) = config {
        if let Some(ref dir) = cfg.working_dir {
            if !dir.is_empty() {
                let working_dir = Path::new(dir);
                if working_dir.is_dir() {
                    cmd.cwd(dir);
                } else {
                    tracing::warn!(
                        working_dir = %dir,
                        "Configured local terminal working directory does not exist; using default working directory"
                    );
                    let _ = app.emit(
                        &format!("session-warning-{}", session_id),
                        format!(
                            "Configured working directory '{}' does not exist; using the default working directory.",
                            dir
                        ),
                    );
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    ensure_macos_interactive_path(&mut cmd);
    #[cfg(target_os = "macos")]
    configure_local_pty_environment(&mut cmd);

    let mut _child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to spawn shell: {}", e);
            let _ = app.emit(
                &format!("session-error-{}", session_id),
                format!("Failed to spawn shell: {}", e),
            );
            emit_session_closed(&app, &session_id);
            rt_handle.block_on(async {
                manager.remove_session(&session_id).await;
            });
            return;
        }
    };
    drop(pair.slave);

    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("Failed to take PTY writer: {}", e);
            let _ = app.emit(
                &format!("session-error-{}", session_id),
                format!("Failed to take PTY writer: {}", e),
            );
            emit_session_closed(&app, &session_id);
            rt_handle.block_on(async {
                manager.remove_session(&session_id).await;
            });
            return;
        }
    };

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to clone PTY reader: {}", e);
            let _ = app.emit(
                &format!("session-error-{}", session_id),
                format!("Failed to clone PTY reader: {}", e),
            );
            emit_session_closed(&app, &session_id);
            rt_handle.block_on(async {
                manager.remove_session(&session_id).await;
            });
            return;
        }
    };
    let master = pair.master;

    let output_event = format!("terminal-output-{}", session_id);
    let output = SessionOutputCoalescer::for_app(app.clone(), output_event.clone());

    let capture_processor = Arc::new(StdMutex::new(OutputCaptureProcessor::new()));
    let capture_for_reader = capture_processor.clone();
    let output_pause = Arc::new((StdMutex::new(false), std::sync::Condvar::new()));
    let output_pause_reader = output_pause.clone();

    let zmodem_state: Arc<StdMutex<Option<ZmodemTransfer>>> = Arc::new(StdMutex::new(None));
    let zmodem_state_reader = zmodem_state.clone();
    let zmodem_event_name = format!("zmodem-event-{session_id}");
    let zmodem_event_reader = zmodem_event_name.clone();
    let (zmodem_out_tx, mut zmodem_out_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let app_read = app.clone();
    let sid_read = session_id.clone();
    let cwd_event = format!("cwd-changed-{}", session_id);
    let rt_for_reader = rt_handle.clone();
    let recording_mgr_reader: Option<Arc<RecordingManager>> = app
        .try_state::<Arc<RecordingManager>>()
        .map(|s| s.inner().clone());
    let sid_for_rec_reader = session_id.clone();
    let output_reader = output.clone();
    let manager_reader = manager.clone();
    let suppress_startup_output = startup_script.is_some();
    let (reader_done_tx, reader_done_rx) = std_mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut raw_buf = [0u8; 4096];
        let mut stripper = OscStripper::new(&ready_marker);
        let mut suppress_visible = suppress_startup_output;
        let mut zmodem_detector = ZmodemDetector::new();
        loop {
            {
                let (lock, cvar) = &*output_pause_reader;
                let mut paused = lock.lock().unwrap();
                while *paused {
                    paused = cvar.wait(paused).unwrap();
                }
            }
            match reader.read(&mut raw_buf) {
                Ok(0) => break,
                Ok(n) => {
                    let raw = &raw_buf[..n];

                    // ZMODEM: if active, route raw bytes to the transfer.
                    {
                        let mut zm = zmodem_state_reader.lock().unwrap();
                        if let Some(ref mut transfer) = *zm {
                            let actions = transfer.feed_incoming(raw);
                            for action in actions {
                                match action {
                                    ZmodemAction::SendToRemote(data) => {
                                        let _ = zmodem_out_tx.send(data);
                                    }
                                    ZmodemAction::EmitEvent(event) => {
                                        let _ = app_read.emit(&zmodem_event_reader, &event);
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

                    // ZMODEM: detect header in raw bytes.
                    let process_raw = if !suppress_visible {
                        match zmodem_detector.feed(raw) {
                            ZmodemDetectResult::Detected {
                                direction,
                                passthrough,
                                initial_bytes,
                            } => {
                                if !passthrough.is_empty() {
                                    let pre = String::from_utf8_lossy(&passthrough).to_string();
                                    if !pre.is_empty() {
                                        output_reader.push_owned(pre);
                                    }
                                }
                                let prepared_upload = if direction == ZmodemDirection::Upload {
                                    rt_for_reader.block_on(async {
                                        manager_reader.take_pending_zmodem_upload(&sid_read).await
                                    })
                                } else {
                                    None
                                };
                                let (transfer, bootstrap_actions) = start_zmodem_transfer(
                                    direction,
                                    &initial_bytes,
                                    prepared_upload,
                                );
                                for action in bootstrap_actions {
                                    match action {
                                        ZmodemAction::SendToRemote(data) => {
                                            let _ = zmodem_out_tx.send(data);
                                        }
                                        ZmodemAction::EmitEvent(event) => {
                                            let _ = app_read.emit(&zmodem_event_reader, &event);
                                        }
                                    }
                                }
                                *zmodem_state_reader.lock().unwrap() = Some(transfer);
                                let _ = app_read.emit(
                                    &zmodem_event_reader,
                                    &ZmodemEvent::Detected { direction },
                                );
                                continue;
                            }
                            ZmodemDetectResult::NoMatch { passthrough } => {
                                if passthrough.is_empty() {
                                    continue;
                                }
                                passthrough
                            }
                        }
                    } else {
                        raw.to_vec()
                    };

                    let text = String::from_utf8_lossy(&process_raw).to_string();
                    let mut result = stripper.push(&text);

                    for path in &result.cwd_paths {
                        let cwd_ev = cwd_event.clone();
                        let app_ref = app_read.clone();
                        let next_cwd = rt_for_reader
                            .block_on(async { update_cwd_if_changed(&cwd, path).await });
                        if let Some(next_cwd) = next_cwd {
                            emit_cwd_changed(&app_ref, &cwd_ev, &sid_for_rec_reader, &next_cwd);
                        }
                    }

                    for command in &result.accepted_commands {
                        rt_for_reader.block_on(
                            manager_reader
                                .confirm_command_submission(&sid_for_rec_reader, command.clone()),
                        );
                        let _ = app_read.emit(
                            "session-command-accepted",
                            serde_json::json!({
                                "sessionId": &sid_for_rec_reader,
                                "command": command,
                            }),
                        );
                    }

                    if !should_emit_visible_output(&mut suppress_visible, result.ready) {
                        continue;
                    }

                    if let Ok(mut proc) = capture_for_reader.lock() {
                        if proc.has_active() {
                            result.visible = proc.process(&result.visible);
                        }
                    }

                    if !result.visible.is_empty() {
                        if let Some(rec) = recording_mgr_reader.as_ref() {
                            rec.write_output(&sid_for_rec_reader, &result.visible);
                        }
                        output_reader.push_owned(result.visible);
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        session_id = %sid_read,
                        error = %error,
                        "Local PTY reader exited"
                    );
                    break;
                }
            }
        }
        output_reader.close();
        let _ = reader_done_tx.send(());
    });

    let recording_mgr: Option<Arc<RecordingManager>> = app
        .try_state::<Arc<RecordingManager>>()
        .map(|s| s.inner().clone());
    if let Some(script) = startup_script.as_deref() {
        if let Err(error) = write_to_pty(&mut *writer, script.as_bytes()) {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "Failed to write local PTY startup script"
            );
        }
    }
    loop {
        match reader_done_rx.try_recv() {
            Ok(()) | Err(std_mpsc::TryRecvError::Disconnected) => break,
            Err(std_mpsc::TryRecvError::Empty) => {}
        }

        // Drain any ZMODEM outgoing data first (non-blocking).
        while let Ok(data) = zmodem_out_rx.try_recv() {
            let _ = write_to_pty(&mut *writer, &data);
        }

        let cmd = match cmd_rx.try_recv() {
            Ok(cmd) => cmd,
            Err(mpsc::error::TryRecvError::Disconnected) => break,
            Err(mpsc::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        match cmd {
            SessionCommand::Attach => {
                output.attach();
            }
            SessionCommand::Write(data) => {
                if zmodem_state.lock().unwrap().is_some() {
                    continue;
                }
                if let Some(ref rec) = recording_mgr {
                    rec.write_input(&session_id, &data);
                }
                if let Err(error) = write_to_pty(&mut *writer, &data) {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %error,
                        "Failed to write to local PTY"
                    );
                }
            }
            SessionCommand::CaptureExec {
                marker_id,
                wrapped_command,
                result_tx,
            } => {
                if let Ok(mut proc) = capture_processor.lock() {
                    proc.register(marker_id, result_tx);
                }
                if let Err(error) = write_to_pty(&mut *writer, &wrapped_command) {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %error,
                        "Failed to write capture command to local PTY"
                    );
                }
            }
            SessionCommand::Resize { cols, rows } => {
                let _ = master.resize(PtySize {
                    rows: rows as u16,
                    cols: cols as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            SessionCommand::PauseOutput => {
                let (lock, _) = &*output_pause;
                if let Ok(mut paused) = lock.lock() {
                    *paused = true;
                }
            }
            SessionCommand::ResumeOutput => {
                let (lock, cvar) = &*output_pause;
                if let Ok(mut paused) = lock.lock() {
                    *paused = false;
                    cvar.notify_all();
                }
            }
            SessionCommand::ZmodemAcceptDownload { save_dir } => {
                let mut zm = zmodem_state.lock().unwrap();
                if let Some(ref mut transfer) = *zm {
                    let actions = transfer.accept_download(save_dir);
                    for action in actions {
                        match action {
                            ZmodemAction::SendToRemote(data) => {
                                let _ = write_to_pty(&mut *writer, &data);
                            }
                            ZmodemAction::EmitEvent(event) => {
                                let _ = app.emit(&zmodem_event_name, &event);
                            }
                        }
                    }
                    if transfer.is_done() {
                        *zm = None;
                    }
                }
            }
            SessionCommand::ZmodemAcceptUpload { files } => {
                let mut zm = zmodem_state.lock().unwrap();
                if let Some(ref mut transfer) = *zm {
                    let actions = transfer.accept_upload(files);
                    for action in actions {
                        match action {
                            ZmodemAction::SendToRemote(data) => {
                                let _ = write_to_pty(&mut *writer, &data);
                            }
                            ZmodemAction::EmitEvent(event) => {
                                let _ = app.emit(&zmodem_event_name, &event);
                            }
                        }
                    }
                    if transfer.is_done() {
                        *zm = None;
                    }
                }
            }
            SessionCommand::ZmodemCancel => {
                rt_handle.block_on(async {
                    manager.clear_pending_zmodem_upload(&session_id).await;
                });
                let mut zm = zmodem_state.lock().unwrap();
                if let Some(ref mut transfer) = *zm {
                    let actions = transfer.cancel();
                    for action in actions {
                        match action {
                            ZmodemAction::SendToRemote(data) => {
                                let _ = write_to_pty(&mut *writer, &data);
                            }
                            ZmodemAction::EmitEvent(event) => {
                                let _ = app.emit(&zmodem_event_name, &event);
                            }
                        }
                    }
                }
                *zm = None;
            }
            SessionCommand::Close => {
                break;
            }
        }
    }

    {
        let (lock, cvar) = &*output_pause;
        if let Ok(mut paused) = lock.lock() {
            *paused = false;
            cvar.notify_all();
        }
    }
    output.close();

    if let Some(ref rec) = recording_mgr {
        rec.cleanup_session(&session_id);
    }

    rt_handle.block_on(async {
        manager.remove_session(&session_id).await;
    });
    emit_session_closed(&app, &session_id);
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::configure_local_pty_environment;
    #[cfg(target_os = "macos")]
    use super::is_utf8_locale;
    use super::{
        build_local_startup_script_for_platform, parse_shell_args, resolve_shell_command,
        should_emit_visible_output,
    };
    use crate::core::ssh::osc::build_ready_marker;
    #[cfg(target_os = "macos")]
    use portable_pty::CommandBuilder;

    fn ready_marker() -> String {
        build_ready_marker("session-1")
    }

    #[test]
    fn shell_path_with_spaces_stays_single_program() {
        let spec =
            resolve_shell_command(r"D:\Soft wares\Git\bin\bash.exe", "").expect("command spec");

        assert_eq!(spec.program, r"D:\Soft wares\Git\bin\bash.exe");
        assert!(spec.args.is_empty());
    }

    #[test]
    fn shell_args_are_split_separately_from_program() {
        let spec = resolve_shell_command("pwsh.exe", "-NoLogo -NoExit").expect("command spec");

        assert!(
            spec.program.ends_with("pwsh.exe"),
            "program was {}",
            spec.program
        );
        assert_eq!(spec.args, vec!["-NoLogo", "-NoExit"]);
    }

    #[test]
    fn unix_bash_defaults_to_login_interactive_when_args_are_empty() {
        let spec = resolve_shell_command("/bin/bash", "").expect("command spec");

        assert_eq!(spec.program, "/bin/bash");
        if cfg!(windows) {
            assert!(spec.args.is_empty());
        } else {
            assert_eq!(spec.args, vec!["--login", "-i"]);
        }
    }

    #[test]
    fn explicit_shell_args_override_unix_interactive_defaults() {
        let spec = resolve_shell_command("/bin/bash", "--noprofile --norc").expect("command spec");

        assert_eq!(spec.program, "/bin/bash");
        assert_eq!(spec.args, vec!["--noprofile", "--norc"]);
    }

    #[test]
    fn legacy_shell_path_command_with_args_is_still_supported() {
        let spec = resolve_shell_command("pwsh.exe -NoLogo", "").expect("command spec");

        assert!(
            spec.program.ends_with("pwsh.exe"),
            "program was {}",
            spec.program
        );
        assert_eq!(spec.args, vec!["-NoLogo"]);
    }

    #[test]
    fn windows_builtin_shell_names_resolve_to_spawnable_programs() {
        if !cfg!(windows) {
            return;
        }

        for shell in ["cmd.exe", "powershell.exe"] {
            let spec = resolve_shell_command(shell, "").expect("command spec");

            assert!(
                spec.program.contains('\\') || spec.program.contains('/'),
                "{shell} should resolve to an absolute executable path, got {}",
                spec.program
            );
            assert!(
                spec.program.to_ascii_lowercase().ends_with(shell),
                "{shell} resolved to unexpected program {}",
                spec.program
            );
        }
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn windows_terminal_alias_does_not_spawn_wt_host() {
        let spec = resolve_shell_command("wt.exe", "").expect("command spec");

        assert!(
            !spec.program.to_ascii_lowercase().ends_with("wt.exe"),
            "wt.exe should resolve to an embeddable shell, got {}",
            spec.program
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn windows_terminal_profile_commandline_is_parsed_as_shell_spec() {
        let spec = super::shell_spec_from_windows_commandline(
            r#"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe -NoLogo"#,
            vec!["-NoExit".to_string()],
        )
        .expect("command spec");

        assert!(
            spec.program
                .to_ascii_lowercase()
                .ends_with("powershell.exe"),
            "program was {}",
            spec.program
        );
        assert_eq!(spec.args, vec!["-NoLogo", "-NoExit"]);
    }

    #[test]
    fn shell_args_support_quotes_and_windows_paths() {
        let args = parse_shell_args(r#"-Command "echo hi" "C:\Program Files\Tool""#).expect("args");

        assert_eq!(args, vec!["-Command", "echo hi", r"C:\Program Files\Tool"]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn utf8_locale_detection_accepts_common_spellings() {
        assert!(is_utf8_locale("en_US.UTF-8"));
        assert!(is_utf8_locale("zh_CN.utf8"));
        assert!(!is_utf8_locale("C"));
        assert!(!is_utf8_locale("zh_CN.GBK"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn local_pty_environment_sets_terminal_and_utf8_locale() {
        let mut cmd = CommandBuilder::new("/bin/zsh");
        configure_local_pty_environment(&mut cmd);

        assert_eq!(
            cmd.get_env("TERM").and_then(|value| value.to_str()),
            Some("xterm-256color")
        );
        assert!(
            cmd.get_env("LANG")
                .and_then(|value| value.to_str())
                .is_some_and(is_utf8_locale)
        );
        assert!(
            cmd.get_env("LC_CTYPE")
                .and_then(|value| value.to_str())
                .is_some_and(is_utf8_locale)
        );
    }

    #[test]
    fn startup_suppression_hides_chunks_until_ready_marker() {
        let mut suppress_visible = true;

        assert!(!should_emit_visible_output(&mut suppress_visible, false));
        assert!(suppress_visible);
    }

    #[test]
    fn startup_suppression_emits_visible_text_from_ready_chunk() {
        let mut suppress_visible = true;

        assert!(should_emit_visible_output(&mut suppress_visible, true));
        assert!(!suppress_visible);
    }

    #[test]
    fn startup_suppression_emits_subsequent_chunks_after_ready() {
        let mut suppress_visible = true;

        assert!(should_emit_visible_output(&mut suppress_visible, true));
        assert!(should_emit_visible_output(&mut suppress_visible, false));
        assert!(!suppress_visible);
    }

    #[test]
    fn local_startup_does_not_inject_supported_unix_shells() {
        let marker = ready_marker();

        for shell in ["/bin/bash", "/bin/zsh", "/usr/local/bin/fish", "/bin/sh"] {
            let startup = build_local_startup_script_for_platform(shell, &marker, true);

            assert!(!startup.shell_integration_active);
            assert!(
                startup.script.is_none(),
                "{shell} should not receive a startup script"
            );
        }
    }

    #[test]
    fn local_startup_does_not_inject_unknown_or_windows_style_shells() {
        let marker = ready_marker();

        for shell in ["nu", "powershell.exe", r"C:\Windows\System32\cmd.exe"] {
            let startup = build_local_startup_script_for_platform(shell, &marker, true);

            assert!(!startup.shell_integration_active);
            assert!(
                startup.script.is_none(),
                "{shell} should not receive a startup script"
            );
        }
    }

    #[test]
    fn local_startup_does_not_inject_when_unix_prelude_is_disabled() {
        let marker = ready_marker();
        let startup = build_local_startup_script_for_platform("/bin/zsh", &marker, false);

        assert!(!startup.shell_integration_active);
        assert!(startup.script.is_none());
    }
}
