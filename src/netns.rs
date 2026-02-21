//! Network namespace lifecycle helpers using an in-memory FD registry.

use anyhow::{anyhow, bail, Context, Result};
use futures::executor;
use nix::sched::{setns, unshare, CloneFlags};
use nix::unistd::gettid;
use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::pin::Pin;
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use tokio::sync::oneshot;
use tracing::debug;

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
    debug!(ns = %name, "netns: open namespace fd");
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
        bail!(
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

type BoxFutureUnit = Pin<Box<dyn Future<Output = Result<()>> + 'static>>;
type WorkerTask = Box<dyn FnOnce() -> BoxFutureUnit + Send + 'static>;

enum WorkerMsg {
    Run {
        task: WorkerTask,
        done: oneshot::Sender<Result<()>>,
    },
    Shutdown,
}

struct Worker {
    tx: mpsc::Sender<WorkerMsg>,
    join: Option<thread::JoinHandle<()>>,
}

impl Worker {
    fn spawn(ns: &str) -> Result<Self> {
        let ns_name = ns.to_string();
        let target = open_netns_fd(&ns_name)
            .with_context(|| format!("open namespace fd for worker '{ns_name}'"))?;
        let (tx, rx) = mpsc::channel::<WorkerMsg>();
        let join = thread::spawn(move || worker_main(ns_name, target, rx));
        Ok(Self {
            tx,
            join: Some(join),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerMsg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn worker_main(ns: String, target: File, rx: mpsc::Receiver<WorkerMsg>) {
    if let Err(err) = setns(&target, CloneFlags::CLONE_NEWNET) {
        debug!(ns = %ns, error = %err, "netns worker: setns failed");
        return;
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            debug!(ns = %ns, error = %err, "netns worker: runtime build failed");
            return;
        }
    };

    while let Ok(msg) = rx.recv() {
        match msg {
            WorkerMsg::Run { task, done } => {
                let local = tokio::task::LocalSet::new();
                let join = local.spawn_local(async move { task().await });
                let res = rt.block_on(local.run_until(async move {
                    match join.await {
                        Ok(v) => v,
                        Err(err) => {
                            if err.is_panic() {
                                Err(anyhow!("netns worker task panicked"))
                            } else {
                                Err(anyhow!("netns worker task cancelled"))
                            }
                        }
                    }
                }));
                let _ = done.send(res);
            }
            WorkerMsg::Shutdown => break,
        }
    }
}

/// Executes async tasks inside dedicated per-namespace worker threads.
///
/// Each namespace gets one long-lived thread with:
/// - `setns(2)` called once to enter that namespace.
/// - a single-threaded Tokio runtime used to execute submitted async closures.
#[derive(Default)]
pub struct NetnsManager {
    workers: Mutex<HashMap<String, Worker>>,
}

impl NetnsManager {
    /// Create an empty namespace manager.
    pub fn new() -> Self {
        Self::default()
    }

    fn worker_sender(&self, ns: &str) -> Result<mpsc::Sender<WorkerMsg>> {
        let mut workers = self.workers.lock().expect("netns worker map poisoned");
        if !workers.contains_key(ns) {
            workers.insert(ns.to_string(), Worker::spawn(ns)?);
        }
        workers
            .get(ns)
            .map(|w| w.tx.clone())
            .ok_or_else(|| anyhow!("missing netns worker for '{ns}'"))
    }

    /// Run one async task in the worker assigned to `ns`.
    ///
    /// Panics inside the async task are forwarded as an error.
    pub async fn run_in<F, Fut>(&self, ns: &str, task: F) -> Result<()>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + 'static,
    {
        let tx = self.worker_sender(ns)?;
        let (done_tx, done_rx) = oneshot::channel();
        let msg = WorkerMsg::Run {
            task: Box::new(move || Box::pin(task())),
            done: done_tx,
        };
        tx.send(msg)
            .map_err(|_| anyhow!("send task to netns worker '{ns}' failed"))?;
        done_rx
            .await
            .with_context(|| format!("receive result from netns worker '{ns}'"))?
    }
}

/// Run a synchronous closure in `ns` using the global namespace worker manager.
pub fn run_closure_in_netns<F, R>(ns: &str, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    let ns_name = ns.to_string();
    let (tx, rx) = mpsc::sync_channel::<Result<R>>(1);
    executor::block_on(global_netns_manager().run_in(&ns_name, move || async move {
        let _ = tx.send(f());
        Ok(())
    }))?;
    rx.recv()
        .context("receive closure result from netns worker")?
}

/// Spawn a host thread that runs a closure inside `ns`.
pub fn spawn_closure_in_netns<F, R>(ns: String, f: F) -> thread::JoinHandle<Result<R>>
where
    F: FnOnce() -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    thread::spawn(move || run_closure_in_netns(&ns, f))
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
