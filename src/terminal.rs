use std::{io::IsTerminal, sync::Arc};

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};

use crate::{process::ChildProcessControl, state::AppState};

pub struct RawMode(bool);

impl RawMode {
    pub fn enable_if_terminal() -> std::io::Result<Self> {
        let terminal = std::io::stdin().is_terminal();
        if terminal {
            enable_raw_mode()?;
        }
        Ok(Self(terminal))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if self.0 {
            let _ = disable_raw_mode();
        }
    }
}

pub async fn input_loop(child: Arc<ChildProcessControl>, state: AppState) -> std::io::Result<()> {
    let terminal = std::io::stdin().is_terminal();
    let mut input = tokio::io::stdin();
    let mut byte = [0_u8; 1];
    let mut escaped = false;
    let mut resize = signal(SignalKind::window_change())?;
    loop {
        let count = tokio::select! {
            count = input.read(&mut byte) => count?,
            _ = resize.recv(), if terminal => {
                child.resize();
                continue;
            }
        };
        if count == 0 {
            child.close_stdin();
            return Ok(());
        }
        if !terminal {
            child.write(&byte);
            continue;
        }
        if escaped {
            escaped = false;
            match byte[0] {
                b'n' | 0x0e => child.write(&[0x0e]),
                b'?' => {
                    terminal_message(
                        "\r\n[nserve] ctrl+n commands: n = send ctrl+n, ? = help, s = status\r\n",
                    )
                    .await?
                }
                b's' => terminal_message(&state.render()).await?,
                other => child.write(&[0x0e, other]),
            }
            continue;
        }
        match byte[0] {
            0x0e => escaped = true,
            _ => child.write(&byte),
        }
    }
}

async fn terminal_message(message: &str) -> std::io::Result<()> {
    let mut stderr = tokio::io::stderr();
    stderr.write_all(message.as_bytes()).await?;
    stderr.flush().await
}
