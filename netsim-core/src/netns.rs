//! Network namespace lifecycle helpers using an in-memory FD registry.

use anyhow::{anyhow, Context, Result};
use nix::sched::{setns, unshare, CloneFlags};
use nix::unistd::gettid;
use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::pin::Pin;
use std::process::{Child, Command, ExitStatus};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::task::{Context as TaskContext, Poll};
use std::thread;
use tokio::sync::oneshot;
use tracing::{debug, debug_span, error, Instrument as _};

use crate::netlink::Netlink;

// ─────────────────────────────────────────────
// FD registry
// ─────────────────────────────────────────────

#[derive(Default)]
struct FdRegistry {
    map: Mutex<HashMap<String, Arc<File>>>,
}

impl FdRegistry {
    fn insert(&self, name: &str, fd: File) {
        let mut m = self.map.lock().expect("fd registry poisoned");
        m.insert(name.to_string(), Arc::new(fd));
    }

    fn get(&self, name: &str) -> Option<Arc<File>> {
        let m = self.map.lock().expect("fd registry poisoned");
        m.get(name).cloned()
    }

    fn remove(&self, name: &str) {
        let mut m = self.map.lock().expect("fd registry poisoned");
        m.remove(name);
    }

    fn remove_prefix(&self, prefix: &str) {
        let mut m = self.map.lock().expect("fd registry poisoned");
        m.retain(|k, _| !k.starts_with(prefix));
    }
}

static FD_REGISTRY: OnceLock<FdRegistry> = OnceLock::new();
static GLOBAL_NETNS_MANAGER: OnceLock<NetnsManager> = OnceLock::new();

fn fd_registry() -> &'static FdRegistry {
    FD_REGISTRY.get_or_init(FdRegistry::default)
}

fn global_netns_manager() -> &'static NetnsManager {
    GLOBAL_NETNS_MANAGER.get_or_init(NetnsManager::new)
}

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

/// Ensure netns runtime prerequisites are initialized.
pub fn ensure_netns_dir() -> Result<()> {
    debug!("netns: using fd backend");
    Ok(())
}

/// Open a namespace FD for `name` from the in-memory registry.
pub fn open_netns_fd(name: &str) -> Result<File> {
    let fd = fd_registry()
        .get(name)
        .ok_or_else(|| anyhow!("netns '{name}' not found"))?;
    fd.try_clone()
        .with_context(|| format!("clone netns fd for '{name}'"))
}

/// Delete a namespace by name from the in-memory registry.
pub fn cleanup_netns(name: &str) {
    fd_registry().remove(name);
}

/// Remove in-memory namespace handles for all names with the given `prefix`.
pub fn cleanup_registry_prefix(prefix: &str) {
    fd_registry().remove_prefix(prefix);
}

/// Create a namespace entry for `name` using the fd backend.
pub async fn create_named_netns(name: &str) -> Result<()> {
    debug!(ns = %name, "netns: create namespace");
    cleanup_netns(name);

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
    fd_registry().insert(name, fd);

    let _ = open_netns_fd(name).with_context(|| format!("open netns fd for '{name}'"))?;
    Ok(())
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
// TaskHandle / TaskCancelled
// ─────────────────────────────────────────────

/// Returned when a namespace task's result could not be received (worker dropped).
#[derive(Debug, derive_more::Display)]
#[display("Task cancelled")]
pub struct TaskCancelled;

impl std::error::Error for TaskCancelled {}

/// A handle to an async task running inside a namespace worker.
/// Implements `Future`; resolves to `Result<T, TaskCancelled>`.
pub struct TaskHandle<T>(oneshot::Receiver<T>);

impl<T> Future for TaskHandle<T> {
    type Output = Result<T, TaskCancelled>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|r| r.map_err(|_| TaskCancelled))
    }
}

// ─────────────────────────────────────────────
// AsyncWorker — persistent LocalSet
// ─────────────────────────────────────────────

type BoxFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

enum AsyncMsg {
    #[allow(dead_code)]
    Task(Box<dyn FnOnce() -> BoxFuture + Send>),
    /// Task that receives a clone of the namespace's persistent `Netlink` handle.
    NetlinkTask(Box<dyn FnOnce(Netlink) -> BoxFuture + Send>),
    Shutdown,
}

struct AsyncWorker {
    tx: tokio::sync::mpsc::UnboundedSender<AsyncMsg>,
    join: Option<thread::JoinHandle<()>>,
}

impl AsyncWorker {
    fn spawn(ns: &str, parent_span: &tracing::Span) -> Result<Self> {
        let target = open_netns_fd(ns)
            .with_context(|| format!("open namespace fd for async worker '{ns}'"))?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ns_name = ns.to_string();
        let span = debug_span!(parent: parent_span, "worker", ns = %ns);
        let join = thread::spawn(move || async_worker_main(ns_name, target, rx, span));
        Ok(Self {
            tx,
            join: Some(join),
        })
    }
}

impl Drop for AsyncWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(AsyncMsg::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

static TASK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn async_worker_main(
    _ns: String,
    target: File,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<AsyncMsg>,
    span: tracing::Span,
) {
    let _guard = span.clone().entered();
    if let Err(err) = setns(&target, CloneFlags::CLONE_NEWNET) {
        error!(error = %err, "async netns worker: setns failed");
        return;
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            error!(error = %err, "async netns worker: runtime build failed");
            return;
        }
    };

    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async move {
        // Create one rtnetlink connection per namespace worker.  Netlink is Clone
        // (cheap Arc-based Handle), so each task gets its own clone.
        let netlink: Option<Netlink> = match rtnetlink::new_connection() {
            Ok((conn, handle, _)) => {
                tokio::task::spawn_local(conn);
                Some(Netlink::new(handle))
            }
            Err(err) => {
                error!(error = %err, "async netns worker: rtnetlink connection failed");
                None
            }
        };

        while let Some(msg) = rx.recv().await {
            let id = TASK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match msg {
                AsyncMsg::Task(f) => {
                    let s = debug_span!(parent: &span, "task", id);
                    tokio::task::spawn_local(f().instrument(s));
                }
                AsyncMsg::NetlinkTask(f) => {
                    if let Some(nl) = netlink.as_ref() {
                        let s = debug_span!(parent: &span, "nl", id);
                        tokio::task::spawn_local(f(nl.clone()).instrument(s));
                    }
                    // else: factory dropped → result_tx dropped → TaskHandle resolves Err
                }
                AsyncMsg::Shutdown => {
                    debug!("worker received shutdown");
                    break;
                }
            }
        }
        debug!("worker loop ended, LocalSet dropping");
    }));
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
    fn spawn(ns: &str, parent_span: &tracing::Span) -> Result<Self> {
        let target = open_netns_fd(ns)
            .with_context(|| format!("open namespace fd for sync worker '{ns}'"))?;
        let (tx, rx) = mpsc::sync_channel(64);
        let ns_name = ns.to_string();
        let span = debug_span!(parent: parent_span, "worker", ns = %ns);
        let join = thread::spawn(move || sync_worker_main(ns_name, target, rx, span));
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
            let _ = j.join();
        }
    }
}

fn sync_worker_main(_ns: String, target: File, rx: mpsc::Receiver<SyncMsg>, span: tracing::Span) {
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
// Worker — holds lazy async + sync workers
// ─────────────────────────────────────────────

struct Worker {
    ns: String,
    parent_span: tracing::Span,
    async_worker: Mutex<Option<AsyncWorker>>,
    sync_worker: Mutex<Option<SyncWorker>>,
}

impl Worker {
    fn new(ns: &str, parent_span: tracing::Span) -> Self {
        Self {
            ns: ns.to_string(),
            parent_span,
            async_worker: Mutex::new(None),
            sync_worker: Mutex::new(None),
        }
    }

    fn async_tx(&self) -> Result<tokio::sync::mpsc::UnboundedSender<AsyncMsg>> {
        let mut guard = self
            .async_worker
            .lock()
            .expect("async worker mutex poisoned");
        if guard.is_none() {
            *guard = Some(AsyncWorker::spawn(&self.ns, &self.parent_span)?);
        }
        Ok(guard.as_ref().unwrap().tx.clone())
    }

    fn sync_tx(&self) -> Result<mpsc::SyncSender<SyncMsg>> {
        let mut guard = self.sync_worker.lock().expect("sync worker mutex poisoned");
        if guard.is_none() {
            *guard = Some(SyncWorker::spawn(&self.ns, &self.parent_span)?);
        }
        Ok(guard.as_ref().unwrap().tx.clone())
    }
}

// ─────────────────────────────────────────────
// NetnsManager
// ─────────────────────────────────────────────

/// Executes tasks inside dedicated per-namespace worker threads.
///
/// Each namespace gets one long-lived async worker thread with a persistent
/// `LocalSet`, and one sync worker thread for short-lived blocking operations.
/// Workers are started lazily on first use.
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
    /// Create an empty namespace manager.
    pub fn new() -> Self {
        Self {
            parent_span: tracing::Span::none(),
            workers: Mutex::new(HashMap::new()),
        }
    }

    /// Create a namespace manager with a parent tracing span.
    pub fn new_with_span(parent_span: tracing::Span) -> Self {
        Self {
            parent_span,
            workers: Mutex::new(HashMap::new()),
        }
    }

    fn async_tx_for(&self, ns: &str) -> Result<tokio::sync::mpsc::UnboundedSender<AsyncMsg>> {
        let mut workers = self.workers.lock().expect("netns worker map poisoned");
        if !workers.contains_key(ns) {
            workers.insert(ns.to_string(), Worker::new(ns, self.parent_span.clone()));
        }
        workers.get(ns).expect("just inserted").async_tx()
    }

    fn sync_tx_for(&self, ns: &str) -> Result<mpsc::SyncSender<SyncMsg>> {
        let mut workers = self.workers.lock().expect("netns worker map poisoned");
        if !workers.contains_key(ns) {
            workers.insert(ns.to_string(), Worker::new(ns, self.parent_span.clone()));
        }
        workers.get(ns).expect("just inserted").sync_tx()
    }

    /// Enqueue an async task on the namespace's persistent tokio RT.
    ///
    /// Returns a `TaskHandle` (a `Future`) that resolves to the task's output.
    /// This call is sync and non-blocking — safe from any async or sync context.
    #[allow(dead_code)]
    pub fn spawn_task_in<F, Fut, T>(&self, ns: &str, f: F) -> TaskHandle<T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();
        match self.async_tx_for(ns) {
            Ok(tx) => {
                let _ = tx.send(AsyncMsg::Task(Box::new(move || {
                    Box::pin(async move {
                        let result = f().await;
                        let _ = result_tx.send(result);
                    })
                })));
            }
            Err(e) => {
                debug!("spawn_task_in ns={ns}: {e}");
                // result_tx drops here → TaskHandle resolves to Err(TaskCancelled)
            }
        }
        TaskHandle(result_rx)
    }

    /// Enqueue an async task that receives a clone of the namespace's `Netlink`.
    ///
    /// `Netlink` is `Clone` (cheap Arc-based Handle); each task gets its own clone.
    /// Returns a `TaskHandle` that resolves to the task's output.
    pub fn spawn_netlink_task_in<F, Fut, T>(&self, ns: &str, f: F) -> TaskHandle<T>
    where
        F: FnOnce(Netlink) -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();
        match self.async_tx_for(ns) {
            Ok(tx) => {
                let _ = tx.send(AsyncMsg::NetlinkTask(Box::new(move |nl| {
                    Box::pin(async move {
                        let result = f(nl).await;
                        let _ = result_tx.send(result);
                    })
                })));
            }
            Err(e) => {
                debug!("spawn_netlink_task_in ns={ns}: {e}");
            }
        }
        TaskHandle(result_rx)
    }

    /// Spawn a persistent OS thread inside `ns`. Non-blocking.
    #[allow(dead_code)]
    pub fn spawn_thread_in<F>(&self, ns: &str, f: F) -> thread::JoinHandle<()>
    where
        F: FnOnce() + Send + 'static,
    {
        let ns = ns.to_string();
        thread::spawn(move || match open_netns_fd(&ns) {
            Ok(fd) => {
                if setns(&fd, CloneFlags::CLONE_NEWNET).is_ok() {
                    f();
                }
            }
            Err(e) => debug!("spawn_thread_in ns={ns}: {e}"),
        })
    }

    /// Run a short-lived sync closure inside `ns`. Blocks the caller.
    ///
    /// Only for fast, non-blocking work (e.g. sysctl writes, `Command::spawn`).
    /// Never pass TCP/UDP I/O here — use `spawn_task_in` instead.
    pub fn run_closure_in<F, R>(&self, ns: &str, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let tx = self.sync_tx_for(ns)?;
        tx.send(SyncMsg::Task(Box::new(move || {
            let _ = result_tx.send(f());
        })))
        .map_err(|_| anyhow!("send task to sync netns worker for '{ns}' failed"))?;
        result_rx
            .recv()
            .context("receive closure result from sync netns worker")?
    }
}

// ─────────────────────────────────────────────
// Global convenience functions
// ─────────────────────────────────────────────

/// Enqueue an async task in `ns` on the global namespace manager.
/// Returns a `TaskHandle` that resolves to the task's output.
#[allow(dead_code)]
pub fn spawn_task_in_netns<F, Fut, T>(ns: &str, f: F) -> TaskHandle<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    global_netns_manager().spawn_task_in(ns, f)
}

/// Run a synchronous closure in `ns` using the global namespace worker manager.
///
/// Only for fast, non-blocking work. Never use for TCP/UDP I/O.
pub fn run_closure_in_netns<F, R>(ns: &str, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    global_netns_manager().run_closure_in(ns, f)
}

/// Spawn a host thread that runs a closure inside `ns`.
pub fn spawn_closure_in_netns<F, R>(ns: String, f: F) -> thread::JoinHandle<Result<R>>
where
    F: FnOnce() -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    thread::spawn(move || {
        let fd = open_netns_fd(&ns)?;
        setns(&fd, CloneFlags::CLONE_NEWNET)
            .with_context(|| format!("setns for spawned thread in '{ns}'"))?;
        f()
    })
}

/// Run a command synchronously inside `ns`.
pub fn run_command_in_netns(ns: &str, mut cmd: Command) -> Result<ExitStatus> {
    debug!(ns = %ns, cmd = ?cmd, "netns: run command");
    run_closure_in_netns(ns, move || cmd.status().context("run command in netns"))
}

/// Spawn a command process inside `ns`.
pub fn spawn_command_in_netns(ns: &str, mut cmd: Command) -> Result<Child> {
    debug!(ns = %ns, cmd = ?cmd, "netns: spawn command");
    run_closure_in_netns(ns, move || cmd.spawn().context("spawn command in netns"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_prefix_cleanup() {
        let reg = FdRegistry::default();
        let fd = File::open("/proc/self/ns/net").unwrap();
        reg.insert("lab-a", fd.try_clone().unwrap());
        reg.insert("lab-b", fd.try_clone().unwrap());
        reg.insert("other", fd);

        reg.remove_prefix("lab-");

        assert!(reg.get("lab-a").is_none());
        assert!(reg.get("lab-b").is_none());
        assert!(reg.get("other").is_some());
    }
}
