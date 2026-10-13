//! Capture listeners standing in for an external server.

use std::io::{ErrorKind, Read};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often a listener thread looks at its stop flag.
const POLL: Duration = Duration::from_millis(50);

/// Bytes one connection or one datagram delivered.
#[derive(Clone, Debug)]
pub(super) struct Hit {
    /// `tcp` or `udp`, with the sender's address.
    pub(super) listener: String,
    pub(super) bytes: Vec<u8>,
}

/// A TCP and a UDP listener on one port of `host`, recording everything they receive.
pub(super) struct Sink {
    pub(super) port: u16,
    hits: Arc<Mutex<Vec<Hit>>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Sink {
    /// Listens on a fresh port of `host`, so no earlier run's bytes reach this one.
    pub(super) fn start(host: IpAddr) -> std::io::Result<Self> {
        let tcp = TcpListener::bind(SocketAddr::new(host, 0))?;
        let port = tcp.local_addr()?.port();
        let udp = UdpSocket::bind(SocketAddr::new(host, port))?;
        tcp.set_nonblocking(true)?;
        udp.set_read_timeout(Some(POLL))?;
        let hits = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let threads = vec![
            {
                let (hits, stop) = (Arc::clone(&hits), Arc::clone(&stop));
                std::thread::spawn(move || accept(&tcp, &hits, &stop))
            },
            {
                let (hits, stop) = (Arc::clone(&hits), Arc::clone(&stop));
                std::thread::spawn(move || receive(&udp, &hits, &stop))
            },
        ];
        Ok(Self {
            port,
            hits,
            stop,
            threads,
        })
    }

    /// Waits `grace` for late bytes, stops listening, and returns everything received.
    pub(super) fn drain(mut self, grace: Duration) -> Vec<Hit> {
        std::thread::sleep(grace);
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        std::mem::take(&mut *self.hits.lock().unwrap())
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn accept(listener: &TcpListener, hits: &Arc<Mutex<Vec<Hit>>>, stop: &Arc<AtomicBool>) {
    let mut readers = Vec::new();
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, from)) => {
                let (hits, stop) = (Arc::clone(hits), Arc::clone(stop));
                readers.push(std::thread::spawn(move || read(stream, from, &hits, &stop)));
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => std::thread::sleep(POLL),
            Err(_) => break,
        }
    }
    for reader in readers {
        let _ = reader.join();
    }
}

/// Reads one connection until it closes or the sink stops, recording it as one hit.
fn read(mut stream: TcpStream, from: SocketAddr, hits: &Mutex<Vec<Hit>>, stop: &AtomicBool) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(POLL));
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    while !stop.load(Ordering::Acquire) {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => bytes.extend_from_slice(&buffer[..read]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    hits.lock().unwrap().push(Hit {
        listener: format!("tcp from {from}"),
        bytes,
    });
}

fn receive(socket: &UdpSocket, hits: &Mutex<Vec<Hit>>, stop: &AtomicBool) {
    let mut buffer = vec![0; 65536];
    while !stop.load(Ordering::Acquire) {
        match socket.recv_from(&mut buffer) {
            Ok((read, from)) => hits.lock().unwrap().push(Hit {
                listener: format!("udp from {from}"),
                bytes: buffer[..read].to_vec(),
            }),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
}
