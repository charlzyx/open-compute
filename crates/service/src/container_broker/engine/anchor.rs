//! Continuity of the qualified engine connection, with bounded keepalive.

use super::{PlatformError, invalid};
use http_body_util::{BodyExt as _, Empty};
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify, watch};
use tracing::instrument::WithSubscriber as _;

const PHASES: [&str; 8] = [
    "validation",
    "identity",
    "mutex",
    "ready",
    "headers",
    "status",
    "body",
    "content",
];

type Sender = hyper::client::conn::http1::SendRequest<Empty<bytes::Bytes>>;

pub(super) struct Anchor {
    path: PathBuf,
    identity: (u64, u64),
    health_path: &'static str,
    sender: Mutex<Sender>,
    dead: Arc<AtomicBool>,
    phase: AtomicUsize,
    requested: Arc<Notify>,
    completed: watch::Sender<()>,
    connection: tokio::task::JoinHandle<()>,
    heartbeat: OnceLock<tokio::task::JoinHandle<()>>,
}

impl Anchor {
    pub(super) async fn connect(
        path: &Path,
        health_path: &'static str,
    ) -> Result<Arc<Self>, PlatformError> {
        let metadata = std::fs::symlink_metadata(path).map_err(|_| invalid())?;
        let identity = (metadata.dev(), metadata.ino());
        let stream = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::UnixStream::connect(path),
        )
        .await
        .map_err(|_| invalid())?
        .map_err(|_| invalid())?;
        let peer = stream.peer_cred().map_err(|_| invalid())?;
        if ![0, rustix::process::getuid().as_raw()].contains(&peer.uid()) {
            return Err(invalid());
        }
        let (sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .map_err(|_| invalid())?;
        let dead = Arc::new(AtomicBool::new(false));
        let completed = dead.clone();
        let connection = tokio::spawn(async move {
            let result = connection.await;
            if !completed.swap(true, Ordering::AcqRel) {
                tracing::warn!(
                    reason = "connection_completed",
                    phase = "driver",
                    timeout = false,
                    driver_failed = result.is_err(),
                    "Container provider continuity lost"
                );
            }
        });
        let anchor = Arc::new(Self {
            path: path.to_owned(),
            identity,
            health_path,
            sender: Mutex::new(sender),
            dead,
            phase: AtomicUsize::new(0),
            requested: Arc::new(Notify::new()),
            completed: watch::channel(()).0,
            connection,
            heartbeat: OnceLock::new(),
        });
        anchor.probe_bounded().await?;
        let weak = Arc::downgrade(&anchor);
        let requested = anchor.requested.clone();
        let task = tokio::spawn(
            async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(10)) => {},
                        _ = requested.notified() => {},
                    }
                    let Some(anchor) = weak.upgrade() else {
                        break;
                    };
                    let result = anchor.probe_bounded().await;
                    anchor.completed.send_replace(());
                    if result.is_err() {
                        break;
                    }
                }
            }
            .with_current_subscriber(),
        );
        let _ = anchor.heartbeat.set(task);
        Ok(anchor)
    }

    pub(super) async fn check(&self) -> Result<(), PlatformError> {
        let mut completed = self.completed.subscribe();
        if self.dead.load(Ordering::Acquire) {
            return Err(invalid());
        }
        // The existing keepalive task owns the probe, so caller cancellation cannot drop its body.
        self.requested.notify_one();
        match tokio::time::timeout(Duration::from_secs(5), completed.changed()).await {
            Ok(Ok(())) if !self.dead.load(Ordering::Acquire) => Ok(()),
            Err(_) => {
                self.fail(PHASES[self.phase.load(Ordering::Acquire)], true);
                Err(invalid())
            }
            _ => Err(invalid()),
        }
    }

    fn fail(&self, phase: &'static str, timeout: bool) {
        if !self.dead.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                reason = "probe_failed",
                phase,
                timeout,
                driver_failed = false,
                "Container provider continuity lost"
            );
        }
    }

    fn set_phase(&self, phase: &mut &'static str, index: usize) {
        *phase = PHASES[index];
        self.phase.store(index, Ordering::Release);
    }

    async fn probe_bounded(&self) -> Result<(), PlatformError> {
        let mut phase = "validation";
        self.set_phase(&mut phase, 0);
        let result = tokio::time::timeout(Duration::from_secs(5), self.probe(&mut phase)).await;
        match result {
            Ok(Ok(())) => Ok(()),
            _ => {
                self.fail(phase, result.is_err());
                Err(invalid())
            }
        }
    }

    async fn probe(&self, phase: &mut &'static str) -> Result<(), PlatformError> {
        super::super::socket::validate_engine_socket(&self.path)?;
        self.set_phase(phase, 1);
        let metadata = std::fs::symlink_metadata(&self.path).map_err(|_| invalid())?;
        if self.dead.load(Ordering::Acquire) || (metadata.dev(), metadata.ino()) != self.identity {
            return Err(invalid());
        }
        let request = hyper::Request::builder()
            .method("GET")
            .uri(self.health_path)
            .header("Host", "localhost")
            .body(Empty::new())
            .map_err(|_| invalid())?;
        self.set_phase(phase, 2);
        let mut sender = self.sender.lock().await;
        self.set_phase(phase, 3);
        sender.ready().await.map_err(|_| invalid())?;
        self.set_phase(phase, 4);
        let response = sender.send_request(request).await.map_err(|_| invalid())?;
        self.set_phase(phase, 5);
        if response.status() != 200 {
            return Err(invalid());
        }
        let mut body = response.into_body();
        let mut total = 0_usize;
        let mut content = Vec::new();
        self.set_phase(phase, 6);
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.map_err(|_| invalid())?.into_data() {
                total = total.checked_add(data.len()).ok_or_else(invalid)?;
                if total > 1 << 20 {
                    return Err(invalid());
                }
                content.extend_from_slice(&data);
            }
        }
        self.set_phase(phase, 7);
        if self.health_path == "/_ping" && content != b"OK" {
            return Err(invalid());
        }
        Ok(())
    }
}

impl Drop for Anchor {
    fn drop(&mut self) {
        self.connection.abort();
        if let Some(task) = self.heartbeat.get() {
            task.abort();
        }
    }
}
