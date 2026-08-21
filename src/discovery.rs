//! Linux listener discovery synchronized at syscall boundaries with `ptrace`.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::Path,
    process::ExitStatus,
};

use nix::{
    sys::{
        ptrace::{self, Options},
        signal::Signal,
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::Pid,
};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum ProcessEvent {
    Listening(u16),
    Exited(ExitStatus),
    Error(String),
}

pub fn trace(root: u32, events: UnboundedSender<ProcessEvent>) {
    let root = Pid::from_raw(root as i32);
    let result = trace_inner(root, &events);
    if let Err(error) = result {
        let _ = events.send(ProcessEvent::Error(error.to_string()));
    }
}

fn trace_inner(root: Pid, events: &UnboundedSender<ProcessEvent>) -> anyhow::Result<()> {
    let initial = waitpid(root, None)?;
    if !matches!(initial, WaitStatus::Stopped(_, Signal::SIGTRAP)) {
        anyhow::bail!("child did not enter its expected pre-exec trace stop: {initial:?}");
    }

    ptrace::setoptions(
        root,
        Options::PTRACE_O_TRACESYSGOOD
            | Options::PTRACE_O_TRACEFORK
            | Options::PTRACE_O_TRACEVFORK
            | Options::PTRACE_O_TRACECLONE
            | Options::PTRACE_O_TRACEEXEC
            | Options::PTRACE_O_EXITKILL,
    )?;

    let mut tracees = HashSet::from([root]);
    let mut listen_calls = HashMap::new();
    let mut discovering = true;
    ptrace::syscall(root, None)?;

    while !tracees.is_empty() {
        let status = waitpid(Pid::from_raw(-1), Some(WaitPidFlag::__WALL))?;
        match status {
            WaitStatus::PtraceSyscall(pid) => {
                if discovering {
                    if let Some(port) = observe_listen(pid, &mut listen_calls)? {
                        discovering = false;
                        let _ = events.send(ProcessEvent::Listening(port));
                    }
                }
                resume(pid, discovering, None)?;
            }
            WaitStatus::PtraceEvent(pid, _, event) => {
                if matches!(
                    event,
                    libc_event::FORK | libc_event::VFORK | libc_event::CLONE
                ) {
                    let child = Pid::from_raw(ptrace::getevent(pid)? as i32);
                    tracees.insert(child);
                }
                resume(pid, discovering, None)?;
            }
            WaitStatus::Stopped(pid, signal) => {
                tracees.insert(pid);
                let deliver = match signal {
                    Signal::SIGSTOP | Signal::SIGTRAP => None,
                    signal => Some(signal),
                };
                resume(pid, discovering, deliver)?;
            }
            WaitStatus::Exited(pid, code) => {
                tracees.remove(&pid);
                if pid == root {
                    let _ = events.send(ProcessEvent::Exited(ExitStatus::from_raw(code << 8)));
                    return Ok(());
                }
            }
            WaitStatus::Signaled(pid, signal, core_dumped) => {
                tracees.remove(&pid);
                if pid == root {
                    let raw = signal as i32 | if core_dumped { 0x80 } else { 0 };
                    let _ = events.send(ProcessEvent::Exited(ExitStatus::from_raw(raw)));
                    return Ok(());
                }
            }
            WaitStatus::Continued(_) | WaitStatus::StillAlive => {}
        }
    }
    Ok(())
}

fn observe_listen(pid: Pid, calls: &mut HashMap<Pid, u32>) -> anyhow::Result<Option<u16>> {
    let info = syscall_info(pid)?;
    match info.op {
        nix::libc::PTRACE_SYSCALL_INFO_ENTRY => {
            // SAFETY: the kernel sets the `entry` union member when op says this
            // is a syscall-entry stop.
            let entry = unsafe { info.u.entry };
            if entry.nr == nix::libc::SYS_listen as u64 {
                calls.insert(pid, entry.args[0] as u32);
            }
            Ok(None)
        }
        nix::libc::PTRACE_SYSCALL_INFO_EXIT => {
            let Some(fd) = calls.remove(&pid) else {
                return Ok(None);
            };
            // SAFETY: the kernel sets the `exit` union member when op says this
            // is a syscall-exit stop.
            let exit = unsafe { info.u.exit };
            if exit.is_error == 0 {
                socket_port(pid.as_raw(), fd).map_err(Into::into)
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

fn syscall_info(pid: Pid) -> io::Result<nix::libc::ptrace_syscall_info> {
    let mut info = std::mem::MaybeUninit::<nix::libc::ptrace_syscall_info>::zeroed();
    // SAFETY: PTRACE_GET_SYSCALL_INFO writes at most the size supplied as its
    // third argument to the valid pointer supplied as its fourth argument.
    let result = unsafe {
        nix::libc::ptrace(
            nix::libc::PTRACE_GET_SYSCALL_INFO,
            pid.as_raw(),
            std::mem::size_of::<nix::libc::ptrace_syscall_info>(),
            info.as_mut_ptr(),
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: the allocation was zero-initialized and the kernel populated
        // all fields included in the returned syscall-info variant.
        Ok(unsafe { info.assume_init() })
    }
}

fn resume(pid: Pid, discovering: bool, signal: Option<Signal>) -> nix::Result<()> {
    if discovering {
        ptrace::syscall(pid, signal)
    } else {
        ptrace::cont(pid, signal)
    }
}

fn socket_port(pid: i32, fd: u32) -> io::Result<Option<u16>> {
    let target = fs::read_link(format!("/proc/{pid}/fd/{fd}"))?;
    let target = target.to_string_lossy();
    let Some(inode) = target
        .strip_prefix("socket:[")
        .and_then(|value| value.strip_suffix(']'))
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return Ok(None);
    };
    Ok(listener_inodes(pid)?.get(&inode).copied())
}

fn listener_inodes(pid: i32) -> io::Result<HashMap<u64, u16>> {
    let mut listeners = HashMap::new();
    for name in ["tcp", "tcp6"] {
        let path = format!("/proc/{pid}/net/{name}");
        match fs::read_to_string(path) {
            Ok(contents) => parse_tcp_table(&contents, &mut listeners),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(listeners)
}

fn parse_tcp_table(contents: &str, listeners: &mut HashMap<u64, u16>) {
    for line in contents.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 || fields[3] != "0A" {
            continue;
        }
        let Some(port_hex) = fields[1].rsplit_once(':').map(|(_, port)| port) else {
            continue;
        };
        if let (Ok(port), Ok(inode)) = (u16::from_str_radix(port_hex, 16), fields[9].parse::<u64>())
        {
            if port != 0 {
                listeners.insert(inode, port);
            }
        }
    }
}

pub fn proc_available() -> bool {
    Path::new("/proc/self/fd").is_dir() && Path::new("/proc/net/tcp").is_file()
}

use std::os::unix::process::ExitStatusExt;

mod libc_event {
    pub const FORK: i32 = 1;
    pub const VFORK: i32 = 2;
    pub const CLONE: i32 = 3;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_listening_tcp_rows() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
          0: 0100007F:1770 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 12345 1\n\
          1: 0100007F:0BB8 0100007F:C001 01 00000000:00000000 00:00000000 00000000 1000 0 99999 1\n";
        let mut listeners = HashMap::new();
        parse_tcp_table(table, &mut listeners);
        assert_eq!(listeners.get(&12345), Some(&6000));
        assert!(!listeners.contains_key(&99999));
    }
}
