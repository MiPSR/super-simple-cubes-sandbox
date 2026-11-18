//! Byte transports for the protocol.
//!
//! Two implementations (`specs.md` §2, §4):
//! - in-memory channel pair for **local** play (server embedded in the
//!   client process, exercising the exact same wire format);
//! - TCP for the **dedicated** server (UDP/RakNet-style comes later).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

/// A framed byte channel. Both halves of a connection are usable from a
/// single owning thread: `send` writes, `try_recv` drains non-blockingly.
pub trait Wire: Send {
    /// Send one frame. `Err` means the connection is dead.
    fn send(&mut self, bytes: &[u8]) -> Result<(), String>;

    /// Poll for the next received frame, if any is buffered.
    fn try_recv(&mut self) -> Option<Vec<u8>>;

    /// Whether the remote side is still connected (best-effort).
    fn is_alive(&self) -> bool;
}

// ---------------------------------------------------------------------------
// In-memory pair (local play)
// ---------------------------------------------------------------------------

/// One end of an in-memory connection.
pub struct MemoryWire {
    out: Sender<Vec<u8>>,
    inbox: Receiver<Vec<u8>>,
    /// Set when the remote end dropped its channel (checked via send).
    dead: std::cell::Cell<bool>,
}

impl Wire for MemoryWire {
    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        match self.out.send(bytes.to_vec()) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.dead.set(true);
                Err("local wire: remote end dropped".to_string())
            }
        }
    }

    fn try_recv(&mut self) -> Option<Vec<u8>> {
        match self.inbox.try_recv() {
            Ok(bytes) => Some(bytes),
            Err(_) => None,
        }
    }

    fn is_alive(&self) -> bool {
        !self.dead.get()
    }
}

/// Create a connected pair: `(client_end, server_end)`.
pub fn memory_pair() -> (MemoryWire, MemoryWire) {
    let (a_to_b, b_inbox) = mpsc::channel::<Vec<u8>>();
    let (b_to_a, a_inbox) = mpsc::channel::<Vec<u8>>();
    (
        MemoryWire { out: a_to_b, inbox: a_inbox, dead: std::cell::Cell::new(false) },
        MemoryWire { out: b_to_a, inbox: b_inbox, dead: std::cell::Cell::new(false) },
    )
}

// ---------------------------------------------------------------------------
// TCP (dedicated server)
// ---------------------------------------------------------------------------

const MAX_FRAME: u32 = 1 << 22; // 4 MiB safety cap

/// TCP wire with a background reader thread that parses frames.
pub struct TcpWire {
    out: TcpStream,
    inbox: Receiver<Vec<u8>>,
    reader_alive: Arc<AtomicBool>,
}

impl TcpWire {
    /// Wrap an established TCP stream.
    pub fn new(stream: TcpStream) -> Result<Self, String> {
        stream
            .set_nodelay(true)
            .map_err(|e| format!("tcp nodelay: {e}"))?;
        let reader_alive = Arc::new(AtomicBool::new(true));
        let mut reader = stream
            .try_clone()
            .map_err(|e| format!("tcp clone: {e}"))?;
        let (tx, inbox) = mpsc::channel::<Vec<u8>>();
        let alive_flag = reader_alive.clone();
        std::thread::Builder::new()
            .name("tcp-reader".into())
            .spawn(move || {
                loop {
                    match read_frame(&mut reader) {
                        Ok(Some(frame)) => {
                            if tx.send(frame).is_err() {
                                break;
                            }
                        }
                        Ok(None) | Err(_) => break, // EOF or error
                    }
                }
                alive_flag.store(false, Ordering::Release);
            })
            .map_err(|e| format!("spawn reader: {e}"))?;
        Ok(TcpWire { out: stream, inbox, reader_alive })
    }

    /// Connect to a dedicated server (`host:port`).
    pub fn connect(addr: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
        Self::new(stream)
    }
}

impl Wire for TcpWire {
    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        let len = u32::try_from(bytes.len()).map_err(|_| "frame too large".to_string())?;
        let mut buf = len.to_le_bytes().to_vec();
        buf.extend_from_slice(bytes);
        self.out
            .write_all(&buf)
            .and_then(|_| self.out.flush())
            .map_err(|e| format!("tcp send: {e}"))
    }

    fn try_recv(&mut self) -> Option<Vec<u8>> {
        match self.inbox.try_recv() {
            Ok(bytes) => Some(bytes),
            Err(_) => None,
        }
    }

    fn is_alive(&self) -> bool {
        self.reader_alive.load(Ordering::Acquire)
    }
}

/// Read one length-prefixed frame; `Ok(None)` on clean EOF.
fn read_frame(stream: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut frame = vec![0u8; len as usize];
    match stream.read_exact(&mut frame) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    Ok(Some(frame))
}
