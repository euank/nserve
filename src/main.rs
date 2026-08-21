mod auth;
mod cli;
mod discovery;
mod ingress;
mod process;
mod proxy;
mod state;
mod terminal;

use std::{
    io::{self, Write},
    os::unix::process::ExitStatusExt,
    process::ExitStatus,
};

use anyhow::{Context, Result};
use clap::Parser;
use discovery::ProcessEvent;
use nix::sys::signal::Signal;
use process::{ChildProcess, ProcessOutput};
use tokio::{sync::watch, task::JoinHandle};

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("[nserve] {error:#}");
            1
        }
    };
    std::process::exit(code);
}

async fn run() -> Result<i32> {
    let cli = cli::Cli::parse();
    cli.validate()?;

    let endpoint = ingress::connect(&cli).await?;
    let public_url = endpoint.url().to_owned();
    println!("[nserve] Created ngrok session with URL {public_url}");
    io::stdout().flush()?;

    let state = state::AppState::new(public_url.clone());
    let (upstream_tx, upstream_rx) = watch::channel(None);
    let mut forwarder = tokio::spawn(proxy::run(endpoint, upstream_rx, state.clone()));

    if cli.open {
        if let Err(error) = open::that(&public_url) {
            eprintln!("[nserve] could not open browser: {error}");
        }
    }

    let auto_port = cli.port.is_none();
    let mut child = ChildProcess::spawn(&cli.command, auto_port)?;
    let _raw_mode = terminal::RawMode::enable_if_terminal()
        .context("failed to enable terminal raw mode for ctrl+n commands")?;
    let input = tokio::spawn(terminal::input_loop(child.control(), state.clone()));

    let port = match cli.port {
        Some(port) => port,
        None => wait_for_listener(&mut child, &mut forwarder).await?,
    };
    state.set_local_port(port);
    upstream_tx.send_replace(Some(port));

    let exit = loop {
        tokio::select! {
            Some(output) = child.output.recv() => write_output(output)?,
            Some(event) = child.events.recv() => match event {
                ProcessEvent::Listening(_) => {}
                ProcessEvent::Exited(status) => break exit_code(status),
                ProcessEvent::Error(error) => {
                    anyhow::bail!("failed while supervising the command: {error}")
                }
            },
            result = &mut forwarder => {
                child.signal(Signal::SIGTERM);
                match result {
                    Ok(Ok(())) => anyhow::bail!("ngrok endpoint closed unexpectedly"),
                    Ok(Err(error)) => return Err(error.context("ngrok forwarding failed")),
                    Err(error) => return Err(error.into()),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                child.signal(Signal::SIGINT);
            }
        }
    };

    input.abort();
    // Remaining output was already drained into this receiver; replay it before
    // returning the wrapped command's exit status.
    while let Ok(output) = child.output.try_recv() {
        write_output(output)?;
    }
    Ok(exit)
}

async fn wait_for_listener(
    child: &mut ChildProcess,
    forwarder: &mut JoinHandle<anyhow::Result<()>>,
) -> Result<u16> {
    loop {
        tokio::select! {
            Some(output) = child.output.recv() => write_output(output)?,
            Some(event) = child.events.recv() => match event {
                ProcessEvent::Listening(port) => return Ok(port),
                ProcessEvent::Exited(status) => {
                    anyhow::bail!("command exited with {status} before opening a TCP listener");
                }
                ProcessEvent::Error(error) => {
                    child.signal(Signal::SIGTERM);
                    anyhow::bail!("could not observe the command's listeners: {error}");
                }
            },
            result = &mut *forwarder => {
                child.signal(Signal::SIGTERM);
                match result {
                    Ok(Ok(())) => anyhow::bail!("ngrok endpoint closed unexpectedly"),
                    Ok(Err(error)) => return Err(error.context("ngrok forwarding failed")),
                    Err(error) => return Err(error.into()),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                child.signal(Signal::SIGINT);
            }
        }
    }
}

fn write_output(output: ProcessOutput) -> io::Result<()> {
    match output {
        ProcessOutput::Stdout(bytes) => {
            io::stdout().write_all(&bytes)?;
            io::stdout().flush()
        }
        ProcessOutput::Stderr(bytes) => {
            io::stderr().write_all(&bytes)?;
            io::stderr().flush()
        }
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}
