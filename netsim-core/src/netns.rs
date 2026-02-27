//! Network namespace lifecycle helpers.
//!
//! Each namespace gets a lazy async worker thread (with a `current_thread`
//! tokio runtime) and a lazy sync worker thread. Namespace FDs are stored
//! per-worker, not in a global registry.

use std::{
    collections::HashMap,
    fs::File,
    os::unix::fs::MetadataExt,
    sync::{mpsc, Arc, Mutex, OnceLock},
    thread,
};

use anyhow::{anyhow, Context, Result};
use nix::{
    sched::{setns, unshare, CloneFlags},
    unistd::gettid,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, error};

use crate::netlink::Netlink;

// ─────────────────────────────────────────────
// Namespace creation helpers (process-global)
// ─────────────────────────────────────────────

fn open_current_thread_netns_fd() -> Result<File> {
    if let Ok(fd) = File::open("/proc/thread-self/ns/net") {
        return Ok(fd);
    }
    let tid = gettid();
    let task_path = format!("/proc/self/task/{}/ns/net", tid.as_raw());
    if let Ok(fd) = File::open(&task_path) {
        return Ok(fd);
    }
    File::open("/proc/self/ns/net")
        .with_context(|| format!("open current thread netns fd (fallback path {})", task_path))
}

fn create_unshared_netns_fd() -> Result<File> {
    let (res_tx, res_rx) = mpsc::channel();
    let _ = thread::spawn(move || {
        let res: Result<()> = (|| {
            unshare(CloneFlags::CLONE_NEWNET).context("unshare CLONE_NEWNET")?;
            let fd =
                open_current_thread_netns_fd().context("open current thread netns fd in new ns")?;
            let fd_for_parent = fd
                .try_clone()
                .context("clone namespace fd for parent registry")?;
            let _ = res_tx.send(Ok(fd_for_parent));
            Ok(())
        })();
        if let Err(err) = res {
            let _ = res_tx.send(Err(err));
        }
    });
    res_rx
        .recv()
        .context("receive netns fd from helper thread")?
}

// ─────────────────────────────────────────────
// SyncWorker — dedicated thread, std::sync::mpsc
// ─────────────────────────────────────────────

enum SyncMsg {
    Task(Box<dyn FnOnce() + Send>),
    Shutdown,
}

struct SyncWorker {
    tx: mpsc::SyncSender<SyncMsg>,
    join: Option<thread::JoinHandle<()>>,
}

impl SyncWorker {
    fn spawn(fd: &File, parent_span: &tracing::Span, ns: &str) -> Result<Self> {
        let target = fd
            .try_clone()
            .with_context(|| format!("clone fd for sync worker '{ns}'"))?;
        let (tx, rx) = mpsc::sync_channel(64);
        let span = debug_span!(parent: parent_span, "sync", ns = %ns);
        let join = thread::spawn(move || sync_worker_main(target, rx, span));
        Ok(Self {
            tx,
            join: Some(join),
        })
    }
}

impl Drop for SyncWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(SyncMsg::Shutdown);
        if let Some(j) = self.join.take() {
            if j.thread().id() == thread::current().id() {
                // Being dropped on the sync worker thread — skip join to avoid
                // EDEADLK.
            } else {
                let _ = j.join();
            }
        }
    }
}

fn sync_worker_main(target: File, rx: mpsc::Receiver<SyncMsg>, span: tracing::Span) {
    let _guard = span.entered();
    if let Err(err) = setns(&target, CloneFlags::CLONE_NEWNET) {
        debug!(error = %err, "sync netns worker: setns failed");
        return;
    }
    while let Ok(msg) = rx.recv() {
        match msg {
            SyncMsg::Task(f) => f(),
            SyncMsg::Shutdown => break,
        }
    }
}

// ─────────────────────────────────────────────
// Worker — holds lazy async RT handle + sync worker + ns fd
// ─────────────────────────────────────────────

struct Worker {
    ns: String,
    parent_span: tracing::Span,
    /// The namespace file descriptor.
    ns_fd: Arc<File>,
    /// Cloned tokio `Handle` from the per-ns `current_thread` runtime.
    rt_handle: OnceLock<tokio::runtime::Handle>,
    /// Persistent rtnetlink connection for this namespace.
    netlink: OnceLock<Netlink>,
    /// Signals the async worker thread to exit.
    cancel_token: CancellationToken,
    /// Join handle for the async worker OS thread.
    async_join: Mutex<Option<thread::JoinHandle<()>>>,
    /// Lazy sync worker.
    sync_worker: Mutex<Option<SyncWorker>>,
}

impl Worker {
    fn new(ns: &str, ns_fd: Arc<File>, parent_span: tracing::Span) -> Self {
        Self {
            ns: ns.to_string(),
            parent_span,
            ns_fd,
            rt_handle: OnceLock::new(),
            netlink: OnceLock::new(),
            cancel_token: CancellationToken::new(),
            async_join: Mutex::new(None),
            sync_worker: Mutex::new(None),
        }
    }

    /// Returns the tokio runtime Handle for this namespace's async worker.
    /// Lazily spawns the worker thread on first call.
    fn rt_handle(&self) -> Result<tokio::runtime::Handle> {
        if let Some(h) = self.rt_handle.get() {
            return Ok(h.clone());
        }
        // Spawn the async worker thread.
        let target = self
            .ns_fd
            .try_clone()
            .with_context(|| format!("clone fd for async worker '{}'", self.ns))?;
        let span = debug_span!(parent: &self.parent_span, "async", ns = %self.ns);
        let cancel = self.cancel_token.clone();
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();

        let join = thread::spawn(move || {
            let _guard = span.entered();
            if let Err(err) = setns(&target, CloneFlags::CLONE_NEWNET) {
                error!(error = %err, "async netns worker: setns failed");
                let _ = handle_tx.send(Err(anyhow!("setns failed: {err}")));
                return;
            }

            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    error!(error = %err, "async netns worker: runtime build failed");
                    let _ = handle_tx.send(Err(anyhow!("runtime build failed: {err}")));
                    return;
                }
            };

            let _ = handle_tx.send(Ok(rt.handle().clone()));

            // Keep the runtime alive until cancellation.
            rt.block_on(cancel.cancelled());
            debug!("async worker shutting down");
        });

        let handle = handle_rx
            .recv()
            .context("receive rt handle from async worker thread")??;

        // Store; if another thread raced us, that's fine — OnceLock handles it.
        let _ = self.rt_handle.set(handle.clone());
        *self.async_join.lock().expect("async_join poisoned") = Some(join);

        Ok(handle)
    }

    /// Returns a clone of the namespace's persistent Netlink handle.
    /// Lazily creates the rtnetlink connection on first call.
    fn netlink(&self) -> Result<Netlink> {
        if let Some(nl) = self.netlink.get() {
            return Ok(nl.clone());
        }

        let rt = self.rt_handle()?;
        // Spawn the rtnetlink connection on the worker's runtime via channel
        // (cannot use block_on since caller may already be inside a runtime).
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            let result = async {
                let (conn, handle, _) = rtnetlink::new_connection()
                    .context("rtnetlink connection for namespace worker")?;
                tokio::spawn(conn);
                Ok::<Netlink, anyhow::Error>(Netlink::new(handle))
            }
            .await;
            let _ = tx.send(result);
        });
        let nl = rx
            .recv()
            .context("receive netlink handle from async worker")??;

        let _ = self.netlink.set(nl.clone());
        Ok(nl)
    }

    fn sync_tx(&self) -> Result<mpsc::SyncSender<SyncMsg>> {
        let mut guard = self.sync_worker.lock().expect("sync worker mutex poisoned");
        if guard.is_none() {
            *guard = Some(SyncWorker::spawn(&self.ns_fd, &self.parent_span, &self.ns)?);
        }
        Ok(guard.as_ref().unwrap().tx.clone())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Signal async worker to exit.
        self.cancel_token.cancel();
        if let Some(j) = self.async_join.lock().expect("async_join poisoned").take() {
            // If we're being dropped on the async worker thread itself (e.g. the
            // last Arc<NetworkCore> is released inside a spawned task after
            // cancellation), joining would deadlock (EDEADLK).  Detect this and
            // just let the thread exit naturally.
            if j.thread().id() == thread::current().id() {
                // Already on this thread — nothing to join.
            } else {
                let _ = j.join();
            }
        }
        // SyncWorker drops via its own Drop impl (sends Shutdown, joins).
    }
}

// ─────────────────────────────────────────────
// NetnsManager
// ─────────────────────────────────────────────

/// Manages per-namespace worker threads and file descriptors.
///
/// Each namespace gets a lazy async worker (with a `current_thread` tokio RT
/// whose `Handle` is cloned out for spawning) and a lazy sync worker for
/// short-lived blocking operations. Workers are started on first use.
pub struct NetnsManager {
    parent_span: tracing::Span,
    workers: Mutex<HashMap<String, Worker>>,
}

impl Default for NetnsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl NetnsManager {
    pub fn new() -> Self {
        Self {
            parent_span: tracing::Span::none(),
            workers: Mutex::new(HashMap::new()),
        }
    }

    pub fn new_with_span(parent_span: tracing::Span) -> Self {
        Self {
            parent_span,
            workers: Mutex::new(HashMap::new()),
        }
    }

    // ── Namespace lifecycle ──────────────────────────────────────────

    /// Create a new isolated network namespace and register it.
    pub fn create_netns(&self, name: &str) -> Result<()> {
        debug!(ns = %name, "netns: create namespace");

        // Remove any previous worker/fd for this name.
        self.remove_worker(name);

        let fd = create_unshared_netns_fd().context("create unshared netns fd")?;
        let created_ino = fd
            .metadata()
            .context("metadata for created netns fd")?
            .ino();
        let current_ino = open_current_thread_netns_fd()
            .context("open current thread netns for sanity check")?
            .metadata()
            .context("metadata for current thread netns")?
            .ino();
        if created_ino == current_ino {
            anyhow::bail!(
                "fd backend namespace creation returned current namespace inode {}; not isolated",
                created_ino
            );
        }

        let fd = Arc::new(fd);
        let worker = Worker::new(name, fd, self.parent_span.clone());
        self.workers
            .lock()
            .expect("netns worker map poisoned")
            .insert(name.to_string(), worker);

        Ok(())
    }

    /// Remove workers/fds for all namespaces matching `prefix`.
    pub fn cleanup_prefix(&self, prefix: &str) {
        let mut workers = self.workers.lock().expect("netns worker map poisoned");
        workers.retain(|k, _| !k.starts_with(prefix));
    }

    fn remove_worker(&self, name: &str) {
        let mut workers = self.workers.lock().expect("netns worker map poisoned");
        workers.remove(name); // Worker::drop cancels token + joins threads
    }

    // ── Async spawn ─────────────────────────────────────────────────

    /// Returns a cloned tokio `Handle` for the namespace's async worker.
    /// Lazily creates the worker thread on first call.
    pub fn rt_handle_for(&self, ns: &str) -> Result<tokio::runtime::Handle> {
        let workers = self.workers.lock().expect("netns worker map poisoned");
        let worker = workers
            .get(ns)
            .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
        worker.rt_handle()
    }

    /// Returns a clone of the namespace's persistent Netlink handle.
    pub(crate) fn netlink_for(&self, ns: &str) -> Result<Netlink> {
        let workers = self.workers.lock().expect("netns worker map poisoned");
        let worker = workers
            .get(ns)
            .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
        worker.netlink()
    }

    // ── Sync execution ──────────────────────────────────────────────

    /// Run a short-lived sync closure inside `ns`. Blocks the caller.
    ///
    /// Only for fast, non-blocking work (sysctl writes, `Command::spawn`).
    /// Never pass TCP/UDP I/O here — use `rt_handle_for` + `handle.spawn` instead.
    pub fn run_closure_in<F, R>(&self, ns: &str, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let tx = {
            let workers = self.workers.lock().expect("netns worker map poisoned");
            let worker = workers
                .get(ns)
                .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
            worker.sync_tx()?
        };
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        tx.send(SyncMsg::Task(Box::new(move || {
            let _ = result_tx.send(f());
        })))
        .map_err(|_| anyhow!("send task to sync netns worker for '{ns}' failed"))?;
        result_rx
            .recv()
            .context("receive closure result from sync netns worker")?
    }

    /// Spawn a dedicated OS thread inside `ns` that runs `f`. Non-blocking.
    ///
    /// The thread enters the namespace via `setns` and then runs the closure.
    /// Returns a `JoinHandle` to collect the result.
    pub fn spawn_thread_in<F, R>(&self, ns: &str, f: F) -> Result<thread::JoinHandle<Result<R>>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let fd = {
            let workers = self.workers.lock().expect("netns worker map poisoned");
            let worker = workers
                .get(ns)
                .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
            worker.ns_fd.clone()
        };
        let ns_name = ns.to_string();
        Ok(thread::spawn(move || {
            let target = fd
                .try_clone()
                .with_context(|| format!("clone fd for spawned thread in '{ns_name}'"))?;
            setns(&target, CloneFlags::CLONE_NEWNET)
                .with_context(|| format!("setns for spawned thread in '{ns_name}'"))?;
            f()
        }))
    }

    /// Clone the namespace fd (for external use like moving veth endpoints).
    pub fn ns_fd(&self, ns: &str) -> Result<File> {
        let workers = self.workers.lock().expect("netns worker map poisoned");
        let worker = workers
            .get(ns)
            .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
        worker
            .ns_fd
            .try_clone()
            .with_context(|| format!("clone ns fd for '{ns}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_cleanup() {
        let mgr = NetnsManager::new();
        mgr.create_netns("test-ns-1").unwrap();
        mgr.create_netns("test-ns-2").unwrap();
        mgr.remove_worker("test-ns-1");
        // test-ns-2 still exists
        assert!(mgr.rt_handle_for("test-ns-2").is_ok());
        assert!(mgr.rt_handle_for("test-ns-1").is_err());
    }

    #[test]
    fn prefix_cleanup() {
        let mgr = NetnsManager::new();
        mgr.create_netns("lab-a").unwrap();
        mgr.create_netns("lab-b").unwrap();
        mgr.create_netns("other").unwrap();
        mgr.cleanup_prefix("lab-");
        assert!(mgr.rt_handle_for("lab-a").is_err());
        assert!(mgr.rt_handle_for("lab-b").is_err());
        assert!(mgr.rt_handle_for("other").is_ok());
    }
}
