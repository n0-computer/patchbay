//! Network namespace lifecycle helpers.
//!
//! Each namespace gets an unconditional async worker thread (with a
//! `current_thread` tokio runtime) and a lazy sync worker thread.
//! The async worker thread is the same OS thread that creates the namespace
//! via `unshare(CLONE_NEWNET)`, saving one thread spawn per namespace.

use std::{
    collections::HashMap,
    fs::File,
    os::unix::fs::MetadataExt,
    sync::{mpsc, Arc, Mutex},
    thread,
};

use anyhow::{anyhow, Context, Result};
use nix::sched::{setns, unshare, CloneFlags};
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span};

use crate::netlink::Netlink;

// ─────────────────────────────────────────────
// Thread-local namespace setup (shared by all worker types)
// ─────────────────────────────────────────────

/// DNS overlay paths for bind-mounting `/etc/hosts` and `/etc/resolv.conf`.
#[derive(Clone, Debug)]
pub(crate) struct DnsOverlay {
    /// Path to the generated hosts file for this namespace.
    pub hosts_path: std::path::PathBuf,
    /// Path to the generated resolv.conf for this lab.
    pub resolv_path: std::path::PathBuf,
}

impl DnsOverlay {
    /// Bind-mounts hosts and resolv.conf in the current thread's mount namespace.
    /// Requires a prior `unshare(CLONE_NEWNS)`.
    fn apply(&self) {
        if let Err(e) = bind_mount(&self.hosts_path, c"/etc/hosts") {
            debug!(error = %e, "dns overlay: hosts bind mount failed");
        }
        if let Err(e) = bind_mount(&self.resolv_path, c"/etc/resolv.conf") {
            debug!(error = %e, "dns overlay: resolv.conf bind mount failed");
        }
    }
}

/// Private mount namespace + optional DNS overlay bind-mounts.
/// Called on every thread that enters a namespace (sync, async, user, blocking pool).
fn apply_mount_overlay(overlay: Option<&DnsOverlay>) {
    let _ = unshare(CloneFlags::CLONE_NEWNS);
    if let Some(o) = overlay {
        o.apply();
    }
}

/// Enters an existing namespace via `setns` and applies mount overlay.
fn enter_namespace(fd: &File, overlay: Option<&DnsOverlay>) -> Result<()> {
    setns(fd, CloneFlags::CLONE_NEWNET).context("setns CLONE_NEWNET")?;
    apply_mount_overlay(overlay);
    Ok(())
}

fn bind_mount(src: &std::path::Path, dst: &std::ffi::CStr) -> Result<()> {
    use std::ffi::CString;
    let src_c = CString::new(src.as_os_str().as_encoded_bytes()).context("invalid path")?;
    unsafe { libc::umount2(dst.as_ptr(), libc::MNT_DETACH) };
    let ret = unsafe {
        libc::mount(
            src_c.as_ptr(),
            dst.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        anyhow::bail!(
            "bind mount {} -> {:?}: {}",
            src.display(),
            dst,
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Builds a thread name like `{ns}:{suffix}`, truncated to 15 chars
/// (Linux `pthread_setname_np` limit). When ns is too long, its leading
/// characters are trimmed.
fn thread_name(ns: &str, suffix: &str) -> String {
    let max = 15;
    let budget = max - suffix.len() - 1; // -1 for ':'
    if ns.len() <= budget {
        format!("{ns}:{suffix}")
    } else {
        format!("{}:{suffix}", &ns[ns.len() - budget..])
    }
}

fn open_current_thread_netns_fd() -> Result<File> {
    if let Ok(fd) = File::open("/proc/thread-self/ns/net") {
        return Ok(fd);
    }
    let tid = nix::unistd::gettid();
    let path = format!("/proc/self/task/{}/ns/net", tid.as_raw());
    if let Ok(fd) = File::open(&path) {
        return Ok(fd);
    }
    File::open("/proc/self/ns/net").with_context(|| format!("open netns fd (tried {path})"))
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
    fn spawn(
        ns: &str,
        fd: &File,
        span: tracing::Span,
        overlay: Option<DnsOverlay>,
    ) -> Result<Self> {
        let target = fd.try_clone().context("clone fd for sync worker")?;
        let (tx, rx) = mpsc::sync_channel(64);
        let join = thread::Builder::new()
            .name(thread_name(ns, "sw"))
            .spawn(move || {
                let _guard = span.entered();
                if let Err(e) = enter_namespace(&target, overlay.as_ref()) {
                    debug!(error = %e, "sync worker: enter_namespace failed");
                    return;
                }
                while let Ok(msg) = rx.recv() {
                    match msg {
                        SyncMsg::Task(f) => f(),
                        SyncMsg::Shutdown => break,
                    }
                }
            })
            .context("spawn sync worker thread")?;
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
            if j.thread().id() != thread::current().id() {
                let _ = j.join();
            }
        }
    }
}

// ─────────────────────────────────────────────
// Worker — per-namespace async RT + lazy sync worker + ns fd
// ─────────────────────────────────────────────

struct Worker {
    ns: String,
    parent_span: tracing::Span,
    ns_fd: Arc<File>,
    rt_handle: tokio::runtime::Handle,
    netlink: Mutex<Option<Netlink>>,
    cancel: CancellationToken,
    async_join: Mutex<Option<thread::JoinHandle<()>>>,
    sync_worker: Mutex<Option<SyncWorker>>,
    dns_overlay: Option<DnsOverlay>,
}

/// Sent back from the async worker thread after namespace creation.
struct WorkerInit {
    ns_fd: File,
    rt_handle: tokio::runtime::Handle,
}

impl Worker {
    /// Spawns the async worker thread which *creates* the namespace via
    /// `unshare(CLONE_NEWNET)`, builds a tokio RT, and stays alive.
    fn spawn(
        ns: &str,
        parent_span: tracing::Span,
        dns_overlay: Option<DnsOverlay>,
    ) -> Result<Self> {
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let span = debug_span!(parent: &parent_span, "async", ns = %ns);
        let overlay = dns_overlay.clone();
        let (init_tx, init_rx) = mpsc::channel::<Result<WorkerInit>>();

        let join = thread::Builder::new()
            .name(thread_name(ns, "aw"))
            .spawn(move || {
                let _guard = span.entered();
                let init = (|| -> Result<(File, tokio::runtime::Runtime)> {
                    unshare(CloneFlags::CLONE_NEWNET).context("unshare CLONE_NEWNET")?;
                    apply_mount_overlay(overlay.as_ref());
                    let ns_fd = open_current_thread_netns_fd()?;
                    let mut builder = tokio::runtime::Builder::new_current_thread();
                    builder.enable_all();
                    if let Some(overlay) = overlay {
                        builder.on_thread_start(move || apply_mount_overlay(Some(&overlay)));
                    }
                    let rt = builder.build().context("build tokio runtime")?;
                    Ok((ns_fd, rt))
                })();

                match init {
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                    }
                    Ok((ns_fd, rt)) => {
                        let fd = match ns_fd.try_clone() {
                            Ok(fd) => fd,
                            Err(e) => {
                                let _ = init_tx.send(Err(e.into()));
                                return;
                            }
                        };
                        let _ = init_tx.send(Ok(WorkerInit {
                            ns_fd: fd,
                            rt_handle: rt.handle().clone(),
                        }));
                        rt.block_on(cancel2.cancelled());
                        debug!("async worker shutting down");
                    }
                }
            })
            .context("spawn async worker thread")?;

        let init = init_rx
            .recv()
            .context("async worker init channel closed")??;

        // Sanity: verify the new namespace is actually isolated.
        let created_ino = init.ns_fd.metadata().context("stat created ns fd")?.ino();
        let current_ino = open_current_thread_netns_fd()
            .context("open caller netns for sanity check")?
            .metadata()
            .context("stat caller ns fd")?
            .ino();
        if created_ino == current_ino {
            anyhow::bail!(
                "namespace creation returned caller's namespace (inode {created_ino}); not isolated"
            );
        }

        Ok(Worker {
            ns: ns.to_string(),
            parent_span,
            ns_fd: Arc::new(init.ns_fd),
            rt_handle: init.rt_handle,
            netlink: Mutex::new(None),
            cancel,
            async_join: Mutex::new(Some(join)),
            sync_worker: Mutex::new(None),
            dns_overlay,
        })
    }

    /// Returns a clone of the namespace's persistent Netlink handle (lazy init).
    fn netlink(&self) -> Result<Netlink> {
        let mut guard = self.netlink.lock().expect("netlink mutex poisoned");
        if let Some(ref nl) = *guard {
            return Ok(nl.clone());
        }
        let (tx, rx) = mpsc::channel();
        self.rt_handle.spawn(async move {
            let result = async {
                let (conn, handle, _) =
                    rtnetlink::new_connection().context("rtnetlink new_connection")?;
                tokio::spawn(conn);
                Ok::<Netlink, anyhow::Error>(Netlink::new(handle))
            }
            .await;
            let _ = tx.send(result);
        });
        let nl = rx.recv().context("netlink init channel closed")??;
        *guard = Some(nl.clone());
        Ok(nl)
    }

    fn sync_tx(&self) -> Result<mpsc::SyncSender<SyncMsg>> {
        let mut guard = self.sync_worker.lock().expect("sync worker mutex poisoned");
        if guard.is_none() {
            let span = debug_span!(parent: &self.parent_span, "sync", ns = %self.ns);
            *guard = Some(SyncWorker::spawn(
                &self.ns,
                &self.ns_fd,
                span,
                self.dns_overlay.clone(),
            )?);
        }
        Ok(guard.as_ref().unwrap().tx.clone())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(j) = self.async_join.lock().expect("async_join poisoned").take() {
            if j.thread().id() != thread::current().id() {
                let _ = j.join();
            }
        }
        // SyncWorker drops via its own Drop impl.
    }
}

// ─────────────────────────────────────────────
// NetnsManager
// ─────────────────────────────────────────────

/// Manages per-namespace worker threads and file descriptors.
///
/// Each namespace gets an unconditional async worker (tokio `current_thread`
/// RT) and a lazy sync worker. The async worker thread is the same OS thread
/// that creates the namespace via `unshare(CLONE_NEWNET)`.
pub(crate) struct NetnsManager {
    parent_span: tracing::Span,
    workers: Mutex<HashMap<String, Worker>>,
}

impl Default for NetnsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl NetnsManager {
    pub(crate) fn new() -> Self {
        Self {
            parent_span: tracing::Span::none(),
            workers: Mutex::new(HashMap::new()),
        }
    }

    // ── Namespace lifecycle ──────────────────────────────────────────

    /// Create a new isolated network namespace and register it.
    ///
    /// Spawns a thread that calls `unshare(CLONE_NEWNET)` to create the
    /// namespace, applies the optional DNS overlay, builds a tokio runtime,
    /// and stays alive as the namespace's async worker.
    pub(crate) fn create_netns(&self, name: &str, dns_overlay: Option<DnsOverlay>) -> Result<()> {
        debug!(ns = %name, "create namespace");
        self.remove_worker(name);
        let worker = Worker::spawn(name, self.parent_span.clone(), dns_overlay)?;
        self.workers
            .lock()
            .expect("netns worker map poisoned")
            .insert(name.to_string(), worker);
        Ok(())
    }

    /// Remove workers/fds for all namespaces matching `prefix`.
    pub(crate) fn cleanup_prefix(&self, prefix: &str) {
        let mut workers = self.workers.lock().expect("netns worker map poisoned");
        workers.retain(|k, _| !k.starts_with(prefix));
    }

    /// Removes a namespace worker. `Drop` cancels its token and joins threads.
    pub(crate) fn remove_worker(&self, name: &str) {
        let mut workers = self.workers.lock().expect("netns worker map poisoned");
        workers.remove(name);
    }

    // ── Async ────────────────────────────────────────────────────────

    /// Returns a cloned tokio `Handle` for the namespace's async worker.
    pub(crate) fn rt_handle_for(&self, ns: &str) -> Result<tokio::runtime::Handle> {
        let workers = self.workers.lock().expect("netns worker map poisoned");
        let w = workers
            .get(ns)
            .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
        Ok(w.rt_handle.clone())
    }

    /// Returns a clone of the namespace's persistent Netlink handle.
    pub(crate) fn netlink_for(&self, ns: &str) -> Result<Netlink> {
        let workers = self.workers.lock().expect("netns worker map poisoned");
        let w = workers
            .get(ns)
            .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
        w.netlink()
    }

    // ── Sync ─────────────────────────────────────────────────────────

    /// Run a short-lived sync closure inside `ns`. Blocks the caller.
    ///
    /// Only for fast non-I/O work (sysctl, `Command::spawn`).
    pub(crate) fn run_closure_in<F, R>(&self, ns: &str, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let tx = {
            let workers = self.workers.lock().expect("netns worker map poisoned");
            let w = workers
                .get(ns)
                .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
            w.sync_tx()?
        };
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        tx.send(SyncMsg::Task(Box::new(move || {
            let _ = result_tx.send(f());
        })))
        .map_err(|_| anyhow!("sync worker for '{ns}' disconnected"))?;
        result_rx
            .recv()
            .context("sync worker result channel closed")?
    }

    /// Spawn a dedicated OS thread inside `ns`. Non-blocking.
    pub(crate) fn spawn_thread_in<F, R>(&self, ns: &str, f: F) -> Result<thread::JoinHandle<Result<R>>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (fd, overlay) = {
            let workers = self.workers.lock().expect("netns worker map poisoned");
            let w = workers
                .get(ns)
                .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
            (w.ns_fd.clone(), w.dns_overlay.clone())
        };
        thread::Builder::new()
            .name(thread_name(ns, "u"))
            .spawn(move || {
                enter_namespace(&fd, overlay.as_ref())?;
                f()
            })
            .context("spawn user thread")
    }

    /// Clone the namespace fd (for moving veth endpoints etc).
    pub(crate) fn ns_fd(&self, ns: &str) -> Result<File> {
        let workers = self.workers.lock().expect("netns worker map poisoned");
        let w = workers
            .get(ns)
            .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
        w.ns_fd.try_clone().context("clone ns fd")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_cleanup() {
        let mgr = NetnsManager::new();
        mgr.create_netns("test-ns-1", None).unwrap();
        mgr.create_netns("test-ns-2", None).unwrap();
        mgr.remove_worker("test-ns-1");
        assert!(mgr.rt_handle_for("test-ns-2").is_ok());
        assert!(mgr.rt_handle_for("test-ns-1").is_err());
    }

    #[test]
    fn prefix_cleanup() {
        let mgr = NetnsManager::new();
        mgr.create_netns("lab-a", None).unwrap();
        mgr.create_netns("lab-b", None).unwrap();
        mgr.create_netns("other", None).unwrap();
        mgr.cleanup_prefix("lab-");
        assert!(mgr.rt_handle_for("lab-a").is_err());
        assert!(mgr.rt_handle_for("lab-b").is_err());
        assert!(mgr.rt_handle_for("other").is_ok());
    }
}
