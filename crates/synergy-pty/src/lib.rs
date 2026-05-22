use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem, SlavePty};
use std::io::{Read, Write};
use std::thread;
use tokio::sync::mpsc;

/// A running PTY session.
///
/// Holds the master writer, the child process handle, and the slave handle.
/// On Windows ConPTY the slave must remain alive for the duration of the
/// child process — dropping it prematurely causes "pipe is being closed"
/// errors.
pub struct PtySession {
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<String>,
    // Keep these alive for the lifetime of the session.
    _child: Box<dyn Child + Send>,
    _slave: Box<dyn SlavePty + Send>,
    _master: Box<dyn MasterPty + Send>,
}

impl PtySession {
    pub fn spawn(cmd: &str, args: &[&str], cwd: Option<&str>) -> Result<Self> {
        Self::spawn_with_env(cmd, args, cwd, Vec::new())
    }

    pub fn spawn_with_env(
        cmd: &str,
        args: &[&str],
        cwd: Option<&str>,
        envs: Vec<(String, String)>,
    ) -> Result<Self> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd_builder = CommandBuilder::new(cmd);
        cmd_builder.args(args);
        if let Some(dir) = cwd {
            cmd_builder.cwd(dir);
        }
        for (k, v) in envs {
            cmd_builder.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd_builder)?;
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, rx) = mpsc::channel(256);

        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut r = reader;
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let s = String::from_utf8_lossy(&buf[..n]).into_owned();
                        if tx.blocking_send(s).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(PtySession {
            writer,
            rx,
            _child: child,
            _slave: pair.slave,
            _master: pair.master,
        })
    }

    pub fn write(&mut self, data: &str) -> Result<()> {
        self.writer.write_all(data.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    pub async fn read_async(&mut self) -> Option<String> {
        self.rx.recv().await
    }

    /// Non-blocking read: returns immediately with `None` if no data is
    /// available yet. Suitable for polling loops.
    pub fn try_read(&mut self) -> Option<String> {
        self.rx.try_recv().ok()
    }

    /// Resize the PTY (cols/rows). Must be called when the UI terminal resizes
    /// so the child process can re-render properly.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self._master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }
}
