//! Unix-domain-socket IPC server owned by the daemon.
//!
//! Protocol: newline-delimited JSON (see `nicewatch_common`).  First message
//! from the daemon is always `Hello`; on a client `Hello` it follows with a
//! full `Snapshot`, then `Diff` frames each poll cycle.
//!
//! Multiple simultaneous clients are supported (e.g. a second GUI instance);
//! each disconnect is cleaned up on the next broadcast.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use nicewatch_common::{ClientMsg, ServerMsg, encode_msg};

pub enum IpcEvent {
    Msg(ClientMsg),
}

#[derive(Clone)]
pub struct IpcHandle {
    clients: Arc<Mutex<HashMap<u64, Sender<ServerMsg>>>>,
    next_id: Arc<AtomicU64>,
}

impl IpcHandle {
    pub fn broadcast(&self, msg: &ServerMsg) {
        let mut dead = Vec::new();
        {
            let clients = self.clients.lock().unwrap();
            for (id, tx) in clients.iter() {
                if tx.send(msg.clone()).is_err() {
                    dead.push(*id);
                }
            }
        }
        if !dead.is_empty() {
            let mut clients = self.clients.lock().unwrap();
            for id in dead {
                clients.remove(&id);
            }
        }
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    fn register(&self, tx: Sender<ServerMsg>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().unwrap().insert(id, tx);
        id
    }

    fn unregister(&self, id: u64) {
        self.clients.lock().unwrap().remove(&id);
    }
}

/// Bind and start the accept loop.  A stale socket file from an unclean
/// shutdown is removed first.
pub fn start(path: &Path, events: Sender<IpcEvent>) -> std::io::Result<IpcHandle> {
    if path.exists() {
        log::warn!("removing stale socket {}", path.display());
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;

    let handle = IpcHandle {
        clients: Arc::new(Mutex::new(HashMap::new())),
        next_id: Arc::new(AtomicU64::new(1)),
    };
    let h = handle.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    // No read timeout: a healthy GUI client sends one `Hello`
                    // and then stays silent until the user acts.  A timeout
                    // would evict it after 500 ms and it would reconnect in a
                    // loop.  Dead clients are detected on socket close (EOF
                    // in the reader) or write failure in the writer thread.
                    spawn_client(stream, h.clone(), events.clone());
                }
                Err(e) => {
                    log::warn!("accept failed: {e}; retrying");
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    });
    Ok(handle)
}

fn spawn_client(stream: UnixStream, handle: IpcHandle, events: Sender<IpcEvent>) {
    let (tx, rx) = mpsc::channel::<ServerMsg>();
    let id = handle.register(tx.clone());

    // Protocol: the daemon's very first frame to every client is `Hello`
    // (carries the identity + the effective poll interval); the full
    // `Snapshot` follows as soon as the client says hello.
    let _ = tx.send(ServerMsg::Hello {
        app_name: nicewatch_common::APP_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        poll_interval_ms: 0,
    });

    // Writer thread: any ServerMsg we put on the channel goes to this client.
    if let Ok(writer_stream) = stream.try_clone() {
        let h = handle.clone();
        std::thread::spawn(move || {
            let mut w = BufWriter::new(writer_stream);
            while let Ok(msg) = rx.recv() {
                let bytes = encode_msg(&msg);
                if w.write_all(&bytes).and_then(|_| w.flush()).is_err() {
                    break;
                }
            }
            h.unregister(id);
        });
    }

    // Reader thread: client requests -> daemon event loop.
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break, // EOF or timeout/error
                Ok(_) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match nicewatch_common::decode_msg::<ClientMsg>(trimmed) {
                Ok(msg) => {
                    if events.send(IpcEvent::Msg(msg)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::debug!("bad client message: {e}");
                }
            }
        }
        handle.unregister(id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Regression test for the connect/disconnect loop: a healthy GUI client
    /// sends one `Hello` and then stays silent until the user acts.  The old
    /// 500 ms read timeout on the daemon side evicted it, and it reconnected
    /// every second.  An idle client must still be connected (and its
    /// messages still forwarded) well past 500 ms.
    #[test]
    fn idle_client_survives_longer_than_the_old_timeout() {
        let path = std::env::temp_dir().join(format!("nw-ipc-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (tx, rx) = mpsc::channel::<IpcEvent>();
        let handle = start(&path, tx).unwrap();

        let mut client = UnixStream::connect(&path).expect("connect to daemon socket");
        let mut reader = BufReader::new(client.try_clone().unwrap());

        // First frame from the daemon is always Hello (protocol contract).
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("daemon sends Hello first");
        assert!(line.contains("Hello"), "first frame is Hello, got: {line}");

        client
            .write_all(&encode_msg(&ClientMsg::Hello {
                client_kind: "test".into(),
            }))
            .unwrap();
        client.flush().unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());

        // Idle for longer than the 500 ms timeout that used to evict us.
        std::thread::sleep(Duration::from_millis(700));

        // Connection must still be alive: a further request gets forwarded.
        client
            .write_all(&encode_msg(&ClientMsg::RequestSnapshot))
            .unwrap();
        client.flush().unwrap();
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(IpcEvent::Msg(ClientMsg::RequestSnapshot)) => {}
            Ok(_) => panic!("expected RequestSnapshot after idle, got something else"),
            Err(e) => panic!("no message forwarded after idle: {e}"),
        }

        handle.unregister(1);
        drop(client);
        std::thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_file(&path);
    }
}