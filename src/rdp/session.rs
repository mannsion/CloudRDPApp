//! Running one FreeRDP process and reporting what happens to it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{oneshot, watch};

use super::args::{LaunchPlan, args_payload};
use super::freerdp;
use crate::paths;
use crate::util;

/// File descriptor on which FreeRDP reads its arguments (`/args-from:fd:3`).
///
/// Not standard input: FreeRDP closes the stream it read the arguments from
/// (`args_from_fp` in `client/common/cmdline.c`). If that is stdin, WinPR's
/// `CreateProcess` later refuses to start the sign-in helper, because the
/// helper inherits FreeRDP's (now closed) standard input.
pub const ARGS_FD: i32 = 3;

/// What to launch and where to keep its files.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub plan: LaunchPlan,
    /// Private directory for this session's connection file.
    pub dir: PathBuf,
    /// File FreeRDP writes its log to.
    pub log_file: PathBuf,
    /// Extra environment: helper socket credentials, certificate bundle.
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// A line of FreeRDP output, already redacted.
    Log(String),
    /// The remote desktop is up.
    Connected,
    /// The process ended. `code` is `None` when it was killed by a signal.
    Exited { code: Option<i32> },
}

pub type EventSink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// Handle to a running session.
#[derive(Debug)]
pub struct SessionControl {
    stop: Option<oneshot::Sender<()>>,
    pub log_file: PathBuf,
}

impl SessionControl {
    /// Ask FreeRDP to disconnect. Safe to call more than once.
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for SessionControl {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start FreeRDP. Must be called inside a Tokio runtime. Events are
/// delivered on runtime threads until [`SessionEvent::Exited`].
pub fn spawn(spec: SessionSpec, events: EventSink) -> Result<SessionControl, String> {
    paths::create_private_dir(&spec.dir).map_err(|err| format!("{}: {err}", spec.dir.display()))?;
    let rdp_path = spec.dir.join("connection.rdp");
    paths::write_private_file(&rdp_path, spec.plan.rdp.serialize().as_bytes())
        .map_err(|err| format!("{}: {err}", rdp_path.display()))?;
    let payload = args_payload(&rdp_path, &spec.plan.args)?;
    let (args_read, args_write) = cloexec_pipe().map_err(|err| format!("could not create a pipe for FreeRDP: {err}"))?;

    let log_dir = spec.log_file.parent().unwrap_or(Path::new(".")).to_path_buf();
    std::fs::create_dir_all(&log_dir).map_err(|err| format!("{}: {err}", log_dir.display()))?;
    let log_name = spec.log_file.file_name().and_then(|n| n.to_str()).ok_or("invalid log file name")?.to_owned();
    // Start from an empty log so the tail below never replays an old session.
    paths::write_private_file(&spec.log_file, b"").map_err(|err| format!("{}: {err}", spec.log_file.display()))?;

    let mut command = Command::new(&spec.plan.program);
    command
        .arg(format!("/args-from:fd:{ARGS_FD}"))
        .envs(spec.plan.env.iter().cloned())
        .envs(spec.env.iter().cloned())
        // WLog's console output is block-buffered on a pipe; the file
        // appender flushes every message, so status shows up immediately.
        .env("WLOG_APPENDER", "file")
        .env("WLOG_FILEAPPENDER_OUTPUT_FILE_PATH", &log_dir)
        .env("WLOG_FILEAPPENDER_OUTPUT_FILE_NAME", &log_name)
        // FreeRDP's terminal fallback for sign-in reads stdin; there is no terminal.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let read_fd = std::os::fd::AsRawFd::as_raw_fd(&args_read);
    // SAFETY: only async-signal-safe calls (fcntl, dup2) between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if read_fd == ARGS_FD {
                let flags = libc::fcntl(read_fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(read_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(read_fd, ARGS_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .map_err(|err| format!("could not start {}: {err}", spec.plan.program.display()))?;
    // The child holds its own copy; FreeRDP sees EOF once the writer closes.
    drop(args_read);
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let (done_tx, done_rx) = watch::channel(false);
    let log_file = spec.log_file.clone();
    let session_dir = spec.dir.clone();

    tokio::spawn(async move {
        // Hand over the arguments, then close the pipe to end the list.
        // Run independently so a stalled child cannot block stop_rx.
        let writer_events = events.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut pipe = std::fs::File::from(args_write);
            if let Err(err) = pipe.write_all(payload.as_bytes()) {
                writer_events(SessionEvent::Log(format!("could not pass the options to FreeRDP: {err}")));
            }
        });

        let mut readers = Vec::new();
        if let Some(stdout) = stdout {
            readers.push(tokio::spawn(forward_lines(stdout, events.clone())));
        }
        if let Some(stderr) = stderr {
            readers.push(tokio::spawn(forward_lines(stderr, events.clone())));
        }
        let tail = tokio::spawn(tail_log(log_file, events.clone(), done_rx));

        let status = tokio::select! {
            status = child.wait() => status,
            _ = &mut stop_rx => {
                // SIGTERM lets FreeRDP send a clean disconnect; SIGKILL follows if it lingers.
                if let Some(pid) = pid {
                    // SAFETY: plain signal delivery to the child we spawned.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                }
                match tokio::time::timeout(Duration::from_secs(4), child.wait()).await {
                    Ok(status) => status,
                    Err(_) => {
                        let _ = child.kill().await;
                        child.wait().await
                    }
                }
            }
        };

        for reader in readers {
            let _ = tokio::time::timeout(Duration::from_secs(2), reader).await;
        }
        let _ = done_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), tail).await;

        // The connection file names the gateway and host pool; do not leave it behind.
        let _ = tokio::fs::remove_dir_all(&session_dir).await;
        events(SessionEvent::Exited { code: status.ok().and_then(|s| s.code()) });
    });

    Ok(SessionControl { stop: Some(stop_tx), log_file: spec.log_file })
}

/// A pipe whose ends are not inherited by child processes.
fn cloexec_pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe2 writes two descriptors into the array on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned by nobody else.
    Ok(unsafe { (std::os::fd::OwnedFd::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1])) })
}

async fn forward_lines<R: tokio::io::AsyncRead + Unpin>(reader: R, events: EventSink) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim_end();
        if !line.is_empty() {
            emit_line(line, &events);
        }
    }
}

fn emit_line(line: &str, events: &EventSink) {
    if freerdp::is_connected_marker(line) {
        events(SessionEvent::Connected);
    }
    events(SessionEvent::Log(util::redact(line)));
}

/// Follow FreeRDP's log file until `done` flips, then drain what is left.
async fn tail_log(path: PathBuf, events: EventSink, mut done: watch::Receiver<bool>) {
    let mut file: Option<tokio::fs::File> = None;
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let finished = *done.borrow();
        if file.is_none() {
            file = tokio::fs::File::open(&path).await.ok();
        }
        if let Some(f) = file.as_mut() {
            let mut chunk = Vec::new();
            if let Ok(read) = f.read_to_end(&mut chunk).await
                && read > 0
            {
                pending.extend_from_slice(&chunk);
            }
        }
        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end();
            if !text.is_empty() {
                emit_line(text, &events);
            }
        }
        if finished {
            if !pending.is_empty() {
                emit_line(String::from_utf8_lossy(&pending).trim_end(), &events);
            }
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            _ = done.changed() => {}
        }
    }
}

/// A log file name for a new session inside `dir`.
pub fn log_file_in(dir: &Path, title: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dir.join(format!("{stamp}-{}-{}.log", util::file_safe(title), util::random_token(3)))
}

/// Delete all but the newest `keep` session logs.
pub fn prune_logs(keep: usize) {
    let Ok(entries) = std::fs::read_dir(paths::logs_dir()) else { return };
    let mut logs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("log"))
        .collect();
    logs.sort();
    let excess = logs.len().saturating_sub(keep);
    for path in logs.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdp::file::RdpFile;
    use std::sync::Mutex;

    fn collector() -> (EventSink, Arc<Mutex<Vec<SessionEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let sink: EventSink = Arc::new(move |event| sink_seen.lock().unwrap().push(event));
        (sink, seen)
    }

    async fn wait_for_exit(seen: &Arc<Mutex<Vec<SessionEvent>>>) -> Option<i32> {
        for _ in 0..200 {
            if let Some(SessionEvent::Exited { code }) =
                seen.lock().unwrap().iter().find(|e| matches!(e, SessionEvent::Exited { .. })).cloned()
            {
                return code;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the session never exited: {:?}", seen.lock().unwrap());
    }

    /// A stand-in for FreeRDP: a shell script that reads its arguments from
    /// fd 3 the way `/args-from:fd:3` does and writes a WLog-style file.
    fn fake_client(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("fake-freerdp.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn spec(root: &Path, program: PathBuf, args: &[&str]) -> SessionSpec {
        SessionSpec {
            plan: LaunchPlan {
                program,
                args: args.iter().map(|a| a.to_string()).collect(),
                rdp: RdpFile::parse("full address:s:host\n"),
                env: vec![("PLAN_ENV".into(), "from-plan".into())],
                entra_sso: true,
                remote_app: false,
            },
            dir: root.join("session"),
            log_file: root.join("logs").join("session.log"),
            env: vec![("EXTRA_ENV".into(), "from-spec".into())],
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passes_arguments_on_a_pipe_and_reports_events() {
        let root = std::env::temp_dir().join(format!("cloudrdpapp-session-{}", util::random_token(6)));
        let script = r#"
[ "$1" = "/args-from:fd:3" ] || exit 90
IFS= read -r rdp <&3
IFS= read -r first <&3
IFS= read -r second <&3
# Standard input is not the argument pipe.
IFS= read -r nothing && exit 91
log="$WLOG_FILEAPPENDER_OUTPUT_FILE_PATH/$WLOG_FILEAPPENDER_OUTPUT_FILE_NAME"
echo "rdp=$(cat "$rdp" | tr -d '\r')" >> "$log"
echo "args=$first|$second env=$PLAN_ENV,$EXTRA_ENV appender=$WLOG_APPENDER" >> "$log"
echo "[INFO][com.freerdp.gdi] - [gdi_init_ex]: Local framebuffer format  PIXEL_FORMAT_BGRX32" >> "$log"
echo "plain stdout line"
echo "token leak code=SECRETCODE&state=1" >&2
exit 7
"#;
        let program = fake_client(&root, script);
        let (sink, seen) = collector();
        let control = spawn(spec(&root, program, &["/f", "/u:alex@contoso.com"]), sink).unwrap();
        assert_eq!(wait_for_exit(&seen).await, Some(7));

        let events = seen.lock().unwrap().clone();
        let logs: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Log(line) => Some(line.as_str()),
                _ => None,
            })
            .collect();
        assert!(logs.contains(&"rdp=full address:s:host"), "{logs:?}");
        assert!(logs.contains(&"args=/f|/u:alex@contoso.com env=from-plan,from-spec appender=file"), "{logs:?}");
        assert!(logs.contains(&"plain stdout line"));
        assert!(logs.iter().any(|l| l.contains("code=<redacted>&state=1")), "{logs:?}");
        assert!(!logs.iter().any(|l| l.contains("SECRETCODE")));
        assert!(events.contains(&SessionEvent::Connected));
        assert!(matches!(events.last(), Some(SessionEvent::Exited { code: Some(7) })));

        // The connection file is removed; the log stays for troubleshooting.
        assert!(!root.join("session").exists());
        assert!(control.log_file.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_terminates_the_process() {
        let root = std::env::temp_dir().join(format!("cloudrdpapp-session-{}", util::random_token(6)));
        let program = fake_client(&root, "while IFS= read -r line <&3; do :; done\nexec sleep 60");
        let (sink, seen) = collector();
        let mut control = spawn(spec(&root, program, &["/f"]), sink).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        control.stop();
        control.stop();
        // Ended by SIGTERM, so there is no exit code.
        assert_eq!(wait_for_exit(&seen).await, None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_program_is_an_error() {
        let root = std::env::temp_dir().join(format!("cloudrdpapp-session-{}", util::random_token(6)));
        let (sink, _) = collector();
        let err = spawn(spec(&root, root.join("does-not-exist"), &[]), sink).unwrap_err();
        assert!(err.contains("could not start"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
