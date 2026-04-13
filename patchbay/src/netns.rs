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
    path::PathBuf,
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

/// Per-namespace options: DNS overlay + run output directory for tracing.
#[derive(Clone, Default, derive_more::Debug)]
pub(crate) struct NamespaceOpts {
    /// DNS overlay paths for bind-mounting `/etc/hosts` and `/etc/resolv.conf`.
    pub dns_overlay: Option<DnsOverlay>,
    /// Run output directory for per-namespace tracing logs.
    pub run_dir: Option<PathBuf>,
    /// Log file prefix like `"device.client"` or `"router.home"`.
    /// Used to name `{prefix}.tracing.jsonl` and `{prefix}.events.jsonl`.
    pub log_prefix: Option<String>,
    /// Pre-existing tracing dispatch to reuse instead of creating a new subscriber.
    /// Set on the sync worker so it shares the async worker's file handles.
    #[debug(skip)]
    pub tracing_dispatch: Option<tracing::Dispatch>,
}

/// DNS overlay paths for bind-mounting `/etc/hosts` and `/etc/resolv.conf`.
#[derive(Clone, Debug)]
pub(crate) struct DnsOverlay {
    /// Path to the generated hosts file for this namespace.
    pub hosts_path: PathBuf,
    /// Path to the generated resolv.conf for this lab.
    pub resolv_path: PathBuf,
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

/// Applies mount overlay and installs per-namespace tracing subscriber.
/// Called on every worker thread that enters or creates a namespace.
///
/// Returns a tracing `DefaultGuard` that must be held for the thread's lifetime.
fn setup_namespace_thread(
    _name: &str,
    opts: &NamespaceOpts,
) -> Option<tracing::subscriber::DefaultGuard> {
    apply_mount_overlay(opts.dns_overlay.as_ref());
    if let Some(dispatch) = &opts.tracing_dispatch {
        // Reuse the async worker's dispatch so both workers share the same
        // file handles and avoid truncation/interleaving of log files.
        Some(tracing::dispatcher::set_default(dispatch))
    } else {
        // Only install file-writing tracing when log_prefix is set (routers/devices).
        // The root namespace (IX) has no log_prefix and should not create tracing files.
        let run_dir = opts.log_prefix.as_ref().and(opts.run_dir.as_ref());
        let log_name = opts.log_prefix.as_deref().unwrap_or("ns");
        crate::ns_tracing::install_namespace_subscriber(log_name, run_dir.map(|p| p.as_path()))
    }
}

/// Private mount namespace + remount `/proc` + optional DNS overlay bind-mounts.
/// Called on every thread that enters a namespace (sync, async, user, blocking pool).
///
/// We always create a private mount namespace and remount `/proc` so that
/// `/proc/net/route` (and other `/proc/net/*` files) reflect *this* network
/// namespace's state instead of the host's. Without this, libraries that read
/// `/proc/net/route` (e.g. netwatch) get the host's default route interface.
fn apply_mount_overlay(overlay: Option<&DnsOverlay>) {
    if let Err(e) = unshare(CloneFlags::CLONE_NEWNS) {
        tracing::warn!(
            "unshare(CLONE_NEWNS) failed: {e} — /proc and DNS overlays may show host data"
        );
        return;
    }
    // Make all mounts private so bind mounts in this namespace do not
    // propagate to the parent. The user namespace already prevents
    // affecting the real root filesystem, but this is a low-cost
    // safeguard against propagation within the lab's namespace tree.
    let ret = unsafe {
        libc::mount(
            c"none".as_ptr(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        tracing::warn!(
            "mount --make-rprivate / failed: {} — bind mounts may propagate within the lab",
            std::io::Error::last_os_error()
        );
    }
    fixup_proc_net();
    if let Some(o) = overlay {
        o.apply();
    }
}

/// Bind-mount `/proc/thread-self/net` over `/proc/net` so that
/// `/proc/net/route` (and other `/proc/net/*` files) reflect *this thread's*
/// network namespace instead of the process's original one.
///
/// On Linux, `/proc/net` is a symlink to `self/net` which resolves to the
/// *thread group leader's* network namespace, not the calling thread's. After
/// `setns(CLONE_NEWNET)`, only `/proc/thread-self/net` reflects the new
/// namespace. This bind-mount makes the standard `/proc/net/route` path work
/// for libraries like `netwatch` that don't know about `thread-self`.
fn fixup_proc_net() {
    // First remove the symlink so we can mount over it
    let ret = unsafe {
        libc::mount(
            c"/proc/thread-self/net".as_ptr(),
            c"/proc/net".as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        tracing::warn!("bind-mount /proc/thread-self/net -> /proc/net failed: {err}");
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
    fn spawn(ns: &str, fd: &File, span: tracing::Span, opts: NamespaceOpts) -> Result<Self> {
        let target = fd.try_clone().context("clone fd for sync worker")?;
        let (tx, rx) = mpsc::sync_channel(64);
        let ns_name = ns.to_string();
        let join = thread::Builder::new()
            .name(thread_name(ns, "sw"))
            .spawn(move || {
                let _guard = span.entered();
                setns(&target, CloneFlags::CLONE_NEWNET)
                    .expect("sync worker: setns CLONE_NEWNET failed");
                let _tracing_guard = setup_namespace_thread(&ns_name, &opts);
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
    opts: NamespaceOpts,
    /// The tracing dispatch from the async worker, shared with the sync worker.
    tracing_dispatch: tracing::Dispatch,
}

/// Sent back from the async worker thread after namespace creation.
struct WorkerInit {
    ns_fd: File,
    rt_handle: tokio::runtime::Handle,
    /// The tracing dispatch installed on the async worker thread,
    /// so the sync worker can reuse it instead of creating a new one.
    tracing_dispatch: tracing::Dispatch,
}

impl Worker {
    /// Spawns the async worker thread which *creates* the namespace via
    /// `unshare(CLONE_NEWNET)`, builds a tokio RT, and stays alive.
    fn spawn(ns: &str, parent_span: tracing::Span, opts: NamespaceOpts) -> Result<Self> {
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let span = debug_span!(parent: &parent_span, "async", ns = %ns);
        let thread_opts = opts.clone();
        let ns_name = ns.to_string();
        let (init_tx, init_rx) = mpsc::channel::<Result<WorkerInit>>();

        let join = thread::Builder::new()
            .name(thread_name(ns, "aw"))
            .spawn(move || {
                let _guard = span.entered();
                let init = (|| -> Result<(File, tokio::runtime::Runtime)> {
                    unshare(CloneFlags::CLONE_NEWNET).context("unshare CLONE_NEWNET")?;
                    let ns_fd = open_current_thread_netns_fd()?;
                    let mut builder = tokio::runtime::Builder::new_current_thread();
                    builder.enable_all();
                    let overlay_for_threads = thread_opts.dns_overlay.clone();
                    builder
                        .on_thread_start(move || apply_mount_overlay(overlay_for_threads.as_ref()));
                    let rt = builder.build().context("build tokio runtime")?;
                    Ok((ns_fd, rt))
                })();

                match init {
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                    }
                    Ok((ns_fd, rt)) => {
                        // Install tracing subscriber for this namespace thread.
                        // The guard lives until the thread exits, ensuring proper flush.
                        let _tracing_guard = setup_namespace_thread(&ns_name, &thread_opts);
                        // Capture the thread-default dispatch so the sync worker can
                        // reuse it — sharing file handles avoids truncation/interleaving.
                        let tracing_dispatch = tracing::dispatcher::get_default(|d| d.clone());
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
                            tracing_dispatch,
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
            opts,
            tracing_dispatch: init.tracing_dispatch,
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
            let mut opts = self.opts.clone();
            opts.tracing_dispatch = Some(self.tracing_dispatch.clone());
            *guard = Some(SyncWorker::spawn(&self.ns, &self.ns_fd, span, opts)?);
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
    /// Run output directory for per-namespace tracing logs.
    run_dir: Option<PathBuf>,
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
            run_dir: None,
        }
    }

    /// Set the run output directory for per-namespace tracing logs.
    pub(crate) fn set_run_dir(&mut self, run_dir: PathBuf) {
        self.run_dir = Some(run_dir);
    }

    // ── Namespace lifecycle ──────────────────────────────────────────

    /// Create a new isolated network namespace and register it.
    ///
    /// Spawns a thread that calls `unshare(CLONE_NEWNET)` to create the
    /// namespace, applies the optional DNS overlay, builds a tokio runtime,
    /// and stays alive as the namespace's async worker.
    pub(crate) fn create_netns(
        &self,
        name: &str,
        dns_overlay: Option<DnsOverlay>,
        log_prefix: Option<String>,
    ) -> Result<()> {
        debug!(ns = %name, "create namespace");
        self.remove_worker(name);
        let opts = NamespaceOpts {
            dns_overlay,
            run_dir: self.run_dir.clone(),
            log_prefix,
            tracing_dispatch: None,
        };
        let worker = Worker::spawn(name, self.parent_span.clone(), opts)?;
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
    pub(crate) fn spawn_thread_in<F, R>(
        &self,
        ns: &str,
        f: F,
    ) -> Result<thread::JoinHandle<Result<R>>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (fd, overlay) = {
            let workers = self.workers.lock().expect("netns worker map poisoned");
            let w = workers
                .get(ns)
                .ok_or_else(|| anyhow!("namespace '{ns}' not registered"))?;
            (w.ns_fd.clone(), w.opts.dns_overlay.clone())
        };
        thread::Builder::new()
            .name(thread_name(ns, "u"))
            .spawn(move || {
                enter_namespace(&fd, overlay.as_ref())?;
                f()
            })
            .context("spawn user thread")
    }

    /// Get the tracing dispatch for a namespace.
    pub(crate) fn dispatch_for(&self, ns: &str) -> Option<tracing::Dispatch> {
        let workers = self.workers.lock().expect("netns worker map poisoned");
        workers.get(ns).map(|w| w.tracing_dispatch.clone())
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
        mgr.create_netns("test-ns-1", None, None).unwrap();
        mgr.create_netns("test-ns-2", None, None).unwrap();
        mgr.remove_worker("test-ns-1");
        assert!(mgr.rt_handle_for("test-ns-2").is_ok());
        assert!(mgr.rt_handle_for("test-ns-1").is_err());
    }

    #[test]
    fn prefix_cleanup() {
        let mgr = NetnsManager::new();
        mgr.create_netns("lab-a", None, None).unwrap();
        mgr.create_netns("lab-b", None, None).unwrap();
        mgr.create_netns("other", None, None).unwrap();
        mgr.cleanup_prefix("lab-");
        assert!(mgr.rt_handle_for("lab-a").is_err());
        assert!(mgr.rt_handle_for("lab-b").is_err());
        assert!(mgr.rt_handle_for("other").is_ok());
    }
}
