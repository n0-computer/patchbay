# Testing with patchbay

This chapter shows how to write integration tests that use patchbay,
run them on Linux and macOS, and inspect the results in the browser.

## Project setup

Add patchbay as a dev dependency alongside tokio and anyhow. If you want
test output directories that persist across runs, add `testdir` too:

```toml
[dev-dependencies]
patchbay = "0.1"
tokio = { version = "1", features = ["rt", "macros", "net", "io-util", "time"] }
anyhow = "1"
ctor = "0.2"
testdir = "0.9"
```

## Writing a test

Create a test file (for example `tests/netsim.rs`) with the namespace
init, a topology, and assertions:

```rust
use std::net::{IpAddr, SocketAddr};
use anyhow::{Context, Result};
use patchbay::{Lab, LabOpts, Nat, OutDir};
use testdir::testdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Runs once before any test thread, entering the user namespace.
#[ctor::ctor]
fn init() {
    patchbay::init_userns().expect("user namespace");
}

#[tokio::test(flavor = "current_thread")]
async fn tcp_through_nat() -> Result<()> {
    // Write topology events and logs into a testdir for later inspection.
    let outdir = testdir!();
    let lab = Lab::with_opts(
        LabOpts::default()
            .outdir(OutDir::Exact(outdir))
            .label("tcp-nat"),
    )
    .await?;

    // Datacenter router (public IPs) and home router (NAT).
    let dc = lab.add_router("dc").build().await?;
    let home = lab
        .add_router("home")
        .nat(Nat::Home)
        .build()
        .await?;

    // Server in the datacenter, client behind NAT.
    let server = lab
        .add_device("server")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;
    let client = lab
        .add_device("client")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    // Start a TCP echo server.
    let server_ip = server.ip().context("no server ip")?;
    let addr = SocketAddr::new(IpAddr::V4(server_ip), 9000);
    server.spawn(move |_| async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 64];
        let n = stream.read(&mut buf).await?;
        stream.write_all(&buf[..n]).await?;
        anyhow::Ok(())
    })?;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Send "hello" from the client, expect it echoed back.
    let echoed = client.spawn(move |_| async move {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        stream.write_all(b"hello").await?;
        let mut buf = vec![0u8; 64];
        let n = stream.read(&mut buf).await?;
        anyhow::Ok(buf[..n].to_vec())
    })?.await??;

    assert_eq!(echoed, b"hello");
    Ok(())
}
```

Key points:

- **`#[ctor::ctor]`** calls `init_userns()` once before any threads
  start. Without this, namespace creation will fail.
- **`#[tokio::test(flavor = "current_thread")]`** is required. patchbay
  namespaces use single-threaded tokio runtimes internally.
- **`testdir!()`** creates a numbered directory next to the test binary
  (e.g. `target/testdir-current/tcp_through_nat/`). Previous runs are
  kept automatically.
- **`OutDir::Exact(path)`** tells the lab to write events and logs into
  that directory. After the test, you can browse them in the devtools UI.

## Running on Linux

On Linux, tests run natively. Install patchbay's CLI if you want the
`serve` command for viewing results:

```bash
cargo install --git https://github.com/n0-computer/patchbay patchbay-runner
```

Then run your tests and serve the output:

```bash
# Run the test.
cargo test tcp_through_nat

# Serve the testdir output in the browser.
patchbay serve --testdir --open
```

The `--testdir` flag automatically locates `<target-dir>/testdir-current`
using `cargo metadata`, so you don't need to pass a path.

## Running on macOS

macOS lacks Linux network namespaces, so tests must run inside a QEMU
VM. Install `patchbay-vm`:

```bash
cargo install --git https://github.com/n0-computer/patchbay patchbay-vm
```

You also need QEMU installed (`brew install qemu` on macOS). On first
run, `patchbay-vm` downloads a Debian cloud image and boots a VM with
all required tools pre-installed.

Run your tests:

```bash
# Run all tests in a package.
patchbay-vm test -p myproject

# Run a specific test file and filter by name.
patchbay-vm test -p myproject --test netsim tcp_through_nat

# Pass environment variables through (RUST_LOG, RUST_BACKTRACE, etc).
RUST_LOG=debug patchbay-vm test -p myproject tcp_through_nat
```

The test binary is cross-compiled for `x86_64-unknown-linux-musl`,
staged into the VM, and executed there. Output written to `testdir` ends
up in `.patchbay-work/binaries/tests/` which is shared back to the host.

Serve the results:

```bash
patchbay-vm serve --testdir --open
```

The VM stays running between commands, so subsequent runs skip the boot
step. Use `patchbay-vm down` to stop it, or `--recreate` to start fresh.

## Viewing results

Both `patchbay serve` and `patchbay-vm serve` open the devtools UI with:

- **Topology** — a graph of routers and devices in the lab.
- **Logs** — per-namespace tracing output and structured event files.
- **Timeline** — custom events plotted across nodes over time.

To emit custom events that show up on the timeline, use the `_events::`
tracing target convention:

```rust
tracing::info!(target: "myapp::_events::ConnectionEstablished", peer = %addr);
```

### Reading logs from the terminal

The `fmt-log` command re-renders `.tracing.jsonl` files as human-readable
ANSI output, matching the familiar `tracing_subscriber` console format:

```bash
# Print a log file.
patchbay fmt-log target/testdir-current/tcp_through_nat/device.client.tracing.jsonl

# Pipe from stdin.
cat device.client.tracing.jsonl | patchbay fmt-log

# Follow a file in real time (like tail -f).
patchbay fmt-log -f device.client.tracing.jsonl
```

## Controlling log output

Per-namespace tracing logs are written to `{kind}.{name}.tracing.jsonl`
files in the output directory. The filter is read from `PATCHBAY_LOG`,
falling back to `RUST_LOG`, falling back to `info`. Full directive
syntax is supported:

```bash
# Only capture trace-level output from your crate's networking code.
PATCHBAY_LOG=myapp::net=trace cargo test tcp_through_nat
```

**Limitation:** the file filter can only capture events at levels the
global subscriber (console output) already enables. tracing-core caches
callsite interest globally, so if the global subscriber rejects TRACE,
those callsites are permanently disabled — including for the file
writer. To get TRACE in file output, ensure the global subscriber also
enables TRACE (e.g. `RUST_LOG=trace`).

## Common flags

`patchbay-vm test` supports the same flags as `cargo test`:

| Flag | Short | Description |
|------|-------|-------------|
| `--package <name>` | `-p` | Test a specific package |
| `--test <name>` | | Select a test target (binary) |
| `--jobs <n>` | `-j` | Parallel compilation jobs |
| `--features <f>` | `-F` | Activate cargo features |
| `--release` | | Build in release mode |
| `--lib` | | Test only the library |
| `--no-fail-fast` | | Run all tests even if some fail |
| `--recreate` | | Stop and recreate the VM |
| `-- <args>` | | Extra args passed to cargo |
