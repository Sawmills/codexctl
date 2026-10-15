//! Point-in-time listener proof; this does not authenticate later connections.
use std::{
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io::{Read, Write},
    net::SocketAddr,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
    time::Duration,
};

use aes_gcm::aead::{OsRng, rand_core::RngCore};
use anyhow::{Context, Result, bail};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const DIRECTORY_FLAGS: i32 = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const FILE_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
const MAX_PROOF_BODY: u64 = 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    version: u8,
    nonce: String,
    proof: String,
}

fn private(file: &File, directory: bool) -> Result<()> {
    let metadata = file.metadata()?;
    let mode = if directory { 0o700 } else { 0o600 };
    if (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
        || metadata.mode() & 0o7777 != mode
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        bail!(
            "relay identity requires same-user private directories (0700) and regular keys (0600)"
        );
    }
    Ok(())
}

fn open_at(parent: &File, name: &CStr, flags: i32) -> Result<File> {
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("could not open relay identity");
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn directory(root: &Path, create: bool) -> Result<File> {
    if create {
        match std::fs::DirBuilder::new().mode(0o700).create(root) {
            Ok(()) => {
                File::open(root.parent().context("relay store has no parent")?)?.sync_all()?
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(DIRECTORY_FLAGS)
        .open(root)?;
    private(&directory, true)?;
    for name in [c"relay", c"identity"] {
        if create {
            let result = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if result == 0 {
                directory.sync_all()?;
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
        }
        directory = open_at(&directory, name, libc::O_RDONLY | DIRECTORY_FLAGS)?;
        if name == c"relay" {
            // Older relays created this shared evidence parent as 0755.
            // Only identity/ and keys need to be unreadable by other users.
            let metadata = directory.metadata()?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
                bail!("relay parent must be same-user and not writable by group or others");
            }
        } else {
            private(&directory, true)?;
        }
    }
    Ok(directory)
}

pub(super) struct Identity {
    key: [u8; 32],
    address: SocketAddr,
}

impl Identity {
    /// Only the bound service creates identity state; existing state is never repaired.
    pub(super) fn open(root: &Path, address: SocketAddr) -> Result<Self> {
        let name = key_name(address)?;
        let directory = directory(root, true)?;
        let mut key = [0; 32];
        OsRng
            .try_fill_bytes(&mut key)
            .map_err(|_| anyhow::anyhow!("could not create relay identity entropy"))?;
        let mut suffix = [0; 8];
        OsRng.fill_bytes(&mut suffix);
        let temp_name = CString::new(format!(
            ".{}.{}.tmp",
            name.to_string_lossy(),
            u64::from_ne_bytes(suffix)
        ))?;
        let key = match open_at(
            &directory,
            &temp_name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | FILE_FLAGS,
        ) {
            Ok(mut file) => {
                let result = (|| -> Result<[u8; 32]> {
                    private(&file, false)?;
                    file.write_all(&key)?;
                    file.sync_all()?;
                    let linked = unsafe {
                        libc::linkat(
                            directory.as_raw_fd(),
                            temp_name.as_ptr(),
                            directory.as_raw_fd(),
                            name.as_ptr(),
                            0,
                        )
                    };
                    if linked == 0 {
                        directory.sync_all()?;
                        Ok(key)
                    } else {
                        let error = std::io::Error::last_os_error();
                        if error.kind() == std::io::ErrorKind::AlreadyExists {
                            read_key(&directory, &name)
                        } else {
                            Err(error.into())
                        }
                    }
                })();
                let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp_name.as_ptr(), 0) };
                result?
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                read_key(&directory, &name)?
            }
            Err(error) => return Err(error),
        };
        Ok(Self { key, address })
    }

    pub(super) fn health(&self, query: Option<&str>) -> Response {
        let Some(query) = query else {
            return "ok".into_response();
        };
        let Some(challenge) = query
            .strip_prefix("nonce=")
            .filter(|value| value.len() == 43)
        else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let Ok(nonce) = URL_SAFE_NO_PAD.decode(challenge) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if nonce.len() != 32 {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Json(Proof {
            version: 1,
            nonce: challenge.to_owned(),
            proof: URL_SAFE_NO_PAD
                .encode(mac(&self.key, self.address, &nonce).finalize().into_bytes()),
        })
        .into_response()
    }
}

fn key_name(address: SocketAddr) -> Result<CString> {
    if !address.ip().is_loopback() || address.port() == 0 {
        bail!("relay identity requires a loopback address and a nonzero port");
    }
    Ok(CString::new(format!(
        "v1-{}.key",
        address.to_string().replace(':', "_")
    ))?)
}

fn read_key(directory: &File, name: &CStr) -> Result<[u8; 32]> {
    let file = open_at(directory, name, libc::O_RDONLY | FILE_FLAGS)?;
    private(&file, false)?;
    let mut bytes = Vec::new();
    file.take(33).read_to_end(&mut bytes)?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("relay identity key must contain exactly 32 bytes"))
}

fn mac(key: &[u8; 32], address: SocketAddr, nonce: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a 256-bit key");
    mac.update(b"codexctl-relay-health-v1\0");
    mac.update(address.to_string().as_bytes());
    mac.update(b"\0");
    mac.update(nonce);
    mac
}

/// Clients only read an existing key. No route or credential is changed here.
pub(crate) fn require_listener(root: &Path, address: SocketAddr) -> Result<()> {
    let verify = || -> Result<()> {
        let name = key_name(address)?;
        let key = read_key(&directory(root, false)?, &name)?;
        let mut nonce = [0; 32];
        OsRng.fill_bytes(&mut nonce);
        let challenge = URL_SAFE_NO_PAD.encode(nonce);
        let response = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()?
            .get(format!("http://{address}/healthz?nonce={challenge}"))
            .send()?;
        if response.status() != reqwest::StatusCode::OK {
            bail!("listener refused the relay identity challenge");
        }
        let mut body = Vec::new();
        response.take(MAX_PROOF_BODY + 1).read_to_end(&mut body)?;
        if body.len() as u64 > MAX_PROOF_BODY {
            bail!("relay identity response exceeds its bound");
        }
        let proof: Proof = serde_json::from_slice(&body)?;
        if proof.version != 1 || proof.nonce != challenge {
            bail!("relay identity challenge does not match");
        }
        mac(&key, address, &nonce)
            .verify_slice(&URL_SAFE_NO_PAD.decode(proof.proof)?)
            .map_err(|_| anyhow::anyhow!("relay identity proof is invalid"))
    };
    verify().with_context(|| format!("relay identity proof failed at {address}; start an upgraded relay before routing or launch"))
}
