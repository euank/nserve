use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use nix::{
    pty::{openpty, Winsize},
    sys::{ptrace, signal::Signal},
    unistd::Pid,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::discovery::{self, ProcessEvent};

pub struct ChildProcess {
    pub pid: u32,
    pub output: UnboundedReceiver<ProcessOutput>,
    pub events: UnboundedReceiver<ProcessEvent>,
    terminal: Arc<Mutex<File>>,
    exited: Arc<AtomicBool>,
}

#[derive(Debug)]
pub struct ProcessOutput(pub Vec<u8>);

pub struct ChildProcessControl {
    terminal: Arc<Mutex<File>>,
}

struct SpawnedChild {
    child: Child,
    reader: File,
    writer: File,
}

impl ChildProcess {
    pub fn spawn(command: &[OsString], trace_listeners: bool) -> Result<Self> {
        if trace_listeners && !discovery::proc_available() {
            anyhow::bail!("automatic port discovery requires Linux procfs; pass --port explicitly");
        }

        let (events_tx, events) = mpsc::unbounded_channel();
        let exited = Arc::new(AtomicBool::new(false));
        let command = command.to_vec();
        let (mut child, pid, reader, writer) = if trace_listeners {
            // Linux associates ptrace supervision with a particular thread. Keep
            // spawn, waitpid, and all ptrace requests on this dedicated thread.
            let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
            let trace_events = events_tx.clone();
            let trace_exited = exited.clone();
            thread::Builder::new()
                .name("nserve-listen-tracer".into())
                .spawn(move || match spawn_command(&command, true) {
                    Ok(spawned) => {
                        let pid = spawned.child.id();
                        if started_tx
                            .send(Ok((pid, spawned.reader, spawned.writer)))
                            .is_ok()
                        {
                            discovery::trace(pid, trace_events, trace_exited);
                        }
                    }
                    Err(error) => {
                        let _ = started_tx.send(Err(error));
                    }
                })?;
            let (pid, reader, writer) = started_rx
                .recv()
                .context("listener tracer failed to start")??;
            (None, pid, reader, writer)
        } else {
            let spawned = spawn_command(&command, false)?;
            let pid = spawned.child.id();
            (Some(spawned.child), pid, spawned.reader, spawned.writer)
        };

        let terminal = Arc::new(Mutex::new(writer));
        let (output_tx, output) = mpsc::unbounded_channel();
        drain(reader, output_tx);

        if let Some(mut child) = child.take() {
            let events_tx = events_tx;
            let wait_exited = exited.clone();
            thread::Builder::new()
                .name("nserve-child-wait".into())
                .spawn(move || match child.wait() {
                    Ok(status) => {
                        wait_exited.store(true, Ordering::Release);
                        let _ = events_tx.send(ProcessEvent::Exited(status));
                    }
                    Err(error) => {
                        let _ = events_tx.send(ProcessEvent::Error(error.to_string()));
                    }
                })?;
        }

        Ok(Self {
            pid,
            output,
            events,
            terminal,
            exited,
        })
    }

    pub fn signal(&self, signal: Signal) {
        let _ = nix::sys::signal::killpg(Pid::from_raw(self.pid as i32), signal);
    }

    pub fn control(&self) -> Arc<ChildProcessControl> {
        Arc::new(ChildProcessControl {
            terminal: self.terminal.clone(),
        })
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        if self.exited.load(Ordering::Acquire) {
            return;
        }

        self.signal(Signal::SIGTERM);
        if wait_for_exit(&self.exited, Duration::from_secs(1)) {
            return;
        }

        self.signal(Signal::SIGKILL);
        let _ = wait_for_exit(&self.exited, Duration::from_secs(1));
    }
}

fn wait_for_exit(exited: &AtomicBool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !exited.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::park_timeout((deadline - now).min(Duration::from_millis(10)));
    }
    true
}

fn spawn_command(command: &[OsString], trace_listeners: bool) -> Result<SpawnedChild> {
    let pty = openpty(Some(&terminal_size()), None).context("failed to create a pseudoterminal")?;
    let stdin = pty.slave.try_clone().context("failed to clone PTY slave")?;
    let stdout = pty.slave.try_clone().context("failed to clone PTY slave")?;
    let reader = File::from(pty.master);
    let writer = reader.try_clone().context("failed to clone PTY master")?;

    let mut builder = Command::new(&command[0]);
    builder
        .args(&command[1..])
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(pty.slave));

    // SAFETY: after fork this closure only invokes async-signal-safe syscalls.
    // Creating a session gives the child its own process group, and making the
    // PTY its controlling terminal restores normal interactive program
    // behavior. PTRACE_TRACEME produces a SIGTRAP immediately after exec.
    unsafe {
        builder.pre_exec(move || {
            if nix::libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if nix::libc::ioctl(nix::libc::STDIN_FILENO, nix::libc::TIOCSCTTY, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if trace_listeners {
                ptrace::traceme().map_err(|error| io::Error::from_raw_os_error(error as i32))
            } else {
                Ok(())
            }
        });
    }

    let child = builder
        .spawn()
        .with_context(|| format!("failed to run {:?}", command[0]))?;
    Ok(SpawnedChild {
        child,
        reader,
        writer,
    })
}

fn terminal_size() -> Winsize {
    let mut size = Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: ioctl writes a winsize value to the valid pointer supplied.
    let result =
        unsafe { nix::libc::ioctl(nix::libc::STDIN_FILENO, nix::libc::TIOCGWINSZ, &mut size) };
    if result == -1 || size.ws_row == 0 || size.ws_col == 0 {
        size.ws_row = 24;
        size.ws_col = 80;
    }
    size
}

impl ChildProcessControl {
    pub fn write(&self, bytes: &[u8]) {
        let mut writer = self.terminal.lock().expect("child PTY mutex poisoned");
        let _ = writer.write_all(bytes);
        let _ = writer.flush();
    }

    pub fn close_stdin(&self) {
        // A PTY cannot be half-closed while retaining its output side. Send
        // the terminal EOF character instead.
        self.write(&[0x04]);
    }

    pub fn resize(&self) {
        let size = terminal_size();
        let writer = self.terminal.lock().expect("child PTY mutex poisoned");
        // SAFETY: ioctl reads the supplied winsize and the locked file keeps
        // the PTY descriptor valid for the duration of the call.
        unsafe {
            nix::libc::ioctl(writer.as_raw_fd(), nix::libc::TIOCSWINSZ, &size);
        }
    }
}

fn drain(mut reader: impl Read + Send + 'static, output: UnboundedSender<ProcessOutput>) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if output
                        .send(ProcessOutput(buffer[..count].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, time::Duration};

    use super::*;

    static PROCESS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn traces_the_first_listener_opened_by_the_command() {
        let _guard = PROCESS_TEST_LOCK.lock().await;
        let command = [
            OsString::from("python3"),
            OsString::from("-c"),
            OsString::from(
                "import os,socket,time; pid=os.fork(); \
                 s=socket.socket() if pid==0 else None; \
                 s.bind(('127.0.0.1',0)) if pid==0 else None; \
                 s.listen() if pid==0 else None; \
                 print(s.getsockname()[1], flush=True) if pid==0 else None; \
                 time.sleep(30)",
            ),
        ];
        let mut child = ChildProcess::spawn(&command, true).unwrap();
        let mut announced = None;
        let mut observed = None;

        tokio::time::timeout(Duration::from_secs(10), async {
            while announced.is_none() || observed.is_none() {
                tokio::select! {
                    Some(output) = child.output.recv() => {
                        announced = String::from_utf8(output.0).ok()
                            .and_then(|value| value.trim().parse::<u16>().ok());
                    }
                    Some(event) = child.events.recv() => match event {
                        ProcessEvent::Listening(port) => observed = Some(port),
                        ProcessEvent::Error(error) => panic!("trace failed: {error}"),
                        ProcessEvent::Exited(status) => panic!("child exited early: {status}"),
                    }
                }
            }
        })
        .await
        .expect("listener discovery timed out");

        assert_eq!(observed, announced);
        child.signal(Signal::SIGTERM);
    }

    #[tokio::test]
    async fn child_runs_with_a_controlling_pseudoterminal() {
        let _guard = PROCESS_TEST_LOCK.lock().await;
        let command = [
            OsString::from("python3"),
            OsString::from("-c"),
            OsString::from(
                "import os; print(os.isatty(0), os.isatty(1), os.isatty(2), \
                 os.tcgetpgrp(0) == os.getpgrp(), \
                 min(os.get_terminal_size(0)) > 0, flush=True)",
            ),
        ];
        let mut child = ChildProcess::spawn(&command, false).unwrap();
        let output = tokio::time::timeout(Duration::from_secs(5), child.output.recv())
            .await
            .expect("child output timed out")
            .expect("child output channel closed");

        assert_eq!(
            String::from_utf8_lossy(&output.0).trim(),
            "True True True True True"
        );
    }

    #[tokio::test]
    async fn dropping_a_running_child_terminates_and_reaps_it() {
        let _guard = PROCESS_TEST_LOCK.lock().await;
        let command = [
            OsString::from("python3"),
            OsString::from("-c"),
            OsString::from("import time; time.sleep(30)"),
        ];
        let child = ChildProcess::spawn(&command, false).unwrap();
        let pid = Pid::from_raw(child.pid as i32);

        drop(child);

        assert_eq!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH)
        );
    }
}
