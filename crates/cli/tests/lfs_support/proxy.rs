//! Test-only TCP fault injection in front of the unmodified LFS server.
//! The server advertises this address itself; no JSON/action rewriting or
//! replacement LFS implementation is involved.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Default)]
pub struct State {
    pub puts: AtomicUsize,
    pub active_puts: AtomicUsize,
    pub peak_puts: AtomicUsize,
    pub peak_clients: AtomicUsize,
    active_clients: Mutex<BTreeMap<String, usize>>,
    pub cut_bytes: AtomicUsize,
    pub cut_length: AtomicUsize,
    pub cuts: AtomicUsize,
    fail_put: AtomicUsize,
    fail_after: AtomicUsize,
    pub delay_ms: AtomicUsize,
    stopped: AtomicBool,
}

pub struct Proxy {
    pub address: String,
    pub state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

impl Proxy {
    pub fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(State::default());
        let shared = Arc::clone(&state);
        let thread = thread::spawn(move || {
            let mut workers = Vec::new();
            while !shared.stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let upstream = upstream.clone();
                        let shared = Arc::clone(&shared);
                        workers.push(thread::spawn(move || {
                            let _ = forward(client, &upstream, shared);
                        }));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        Self {
            address,
            state,
            thread: Some(thread),
        }
    }

    pub fn interrupt_nth_next_put(&self, nth: usize, after: usize) {
        self.state.fail_after.store(after, Ordering::SeqCst);
        self.state.fail_put.store(
            self.state.puts.load(Ordering::SeqCst) + nth,
            Ordering::SeqCst,
        );
    }

    pub fn clear_fault(&self) {
        self.state.fail_put.store(0, Ordering::SeqCst);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn forward(mut client: TcpStream, upstream: &str, state: Arc<State>) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(10)))?;
    client.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut server = TcpStream::connect(upstream)?;
    server.set_read_timeout(Some(Duration::from_secs(10)))?;
    server.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut input = BufReader::new(client.try_clone()?);
    let mut header = String::new();
    let mut first = String::new();
    input.read_line(&mut first)?;
    if first.is_empty() {
        return Ok(());
    }
    let put = first.starts_with("PUT ");
    header.push_str(&first);
    let mut length = 0usize;
    let mut client_name = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if line == "\r\n" {
            break;
        }
        if header.len() + line.len() > 64 * 1024 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let (name, value) = line.split_once(':').ok_or(io::ErrorKind::InvalidData)?;
        if name.eq_ignore_ascii_case("content-length") {
            length = value
                .trim()
                .parse()
                .map_err(|_| io::ErrorKind::InvalidData)?;
        }
        if name.eq_ignore_ascii_case("x-cdc-test-client") {
            client_name = Some(value.trim().to_owned());
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            // The reference client's file and JSON requests have known sizes.
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unexpected chunked request",
            ));
        }
        if !name.eq_ignore_ascii_case("connection") {
            header.push_str(&line);
        }
    }
    // One request per TCP connection keeps the fault boundary deterministic.
    // This is normal HTTP/1.1 connection management, supported by both ends.
    header.push_str("Connection: close\r\n\r\n");
    server.write_all(header.as_bytes())?;
    let mut response = server.try_clone()?;
    let mut output = client.try_clone()?;
    // Start the reverse path before reading the body so Expect:100-continue
    // works exactly as it does without the proxy.
    let reverse = thread::spawn(move || io::copy(&mut response, &mut output));
    let sequence = if put {
        let n = state.puts.fetch_add(1, Ordering::SeqCst) + 1;
        let active = state.active_puts.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak_puts.fetch_max(active, Ordering::SeqCst);
        if let Some(name) = &client_name {
            let mut clients = state.active_clients.lock().unwrap();
            *clients.entry(name.clone()).or_default() += 1;
            state
                .peak_clients
                .fetch_max(clients.len(), Ordering::SeqCst);
        }
        n
    } else {
        0
    };
    let failure_start = state.fail_put.load(Ordering::SeqCst);
    let cut = put && failure_start > 0 && sequence >= failure_start;
    let stop_after = if cut {
        state
            .fail_after
            .load(Ordering::SeqCst)
            .min(length.saturating_sub(1))
    } else {
        length
    };
    let result: io::Result<()> = (|| {
        let mut sent = 0usize;
        let mut bytes = [0u8; 16 * 1024];
        while sent < stop_after {
            let wanted = bytes.len().min(stop_after - sent);
            let n = input.read(&mut bytes[..wanted])?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            server.write_all(&bytes[..n])?;
            sent += n;
            if put {
                let delay = state.delay_ms.load(Ordering::SeqCst);
                if delay > 0 {
                    thread::sleep(Duration::from_millis(delay as u64));
                }
            }
        }
        if cut {
            // These bytes reached the actual server; terminate both sockets
            // while the advertised Content-Length is still unsatisfied.
            if state.cuts.fetch_add(1, Ordering::SeqCst) == 0 {
                state.cut_bytes.store(sent, Ordering::SeqCst);
                state.cut_length.store(length, Ordering::SeqCst);
            }
            let _ = server.shutdown(Shutdown::Both);
            let _ = client.shutdown(Shutdown::Both);
        } else {
            let _ = server.shutdown(Shutdown::Write);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = server.shutdown(Shutdown::Both);
        let _ = client.shutdown(Shutdown::Both);
    }
    let reply = reverse.join().map_err(|_| io::ErrorKind::Other)?;
    if put {
        state.active_puts.fetch_sub(1, Ordering::SeqCst);
        if let Some(name) = &client_name {
            let mut clients = state.active_clients.lock().unwrap();
            let count = clients.get_mut(name).unwrap();
            *count -= 1;
            if *count == 0 {
                clients.remove(name);
            }
        }
    }
    result?;
    if !cut {
        reply?;
    }
    // Do not retain a connection after forwarding a complete response.
    let _ = client.flush();
    Ok(())
}
