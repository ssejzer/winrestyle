//! Message protocol + named-pipe transport shared by the watchdog, shell, and
//! (later) installer.
//!
//! The watchdog hosts the pipe server (`wr_core::PIPE_NAME`) — it is the
//! session root with the most stable lifetime; the shell and installer are
//! clients. Messages are newline-delimited JSON, one message per line.
//!
//! ## ShellHeartbeat (ADR 0003)
//!
//! The shell sends [`ToWatchdog::ShellHeartbeat`] every [`HEARTBEAT_INTERVAL`];
//! the watchdog answers each with [`ToShell::HeartbeatAck`]. Silence longer
//! than [`HEARTBEAT_TIMEOUT`] on a previously-live channel means the peer is
//! *hung* (alive but wedged) — the observer kills it, which converts the hang
//! into a death, and the Phase 0 death paths (supervision, mutual relaunch,
//! crash-loop caps) take over. The heartbeat layer only ever detects; it never
//! recovers.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// How often the shell sends a heartbeat.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// Silence longer than this on a previously-live channel means "peer is hung".
/// Five missed beats: tolerant of scheduling stalls, and recovery still lands
/// within a few seconds — comparable to reaching for the emergency hotkey.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);

/// Messages sent *to* the watchdog (the guardian).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToWatchdog {
    /// The shell is alive and pumping. `pid` identifies the sender.
    ShellHeartbeat { seq: u64, pid: u32 },
    /// Restore `explorer.exe` now (same path as the emergency hotkey).
    RequestRestore,
}

/// Messages sent *to* the shell.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToShell {
    /// Answer to a [`ToWatchdog::ShellHeartbeat`], echoing its `seq`. `pid` is
    /// the watchdog's — the shell uses it (not a possibly-stale env var) when
    /// it must kill a hung watchdog.
    HeartbeatAck { seq: u64, pid: u32 },
    /// Re-read config from disk and re-apply (hot reload).
    ReloadConfig,
    /// Begin a clean shutdown.
    Shutdown,
}

/// Encode a message as one JSON line (newline included).
pub fn encode<T: Serialize>(msg: &T) -> String {
    // Serialization of these enums cannot fail.
    let mut line = serde_json::to_string(msg).expect("message serializes");
    line.push('\n');
    line
}

/// Decode one line (without the trailing newline). `None` for unknown or
/// malformed messages — a version-skewed peer must not take the channel down.
pub fn decode<T: DeserializeOwned>(line: &str) -> Option<T> {
    serde_json::from_str(line).ok()
}

#[cfg(windows)]
pub mod pipe {
    //! Blocking named-pipe transport with non-blocking reads.
    //!
    //! Reads poll `PeekNamedPipe` instead of using overlapped I/O — the same
    //! polling idiom as the watchdog's `try_wait` supervision loop, and enough
    //! for a 1 Hz heartbeat. Phase 1 serves a single client (the shell); the
    //! installer later means bumping `MAX_INSTANCES` and serving each
    //! connection on its own thread.

    use anyhow::{bail, Context, Result};
    use serde::de::DeserializeOwned;
    use serde::Serialize;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    const MAX_INSTANCES: u32 = 1;
    const BUFFER_SIZE: u32 = 4096;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// One end of a connected pipe. Closes the handle on drop.
    pub struct Connection {
        handle: HANDLE,
        buf: Vec<u8>,
    }

    // HANDLE is a raw pointer newtype; pipe handles are safe to move across
    // threads (each Connection is only ever *used* from one thread).
    unsafe impl Send for Connection {}

    impl Drop for Connection {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    impl Connection {
        /// Connect to a server pipe as a client.
        pub fn connect(name: &str) -> Result<Self> {
            let name = wide(name);
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            }
            .context("connecting to pipe")?;
            Ok(Connection {
                handle,
                buf: Vec::new(),
            })
        }

        /// Send one message. An error means the peer is gone.
        pub fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
            let line = super::encode(msg);
            let mut written = 0u32;
            unsafe { WriteFile(self.handle, Some(line.as_bytes()), Some(&mut written), None) }
                .context("pipe write")?;
            if written as usize != line.len() {
                bail!("short pipe write ({written} of {} bytes)", line.len());
            }
            Ok(())
        }

        /// Receive one message without blocking. `Ok(None)` means "nothing
        /// complete yet"; an error means the peer is gone. Unknown/malformed
        /// lines are logged and skipped.
        pub fn try_recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
            loop {
                if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line[..line.len() - 1]);
                    match super::decode(&line) {
                        Some(msg) => return Ok(Some(msg)),
                        None => {
                            log::warn!("ignoring unknown pipe message: {line}");
                            continue;
                        }
                    }
                }

                let mut available = 0u32;
                unsafe { PeekNamedPipe(self.handle, None, 0, None, Some(&mut available), None) }
                    .context("pipe peek")?;
                if available == 0 {
                    return Ok(None);
                }

                let mut chunk = vec![0u8; available as usize];
                let mut read = 0u32;
                unsafe { ReadFile(self.handle, Some(&mut chunk), Some(&mut read), None) }
                    .context("pipe read")?;
                self.buf.extend_from_slice(&chunk[..read as usize]);
            }
        }
    }

    /// The server end. Owns one pipe instance and serves one client at a time.
    pub struct Server {
        conn: Connection,
    }

    impl Server {
        /// Create the pipe. Fails if another server already owns the name.
        pub fn create(name: &str) -> Result<Self> {
            let name = wide(name);
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    MAX_INSTANCES,
                    BUFFER_SIZE,
                    BUFFER_SIZE,
                    0,
                    None,
                )
            };
            if handle.is_invalid() {
                return Err(windows::core::Error::from_win32()).context("creating pipe");
            }
            Ok(Server {
                conn: Connection {
                    handle,
                    buf: Vec::new(),
                },
            })
        }

        /// Block until a client connects.
        pub fn wait_for_client(&mut self) -> Result<()> {
            self.conn.buf.clear();
            let result = unsafe { ConnectNamedPipe(self.conn.handle, None) };
            match result {
                Ok(()) => Ok(()),
                // The client connected between create/disconnect and this call.
                Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(()),
                Err(e) => Err(e).context("waiting for pipe client"),
            }
        }

        /// Drop the current client so [`Self::wait_for_client`] can accept the
        /// next one.
        pub fn disconnect(&mut self) {
            unsafe {
                let _ = DisconnectNamedPipe(self.conn.handle);
            }
        }

        pub fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
            self.conn.send(msg)
        }

        pub fn try_recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
            self.conn.try_recv()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_round_trips() {
        let msg = ToWatchdog::ShellHeartbeat { seq: 42, pid: 1234 };
        let line = encode(&msg);
        assert!(line.ends_with('\n'));
        assert_eq!(decode::<ToWatchdog>(line.trim_end()), Some(msg));
    }

    #[test]
    fn ack_round_trips() {
        let msg = ToShell::HeartbeatAck { seq: 42, pid: 5678 };
        assert_eq!(decode::<ToShell>(encode(&msg).trim_end()), Some(msg));
    }

    #[test]
    fn unknown_or_malformed_decodes_to_none() {
        for line in [
            "",
            "garbage",
            "{\"NotAMessage\":{}}",
            "{\"ShellHeartbeat\":{}}",
        ] {
            assert_eq!(decode::<ToWatchdog>(line), None, "{line:?}");
        }
    }
}
