use std::{
    ffi::OsString,
    io::{self, Read, Write},
    os::unix::process::CommandExt,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

use anyhow::{Context, Result};
use nix::{
    sys::{ptrace, signal::Signal},
    unistd::Pid,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::discovery::{self, ProcessEvent};

pub struct ChildProcess {
    pub pid: u32,
    pub stdin: Arc<Mutex<Option<ChildStdin>>>,
    pub output: UnboundedReceiver<ProcessOutput>,
    pub events: UnboundedReceiver<ProcessEvent>,
}

#[derive(Debug)]
pub enum ProcessOutput {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

pub struct ChildProcessControl {
    pid: u32,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

impl ChildProcess {
    pub fn spawn(command: &[OsString], trace_listeners: bool) -> Result<Self> {
        if trace_listeners && !discovery::proc_available() {
            anyhow::bail!("automatic port discovery requires Linux procfs; pass --port explicitly");
        }

        let (events_tx, events) = mpsc::unbounded_channel();
        let command = command.to_vec();
        let (mut child, pid, child_stdin, child_stdout, child_stderr) = if trace_listeners {
            // Linux associates ptrace supervision with a particular thread. Keep
            // spawn, waitpid, and all ptrace requests on this dedicated thread.
            let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
            let trace_events = events_tx.clone();
            thread::Builder::new()
                .name("nserve-listen-tracer".into())
                .spawn(move || match spawn_command(&command, true) {
                    Ok(mut child) => {
                        let pid = child.id();
                        let parts = take_parts(&mut child);
                        if started_tx.send(Ok((pid, parts))).is_ok() {
                            discovery::trace(pid, trace_events);
                        }
                    }
                    Err(error) => {
                        let _ = started_tx.send(Err(error));
                    }
                })?;
            let (pid, parts) = started_rx
                .recv()
                .context("listener tracer failed to start")??;
            (None, pid, parts.0, parts.1, parts.2)
        } else {
            let mut child = spawn_command(&command, false)?;
            let pid = child.id();
            let parts = take_parts(&mut child);
            (Some(child), pid, parts.0, parts.1, parts.2)
        };

        let stdin = Arc::new(Mutex::new(Some(child_stdin)));
        let (output_tx, output) = mpsc::unbounded_channel();
        drain(child_stdout, output_tx.clone(), ProcessOutput::Stdout);
        drain(child_stderr, output_tx, ProcessOutput::Stderr);

        if let Some(mut child) = child.take() {
            let events_tx = events_tx;
            thread::Builder::new()
                .name("nserve-child-wait".into())
                .spawn(move || match child.wait() {
                    Ok(status) => {
                        let _ = events_tx.send(ProcessEvent::Exited(status));
                    }
                    Err(error) => {
                        let _ = events_tx.send(ProcessEvent::Error(error.to_string()));
                    }
                })?;
        }

        Ok(Self {
            pid,
            stdin,
            output,
            events,
        })
    }

    pub fn signal(&self, signal: Signal) {
        let _ = nix::sys::signal::killpg(Pid::from_raw(self.pid as i32), signal);
    }

    pub fn control(&self) -> Arc<ChildProcessControl> {
        Arc::new(ChildProcessControl {
            pid: self.pid,
            stdin: self.stdin.clone(),
        })
    }
}

fn spawn_command(command: &[OsString], trace_listeners: bool) -> Result<Child> {
    let mut builder = Command::new(&command[0]);
    builder
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    if trace_listeners {
        // SAFETY: after fork this closure only invokes ptrace. PTRACE_TRACEME
        // produces a SIGTRAP immediately after exec, before command code can
        // call listen(2).
        unsafe {
            builder.pre_exec(|| {
                ptrace::traceme().map_err(|error| io::Error::from_raw_os_error(error as i32))
            });
        }
    }
    builder
        .spawn()
        .with_context(|| format!("failed to run {:?}", command[0]))
}

fn take_parts(child: &mut Child) -> (ChildStdin, ChildStdout, ChildStderr) {
    (
        child.stdin.take().expect("piped stdin"),
        child.stdout.take().expect("piped stdout"),
        child.stderr.take().expect("piped stderr"),
    )
}

impl ChildProcessControl {
    pub fn signal(&self, signal: Signal) {
        let _ = nix::sys::signal::killpg(Pid::from_raw(self.pid as i32), signal);
    }

    pub fn write(&self, bytes: &[u8]) {
        if let Some(writer) = self
            .stdin
            .lock()
            .expect("child stdin mutex poisoned")
            .as_mut()
        {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    pub fn close_stdin(&self) {
        self.stdin
            .lock()
            .expect("child stdin mutex poisoned")
            .take();
    }
}

fn drain(
    mut reader: impl Read + Send + 'static,
    output: UnboundedSender<ProcessOutput>,
    wrap: fn(Vec<u8>) -> ProcessOutput,
) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if output.send(wrap(buffer[..count].to_vec())).is_err() {
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

    #[tokio::test]
    async fn traces_the_first_listener_opened_by_the_command() {
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
                        if let ProcessOutput::Stdout(bytes) = output {
                            announced = String::from_utf8(bytes).ok()
                                .and_then(|value| value.trim().parse::<u16>().ok());
                        }
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
}
