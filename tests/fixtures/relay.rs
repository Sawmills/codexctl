use std::{
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// A real, isolated relay for public CLI and launch tests. No provider calls.
pub struct TestRelay {
    child: Child,
    pub address: String,
}

impl TestRelay {
    pub fn start(home: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        Self::start_at(home, &address)
    }

    pub fn start_at(home: &Path, address: &str) -> Self {
        Self::start_upstream(home, address, "http://127.0.0.1:9")
    }

    pub fn start_upstream(home: &Path, address: &str, upstream: &str) -> Self {
        let command = || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_codexctl"));
            command
                .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
                .env("HOME", home)
                .env_remove("CODEX_HOME")
                .env_remove("CODEXCTL_PINNED_ALIAS")
                .env_remove("CODEXCTL_PINNED_CONNECTION");
            command
        };
        let child = command()
            .args([
                "relay",
                "serve",
                "--listen",
                address,
                "--upstream",
                upstream,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut relay = Self {
            child,
            address: address.to_owned(),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if relay.child.try_wait().unwrap().is_some() {
                use std::io::Read;
                let mut stderr = String::new();
                relay
                    .child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("relay failed startup: {stderr}");
            }
            let status = command()
                .args(["relay", "status", "--listen", address])
                .output()
                .unwrap();
            if status.status.success() {
                return relay;
            }
            assert!(
                Instant::now() < deadline,
                "relay identity was not ready: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for TestRelay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
