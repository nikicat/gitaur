//! HTTP tarpit used by `tests/container/extended/*` to exercise gitaur's
//! idle-fetch timeout (`mirror_idle_timeout_secs` → curl `lowSpeedTime`).
//!
//! Accepts TCP, drains the HTTP request line + headers, then sleeps for
//! a day. From curl's perspective: the connect + request-write succeeds,
//! and the wait-for-response phase silently stalls — exactly the failure
//! mode the timeout has to catch.
//!
//! Usage: `tarpit <port>` (binds 127.0.0.1).

use std::env;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

fn main() {
    let port: u16 = env::args()
        .nth(1)
        .expect("usage: tarpit <port>")
        .parse()
        .expect("port must be u16");
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    eprintln!("tarpit listening on 127.0.0.1:{port}");
    for stream in listener.incoming().flatten() {
        // Standalone test tarpit — no gitaur thread-locals to propagate.
        #[allow(clippy::disallowed_methods)]
        thread::spawn(move || {
            // Drain headers so the server reaches the "respond" phase before
            // stalling — that's where curl's lowSpeedTime applies. Without
            // this, curl might trip on a different (request-write) error.
            let mut reader = BufReader::new(&stream);
            let mut buf = String::new();
            while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                if buf == "\r\n" || buf == "\n" {
                    break;
                }
                buf.clear();
            }
            // Hold the connection open and silent. Tests kill the parent
            // process to release these threads.
            thread::sleep(Duration::from_hours(24));
            // Touch `stream` here so the borrow lives until sleep returns,
            // ensuring we don't drop (and close) the socket early.
            drop(stream);
        });
    }
}
