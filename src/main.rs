use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::{Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process;
use std::time::Duration;

/// Tried in order — first match wins. Supports both the rebranded Atoll app
/// and the legacy Claude Island app that may still be installed.
const SOCKET_PATHS: &[&str] = &["/tmp/claude-atoll.sock", "/tmp/claude-island.sock"];
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const PERMISSION_RECV_TIMEOUT: Duration = Duration::from_secs(300);

fn is_debug() -> bool {
    std::env::var("CLAUDE_ATOLL_DEBUG").as_deref() == Ok("1")
}

macro_rules! dbg_log {
    ($($arg:tt)*) => {
        if is_debug() {
            eprintln!("[claude-atoll] {}", format!($($arg)*));
        }
    };
}

// MARK: - Data types

#[derive(Deserialize)]
struct HookEvent {
    session_id: Option<String>,
    hook_event_name: Option<String>,
    cwd: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_use_id: Option<String>,
    notification_type: Option<String>,
    message: Option<String>,
}

#[derive(Serialize)]
struct Payload {
    session_id: String,
    cwd: String,
    event: String,
    pid: u32,
    tty: Option<String>,
    tty_valid: bool,
    session_active: bool,
    status: String,
    tool_input: Map<String, Value>, // always present, even as {}
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notification_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Deserialize)]
struct PermissionResponse {
    decision: Option<String>,
    reason: Option<String>,
}

// MARK: - Status extras (builder)

struct Extras {
    status: &'static str,
    tool: Option<String>,
    tool_input: Option<Value>,
    tool_use_id: Option<String>,
    notification_type: Option<String>,
    message: Option<String>,
}

impl Extras {
    fn new(status: &'static str) -> Self {
        Self {
            status,
            tool: None,
            tool_input: None,
            tool_use_id: None,
            notification_type: None,
            message: None,
        }
    }

    fn with_tool_extras(mut self, ev: &HookEvent) -> Self {
        self.tool = ev.tool_name.clone();
        self.tool_input = ev.tool_input.clone();
        self.tool_use_id = ev.tool_use_id.clone();
        self
    }

    fn with_notification(mut self, ev: &HookEvent) -> Self {
        self.notification_type = ev.notification_type.clone();
        self.message = ev.message.clone();
        self
    }
}

// MARK: - Status determination

fn determine_status(event: &str, ev: &HookEvent) -> Extras {
    match event {
        "UserPromptSubmit" => Extras::new("processing"),
        // No longer registered on PreToolUse (Claude Code bug #15897) — skip harmlessly
        "PreToolUse" => Extras::new("skip"),
        "PostToolUse" => Extras::new("processing").with_tool_extras(ev),
        "PermissionRequest" => Extras::new("waiting_for_approval").with_tool_extras(ev),
        "Notification" => match ev.notification_type.as_deref() {
            // Handled by PermissionRequest hook with better info
            Some("permission_prompt") => Extras::new("skip"),
            Some("idle_prompt") => Extras::new("waiting_for_input").with_notification(ev),
            _ => Extras::new("notification").with_notification(ev),
        },
        "Stop" => Extras::new("waiting_for_input"),
        "SubagentStop" => Extras::new("processing"),
        "SessionStart" => Extras::new("waiting_for_input"),
        "SessionEnd" => Extras::new("ended"),
        "PreCompact" => Extras::new("compacting"),
        _ => Extras::new("unknown"),
    }
}

// MARK: - Process tree

// Walk the process tree upward until we find the process named "claude".
// When hooks run via uv, the immediate parent is uv (not claude), so we walk up.
fn get_claude_pid() -> u32 {
    let mut current = process::id();
    for _ in 0..10 {
        let Ok(out) = process::Command::new("ps")
            .args(["-p", &current.to_string(), "-o", "ppid=,comm="])
            .output()
        else {
            break;
        };
        if !out.status.success() {
            break;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut parts = text.split_whitespace();
        let (Some(ppid_str), Some(comm)) = (parts.next(), parts.next()) else {
            break;
        };
        let Ok(ppid) = ppid_str.parse::<u32>() else {
            break;
        };
        if comm.eq_ignore_ascii_case("claude") {
            return current;
        }
        current = ppid;
    }
    // Fallback: immediate parent
    unsafe { libc::getppid() as u32 }
}

// MARK: - TTY helpers

fn get_tty(pid: u32) -> Option<String> {
    let out = process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "tty="])
        .output()
        .ok()?;
    let tty = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !tty.is_empty() && tty != "??" && tty != "-" {
        return Some(if tty.starts_with("/dev/") { tty } else { format!("/dev/{tty}") });
    }
    // Fallback: check our own stdin/stdout
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
        let mut buf = vec![0u8; 64];
        let ret =
            unsafe { libc::ttyname_r(fd, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if ret == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..end]) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn validate_tty(tty: &str) -> bool {
    let path = Path::new(tty);
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.file_type().is_char_device() {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(tty) else {
        return false;
    };
    unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 }
}

fn is_session_active(pid: u32, tty: Option<&str>) -> bool {
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    // EPERM means process exists but we lack permission to signal it — still active
    if ret != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        return false;
    }
    if let Some(t) = tty {
        if !validate_tty(t) {
            return false;
        }
    }
    true
}

fn normalize_tool_input(v: Option<Value>) -> Map<String, Value> {
    match v {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

// MARK: - Socket I/O

fn send_event(payload: &Payload) -> Option<PermissionResponse> {
    let json = serde_json::to_vec(payload).ok()?;

    // Try each socket path in order; use the first one that connects.
    let (mut stream, socket_path) = match SOCKET_PATHS
        .iter()
        .find_map(|path| UnixStream::connect(path).ok().map(|s| (s, *path)))
    {
        Some(pair) => pair,
        None => {
            dbg_log!(
                "connect error: no socket found (tried: {})",
                SOCKET_PATHS.join(", ")
            );
            return None;
        }
    };
    dbg_log!("connected to {socket_path}");

    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    if let Err(e) = stream.write_all(&json) {
        dbg_log!("write error: {e}");
        return None;
    }
    dbg_log!("sent {} bytes (event={}, status={})", json.len(), payload.event, payload.status);

    if payload.status != "waiting_for_approval" {
        return None;
    }

    let _ = stream.set_read_timeout(Some(PERMISSION_RECV_TIMEOUT));
    dbg_log!("waiting for permission response...");

    // Loop until EOF — Swift closes the socket after writing the response.
    // read_to_end handles partial recvs internally; on timeout it returns Err.
    let mut buf = Vec::new();
    let read_result = stream.read_to_end(&mut buf);

    if buf.is_empty() {
        if let Err(e) = read_result {
            dbg_log!("read error: {e}");
        } else {
            dbg_log!("empty response (socket closed)");
        }
        return None;
    }

    dbg_log!("received {} bytes: {}", buf.len(), String::from_utf8_lossy(&buf));
    match serde_json::from_slice(&buf) {
        Ok(r) => Some(r),
        Err(e) => {
            dbg_log!("parse error: {e}");
            None
        }
    }
}

// MARK: - Permission response handler

fn handle_permission_response(response: Option<PermissionResponse>) {
    let Some(resp) = response else {
        println!("{{}}");
        return;
    };
    match resp.decision.as_deref().unwrap_or("ask") {
        "allow" => {
            let out = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {"behavior": "allow"}
                }
            });
            println!("{out}");
            process::exit(0);
        }
        "deny" => {
            let reason = resp.reason.as_deref().unwrap_or("Denied by user via ClaudeAtoll");
            let out = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {"behavior": "deny", "message": reason}
                }
            });
            println!("{out}");
            process::exit(0);
        }
        other => {
            dbg_log!("unknown decision: {other}");
            println!("{{}}");
        }
    }
}

// MARK: - Entry point

fn main() {
    // Ignore SIGPIPE so a broken socket write doesn't terminate the process
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let ev: HookEvent = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(v) => v,
        Err(_) => process::exit(1),
    };

    let session_id = ev.session_id.clone().unwrap_or_else(|| "unknown".to_string());
    let event = ev.hook_event_name.clone().unwrap_or_default();
    let cwd = ev.cwd.clone().unwrap_or_default();

    let extras = determine_status(&event, &ev);
    let status = extras.status;

    dbg_log!(
        "event={event} session={} status={status}",
        &session_id[..session_id.len().min(8)]
    );

    if status == "skip" {
        println!("{{}}");
        return;
    }

    let claude_pid = get_claude_pid();
    let tty = get_tty(claude_pid);
    let tty_valid = tty.as_deref().map(validate_tty).unwrap_or(false);
    let session_active = is_session_active(claude_pid, tty.as_deref());

    let payload = Payload {
        session_id,
        cwd,
        event,
        pid: claude_pid,
        tty: tty.clone(),
        tty_valid,
        session_active,
        status: status.to_string(),
        tool_input: normalize_tool_input(extras.tool_input),
        tool: extras.tool,
        tool_use_id: extras.tool_use_id,
        notification_type: extras.notification_type,
        message: extras.message,
    };

    let response = send_event(&payload);

    if status == "waiting_for_approval" {
        handle_permission_response(response);
    } else {
        println!("{{}}");
    }
}
