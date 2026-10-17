#![cfg(all(unix, feature = "central-prototype"))]

//! Measure the real relay alone. Other integration tests must not contribute
//! CPU time, and a regression must not strand the test runner in hyper's loop.
use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use codexctl::relay::{self, RelayConfig};

fn isolated(name: &str, scenario: impl FnOnce()) {
    if std::env::var("CODEXCTL_RELAY_CPU_TEST").as_deref() == Ok(name) {
        scenario();
        return;
    }
    let output = tempfile::tempfile().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("CODEXCTL_RELAY_CPU_TEST", name)
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    use std::io::{Seek, SeekFrom};
    let mut output = output;
    output.seek(SeekFrom::Start(0)).unwrap();
    let mut text = String::new();
    output.read_to_string(&mut text).unwrap();
    eprint!("{text}");
    assert!(
        status.is_some_and(|status| status.success()),
        "{name}: {status:?}\n{text}"
    );
}

fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes this valid, writable rusage on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    // SAFETY: the successful getrusage call initialized usage.
    let usage = unsafe { usage.assume_init() };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1_000_000.0;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

fn read_through(socket: &mut TcpStream, suffix: &[u8]) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(suffix) {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 32 * 1024, "missing HTTP marker");
    }
}

fn measure_half_close(silent_sse: bool) {
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_url = format!("http://{}", upstream.local_addr().unwrap());
    let (ready, arrived) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut socket, _) = upstream.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        read_through(&mut socket, b"\r\n\r\n");
        if silent_sse {
            let frame = b"data: {\"type\":\"response.created\"}\n\n";
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n", frame.len()).unwrap();
            socket.write_all(frame).unwrap();
            socket.write_all(b"\r\n").unwrap();
        } else {
            // #4085: EOF before any response, with the request body still open.
            socket.shutdown(Shutdown::Write).unwrap();
        }
        ready.send(()).unwrap();
        let mut buffer = [0; 8192];
        while socket.read(&mut buffer).is_ok_and(|count| count != 0) {}
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let identity = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    {
        let _entered = runtime.enter();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let config = RelayConfig::new(&upstream_url)
            .unwrap()
            .with_identity_root(identity.path().join("store"))
            .with_log(|_| {});
        runtime.spawn(async move {
            relay::serve(listener, config, std::future::pending())
                .await
                .unwrap();
        });
    }
    let mut downstream = TcpStream::connect(address).unwrap();
    downstream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    if silent_sse {
        downstream.write_all(b"POST /backend-api/codex/responses HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        read_through(&mut downstream, b"\n\n\r\n");
        downstream.shutdown(Shutdown::Write).unwrap();
    } else {
        downstream.write_all(b"POST /backend-api/codex/responses HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n2000\r\n").unwrap();
        // Exceed the relay's 4KiB model prefix, without ending the upload.
        downstream.write_all(&[b'x'; 8192]).unwrap();
        downstream.write_all(b"\r\n").unwrap();
    }
    arrived.recv_timeout(Duration::from_secs(5)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    let cpu_before = cpu_seconds();
    std::thread::sleep(Duration::from_secs(2));
    let cpu = cpu_seconds() - cpu_before;
    let percent = 100.0 * cpu / started.elapsed().as_secs_f64();
    eprintln!(
        "relay CPU: {percent:.3}% over {:.3}s (silent_sse={silent_sse})",
        started.elapsed().as_secs_f64()
    );
    drop(downstream);
    runtime.shutdown_background();
    assert!(percent < 5.0, "relay spun: {percent:.3}% CPU, limit 5%");
}

#[test]
fn silent_sse_after_downstream_half_close_parks() {
    isolated("silent_sse_after_downstream_half_close_parks", || {
        measure_half_close(true)
    });
}

#[test]
fn upstream_half_close_during_open_upload_parks() {
    isolated("upstream_half_close_during_open_upload_parks", || {
        measure_half_close(false)
    });
}
