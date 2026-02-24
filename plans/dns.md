# DNS & Name Resolution Plan

**Status:** ❌ not implemented

## TODO

- [x] Write plan
- [ ] Phase 1: bind-mount infra (`NsFileOverrides` registry, `pre_exec` hook)
- [ ] Phase 1: `/etc/hosts` per device (`add_host_entry`, `add_host_entry_all`, `auto_populate_hosts`)
- [ ] Phase 1: `/etc/resolv.conf` per device (`set_nameserver`, `set_nameserver_all`)
- [ ] Phase 1: TOML `[dns]` / `[[device]] dns_name` / `[[dns.host]]` (phase 1 fields)
- [ ] Phase 1: tests (hosts visible, isolation, nameserver written, auto-populate)
- [ ] Phase 2: `hickory-server` dep + `DnsServer` struct + `InMemoryAuthority` zone
- [ ] Phase 2: `ServerFuture` binding in root namespace worker
- [ ] Phase 2: `Lab::spawn_dns_server`, `auto_populate_dns`, `start_dns`
- [ ] Phase 2: TOML `[dns] enabled` / `dns_txt` device field
- [ ] Phase 2: tests (A/AAAA/TXT roundtrip, runtime mutation, resolver method, isolation)
- [ ] Phase 3: pebble binary acquisition + config + process lifecycle (`PebbleHandle`)
- [ ] Phase 3: cert trust injection (env vars + optional `inject_system_ca`)
- [ ] Phase 3: DNS-01 challenge responder task
- [ ] Phase 3: `Lab::start_acme`, `run_with_acme`, TOML `[acme]`
- [ ] Phase 3: tests (cert issued HTTP-01, cert trusted, DNS-01 challenge, env injection)
- [ ] Final review

---

## Overview

Three self-contained phases, each building on the previous:

| Phase | Feature | Effort |
|-------|---------|--------|
| 1 | Custom `/etc/hosts` + `/etc/resolv.conf` per device via bind-mount | ~2 d |
| 2 | In-lab authoritative DNS server (hickory-server) | ~3.5 d |
| 3 | In-lab ACME CA (pebble) + cert trust injection | ~3 d |

---

## Bind-Mount Infrastructure (shared by all phases)

### Why `/etc/netns/NSNAME/resolv.conf` doesn't apply here

`ip-netns(8)` bind-mounts `/etc/netns/NSNAME/{hosts,resolv.conf}` automatically when
entering a **named** namespace (stored in `/var/run/netns/`). Netsim uses **fd-based
anonymous namespaces** (`unshare(CLONE_NEWNET)` + fd registry). No `/var/run/netns/`
entry exists, so `ip netns exec` is never used and the bind-mount never happens.

All processes spawned via `spawn_command_in_netns` share the **host mount namespace**
and therefore the host's `/etc/hosts` and `/etc/resolv.conf`.

### Approach: `unshare(CLONE_NEWNS)` + `MS_BIND` in `pre_exec`

**Mount namespaces are per-process on Linux, not per-thread.** Calling
`unshare(CLONE_NEWNS)` in a worker thread would affect the entire process, invalidating
isolation for all other workers. This means the bind-mount cannot be done at worker
thread setup time — it must happen in the **forked child**, between `fork` and `exec`.

The correct place is a `pre_exec` hook (available via
`std::os::unix::process::CommandExt::pre_exec`):

```rust
// Inside spawn_command_in_netns, after setns:
if let Some(ovr) = NS_FILE_REGISTRY.get(ns) {
    unsafe {
        cmd.pre_exec(move || {
            // New private mount ns for this child only.
            if libc::unshare(libc::CLONE_NEWNS) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            for (src, dst) in &ovr.bind_mounts {
                let src = CString::new(src.as_os_str().as_bytes())?;
                let dst = CString::new(dst.as_bytes())?;
                if libc::mount(
                    src.as_ptr(), dst.as_ptr(),
                    std::ptr::null(), libc::MS_BIND | libc::MS_RDONLY,
                    std::ptr::null(),
                ) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}
```

Each namespace entry holds a list of `(src_path, dst_path)` pairs.
`/etc/hosts` and `/etc/resolv.conf` are two entries in that list; phase 3 adds a
third for the system CA bundle.

**For in-process Rust code** (e.g., `hickory-resolver`, `reqwest`) running in a
namespace worker closure: bind-mounting `/etc/resolv.conf` does not help since the
thread shares the process mount namespace. Instead expose
`DnsServer::resolver() -> AsyncResolver` (phase 2) which constructs a
`hickory-resolver` pointed directly at the server's IP, bypassing `/etc/resolv.conf`.

**`mount` in `pre_exec` and capabilities**: in the rootless/userns setup the lab runs
in a user namespace where `mount --bind` in a new private mount namespace (created by
the same `unshare`) is allowed without `CAP_SYS_ADMIN`. Verified behaviour of
`unshare -m` in rootless containers. Add an explicit test for this.

### Central registry

```rust
// In core.rs (or a new ns_files.rs)

struct NsFileOverrides {
    // Ordered list of (src_file, dst_path_in_ns) pairs applied at spawn time.
    bind_mounts: Vec<(PathBuf, &'static str)>,
}

// Global: Mutex<HashMap<ns_name, NsFileOverrides>>
static NS_FILE_REGISTRY: ...
```

Helper: `fn register_ns_file(ns: &str, src: PathBuf, dst: &'static str)` appends to
the list. Calling it twice for the same `dst` replaces the entry (last writer wins).

---

## Phase 1 — Custom `/etc/hosts` and `/etc/resolv.conf`

**Goal**: let individual devices and the whole lab have custom host→IP mappings and
a custom nameserver, without needing a DNS server.

### `/etc/hosts`

A per-device hosts file is assembled as:

```
# auto-generated by netsim
127.0.0.1  localhost
::1        localhost

# lab entries
10.0.1.5   relay.sim.test relay
203.0.113.10  provider.sim.test provider
```

The file is written to `.netsim-work/{run}/hosts/{ns}.hosts` at build time (or when
mutated). On first `set_dns` / `add_host_entry` for a device, the file is initialised
with a minimal localhost block.

### Lab API

```rust
// Add a single A (and optionally AAAA) entry to one device's hosts file.
// `name` may be a bare label ("relay") or FQDN ("relay.sim.test.").
// Appends; calling twice with the same name adds a second address line
// (valid for multi-address hosts).
pub fn add_host_entry(
    &mut self,
    device: &str,
    name: &str,
    ip: IpAddr,
) -> Result<()>

// Add a hosts entry to every device in the lab.
pub fn add_host_entry_all(
    &mut self,
    name: &str,
    ip: IpAddr,
) -> Result<()>

// Set the nameserver for a device (writes /etc/resolv.conf).
// `server` is any IP — can be an external address or the in-lab server (phase 2).
// Overwrites any previous nameserver for that device.
pub fn set_nameserver(&mut self, device: &str, server: IpAddr) -> Result<()>

// Set the nameserver for every device.
pub fn set_nameserver_all(&mut self, server: IpAddr) -> Result<()>
```

Both calls write/update the temp file and call `register_ns_file`.

### Auto-populate hosts from topology

```rust
// Adds one hosts entry per device interface using device name as hostname.
// Called after Lab::build() if auto_hosts = true in [dns] TOML section.
pub fn auto_populate_hosts(&mut self) -> Result<()>
```

### TOML config

```toml
[dns]
# Automatically add all device IPs to all devices' /etc/hosts (default false).
auto_hosts = true
# Set this nameserver for all devices (phase 1; overridden by in-lab server in phase 2).
nameserver = "1.1.1.1"

# Per-device DNS name and optional records, declared inline with the device.
# The device's primary interface IP (default_via interface, or eth0) is used.
[[device]]
name = "provider"
dns_name = "provider.sim.test."   # registers hosts entry (phase 1) / A record (phase 2)
dns_txt  = ["role=provider"]      # TXT records — phase 2 only; silently ignored in phase 1

[[device]]
name = "relay"
dns_name = "relay"                # short form: resolved against zone in phase 2;
                                  # written as-is to /etc/hosts in phase 1

# Extra static hosts entries not tied to any device.
[[dns.host]]
name = "stun.sim.test."
ip   = "203.0.113.10"

[[dns.host]]
name = "bootstrap.sim.test."
ip   = "203.0.113.11"
ip6  = "2001:db8::b"             # optional AAAA alongside A
```

`dns_name` on a device serves double duty: phase 1 writes it to `/etc/hosts` for all
devices, phase 2 registers it as an A/AAAA record in the DNS zone. The `dns_txt` field
is silently ignored in phase 1 (no DNS server yet).

### Implementation steps

| # | Task |
|---|------|
| 1 | `NsFileOverrides` registry + `register_ns_file` helper in `core.rs` |
| 2 | `pre_exec` hook injection in `spawn_command_in_netns` / `run_command_in_netns` |
| 3 | `write_hosts_file(ns, entries) -> PathBuf` — writes temp file, calls `register_ns_file` |
| 4 | `write_resolv_conf(ns, server_ip) -> PathBuf` — same for resolv.conf |
| 5 | `Lab::add_host_entry`, `add_host_entry_all`, `set_nameserver`, `set_nameserver_all` |
| 6 | `Lab::auto_populate_hosts` + TOML `auto_hosts` flag |
| 7 | TOML `[[device]] dns_name`, `[[dns.host]]`, `nameserver` fields |
| 8 | Tests (see below) |

### Tests

```
hosts_entry_visible_in_ns       — add_host_entry; getaddrinfo/ping in ns resolves name
hosts_entry_all_devices         — add_host_entry_all; check from two different devices
hosts_isolation                 — device without add_host_entry cannot resolve the name
nameserver_written              — set_nameserver writes correct resolv.conf; cat confirms
auto_populate_hosts_roundtrip   — build topo; auto_populate_hosts; each device resolves peers
toml_dns_name                   — dns_name in TOML causes correct hosts entry
```

Tests run `run_command_in_netns` with a small static binary or inline Rust via
`run_closure_in_netns` — no `getaddrinfo` in closure (shares host mount ns), so use
a spawned `nslookup`/`grep /etc/hosts` subprocess.

**Effort: ~2 dev-days.**

---

## Phase 2 — In-Lab Authoritative DNS Server

**Goal**: run a real DNS server inside the lab so devices can resolve names that don't
exist in public DNS, with dynamic record management.

Builds on phase 1: `set_nameserver_all(&dns_server_ip)` replaces the static external
nameserver with the in-lab server address.

### Crate

```toml
hickory-server = { version = "0.25", default-features = false }
```

`default-features = false` drops DNSSEC, DoT/DoH, file-based zones. Retains
`InMemoryAuthority` + `ServerFuture` + UDP/TCP transport.

### Server placement

The server runs in the **lab root namespace**, bound to the IX gateway IP:

```
root_ns  203.0.113.1:53  ← hickory ServerFuture (UDP + TCP)
          │
          IX bridge
          │
       routers → devices
```

Every device namespace reaches `203.0.113.1` via its default route
(device → router (ip_forward=1) → IX bridge → root_ns). No special per-device routing.

For dual-stack the server also binds to `2001:db8::1:53`.
Multiple concurrent labs have isolated root namespaces; no port conflict.

### `InMemoryAuthority` sketch

```rust
let zone_name: Name = "sim.test.".parse()?;
let mut zone = InMemoryAuthority::empty(zone_name.into(), ZoneType::Primary, false);

// Records with explicit TTL
zone.upsert(Record::from_rdata(
    "provider.sim.test.".parse()?,
    60,   // TTL — short default so tests don't cache stale state
    RData::A(rdata::A(ip)),
), 0);

let mut catalog = Catalog::new();
catalog.upsert(LowerName::new(&zone_name), Box::new(Arc::clone(&authority)));

let mut server = ServerFuture::new(catalog);
server.register_socket(UdpSocket::bind("203.0.113.1:53").await?);
server.register_listener(TcpListener::bind("203.0.113.1:53").await?,
                         Duration::from_secs(5));
tokio::spawn(server.block_until_done());
```

`authority` is `Arc<Mutex<InMemoryAuthority>>` shared between the server and the
`DnsServer` handle for runtime mutations.

### `DnsServer` type

```rust
pub struct DnsServer {
    zone: Name,
    authority: Arc<Mutex<InMemoryAuthority>>,
    server_ip: IpAddr,       // 203.0.113.1 (or v6 equivalent)
    server_ip_v6: Option<IpAddr>,
}

impl DnsServer {
    // Relative names ("relay") are expanded to "relay.<zone>."; FQDNs (ending ".") used as-is.

    // Replaces all A records for the name (upserts the full RRSet).
    pub fn set_a(&self, name: &str, ips: &[Ipv4Addr]) -> Result<()>;
    // Adds one A record, appending to any existing RRSet.
    pub fn add_a(&self, name: &str, ip: Ipv4Addr) -> Result<()>;
    pub fn set_aaaa(&self, name: &str, ips: &[Ipv6Addr]) -> Result<()>;
    pub fn add_aaaa(&self, name: &str, ip: Ipv6Addr) -> Result<()>;
    // Adds one TXT string; multiple calls accumulate strings in the same RRSet.
    pub fn add_txt(&self, name: &str, text: &str) -> Result<()>;
    // Removes all records of the given type for a name.
    pub fn remove(&self, name: &str, rtype: RecordType) -> Result<()>;
    // Lists all records for a name (for test assertions / debugging).
    pub fn records(&self, name: &str) -> Result<Vec<Record>>;
    // Returns a pre-configured hickory AsyncResolver pointing at this server.
    // Use this for in-process DNS queries from Rust (bypasses /etc/resolv.conf).
    pub fn resolver(&self) -> Result<AsyncResolver>;
}
```

**API shortcomings noted and resolved:**

| Issue | Resolution |
|-------|-----------|
| `add_a` replaces vs appends ambiguity | Split into `add_a` (append) and `set_a` (replace RRSet) |
| No TTL control | Default 60 s (short); `set_a_ttl(&self, name, ips, ttl)` for tests that need it |
| No listing/inspection | `records(name)` returns current RRSet for assertions |
| In-process Rust DNS (hickory-resolver) ignores bind-mount | `resolver()` constructs `AsyncResolver` with explicit server IP |
| `spawn_dns_server` must follow `build()` | Enforced by `&mut self` borrow on `Lab`; document clearly |
| `add_a` twice for same name adds duplicate vs updates | `add_a` appends to RRSet; `set_a` replaces; both documented |
| Relative vs absolute name convention undocumented | Documented on each method: trailing `.` = absolute, otherwise zone-relative |
| `auto_populate_dns` conflicts with explicit `dns_name` | `auto_populate_dns` skips devices that already have an explicit `dns_name` |
| Multiple zones | Each `spawn_dns_server` call creates one zone; call multiple times for multiple zones — same server can only host one zone per call in this simple API |
| No CNAME, SRV, wildcard | Out of scope; use a real DNS daemon |

### Lab API

```rust
// Spawns authoritative DNS server in root namespace.
// zone: DNS zone name, defaults to "sim.test." if None.
// After return, call set_nameserver_all(&dns.server_ip) to point devices at it.
pub async fn spawn_dns_server(&mut self, zone: Option<&str>) -> Result<DnsServer>

// Auto-register A/AAAA records for all devices from topology.
// Uses device.dns_name if set, otherwise device name as label within zone.
// Skips devices that already have records registered for their name.
pub fn auto_populate_dns(&mut self, server: &DnsServer) -> Result<()>

// Convenience: spawn server + auto_populate + set_nameserver_all in one call.
pub async fn start_dns(&mut self, zone: Option<&str>) -> Result<DnsServer>
```

### TOML — phase 2 additions

```toml
[dns]
zone    = "sim.test."
# Shorthand for spawn_dns_server + auto_populate_dns + set_nameserver_all.
enabled = true
# Also registers hosts entries (phase 1) for same names, so /etc/hosts works
# as fallback even without DNS queries. Default true.
also_hosts = true

[[device]]
name     = "provider"
dns_name = "provider.sim.test."
dns_txt  = ["role=provider", "build=abc123"]
```

### Tests (phase 2)

```
dns_a_record_resolves           — spawn server, set_a; query from device ns returns correct IP
dns_aaaa_record_resolves        — same for AAAA (dual-stack topo)
dns_txt_record_roundtrip        — add_txt; query TXT returns strings
dns_auto_populate               — start_dns; each device resolves peer by name
dns_runtime_add                 — add_a after build; device query reflects new record
dns_runtime_remove              — remove; subsequent query returns NXDOMAIN
dns_records_inspection          — records("relay") returns correct list after mutations
dns_multi_zone                  — two zones on separate DnsServer handles, no cross-hit
dns_resolver_method             — dns.resolver() returns AsyncResolver; in-process query works
dns_ns_isolation                — device without set_nameserver still queries host resolver
```

**Effort: ~3.5 dev-days** (1 d hickory wiring, 0.5 d Lab API, 0.5 d TOML, 1.5 d tests).

---

## Phase 3 — In-Lab ACME CA (pebble) + Cert Trust Injection

**Goal**: allow binaries that provision TLS certificates via ACME (e.g., iroh relay,
any QUIC server using rcgen+ACME) to work inside the sim without real internet access
or manual certificate management.

Builds on phases 1 and 2:
- Phase 2 DNS server serves `_acme-challenge` TXT records for DNS-01.
- Phase 1 bind-mount infrastructure is extended to inject CA certs.

### Pebble overview

[pebble](https://github.com/letsencrypt/pebble) is a minimal ACME server for testing.
Key properties:
- Single static binary (~10 MB), no dependencies.
- Issues short-lived test certificates signed by an ephemeral root CA.
- Intentionally rejects production Let's Encrypt behaviour (no rate limits, randomised
  validation delays configurable to 0).
- ACME directory at `https://<host>:14000/dir`.
- Management API at `http://<host>:15000` (challenge status, add TXT records).
- Certificate chain endpoint: `https://<host>:15000/roots/0` (DER) and
  `https://<host>:15000/intermediates/0`.

### Binary acquisition

Pebble is downloaded from GitHub Releases using the existing binary-fetch
infrastructure (`sim/build.rs`), same as iroh binaries. Add a `pebble` target to
`iroh-defaults.toml` or allow a `[[sim.binaries]]` entry:

```toml
[[sim.binaries]]
name    = "pebble"
fetch   = "https://github.com/letsencrypt/pebble/releases/latest/download/pebble_linux-amd64"
# or: path = "/usr/local/bin/pebble"
```

Alternatively, `Lab::start_acme()` checks `$PATH` first, then falls back to
downloading. Cache in the same binary cache as other fetched binaries.

### Pebble config

Pebble needs a minimal JSON config:

```json
{
  "pebble": {
    "listenAddress": "203.0.113.1:14000",
    "managementListenAddress": "203.0.113.1:15000",
    "certificate": "",
    "privateKey": "",
    "httpPort": 80,
    "tlsPort": 443,
    "ocspResponderURL": "",
    "externalAccountBindingRequired": false
  }
}
```

Written to `.netsim-work/{run}/pebble/config.json`. Pebble generates its own ephemeral
root CA on each start (no config needed for the CA itself).

### Pebble process lifecycle

Pebble runs inside the **root namespace** (reachable from all devices via IX bridge),
spawned with `spawn_command_in_netns(&root_ns, ...)`. The `PebbleHandle` wraps the
`Child` and kills it on drop.

Startup sequence in `Lab::start_acme()`:

```
1. Write pebble config.json
2. spawn pebble in root_ns
3. Poll GET https://203.0.113.1:15000/roots/0 until pebble is ready (up to 5 s)
   — pebble's management port is HTTP (no TLS), so no cert chicken-and-egg
4. Fetch root CA cert (DER) from /roots/0, also intermediate from /intermediates/0
5. Write PEM bundle to .netsim-work/{run}/pebble/ca.pem
6. Store PebbleHandle { child, ca_pem_path, acme_directory_url }
```

Pebble's HTTPS listener uses a self-signed cert for its own ACME endpoint. Clients
need to either skip TLS verification (pebble supports `PEBBLE_WFE_NONCEREUSE=0`) or
be given the pebble root cert. Since we inject the cert into devices anyway, clients
can be given it as a trusted CA.

### Cert trust injection

Linux has no single system CA bundle path — it varies by distro. The practical
approach for test binaries: **environment variable injection** is more portable than
bind-mounting the system bundle.

```rust
pub struct PebbleHandle {
    child: Child,
    pub ca_pem_path: PathBuf,        // path to pebble root CA PEM
    pub acme_directory: String,      // "https://203.0.113.1:14000/dir"
    // Env vars to inject into processes that need to trust pebble:
    pub env_vars: Vec<(String, String)>,
}
```

`env_vars` contains:
- `SSL_CERT_FILE=<ca_pem_path>` — OpenSSL, Go, Python (on most systems)
- `REQUESTS_CA_BUNDLE=<ca_pem_path>` — Python requests
- `NODE_EXTRA_CA_CERTS=<ca_pem_path>` — Node.js
- `CURL_CA_BUNDLE=<ca_pem_path>` — curl
- For Rust/rustls: `RUSTLS_CA_FILE` is not standard; pass via `--ca-cert` flag or
  build-time configuration. Expose `PebbleHandle::ca_pem_path` so callers can handle
  rustls cert injection themselves.

`Lab::run_with_acme(device, cmd)` wraps `run_on` and injects `env_vars` automatically.

For bind-mount injection (alternative, distro-specific): the phase 1 bind-mount
infrastructure can mount `ca.pem` over `/etc/ssl/certs/ca-certificates.crt`
(Debian) or the appropriate path. Since the target path is distro-dependent, this is
opt-in via `PebbleHandle::inject_system_ca(&mut lab, device)`:

```rust
// Appends pebble root CA to a copy of the device's current system CA bundle
// and bind-mounts the result over the system bundle path.
// More reliable than SSL_CERT_FILE for processes that hardcode the system path.
pub fn inject_system_ca(&self, lab: &mut Lab, device: &str) -> Result<()>
pub fn inject_system_ca_all(&self, lab: &mut Lab) -> Result<()>
```

### DNS-01 challenge flow

When using DNS-01 validation (needed for wildcard certs or when HTTP-01 port 80 is
not reachable):

1. Binary requests cert from pebble.
2. Pebble sends DNS-01 challenge: set `_acme-challenge.<domain>` TXT to `<token>`.
3. A challenge-hook task in the lab polls pebble's management API for pending
   challenges (`GET http://203.0.113.1:15000/challengers`) and calls
   `dns.add_txt("_acme-challenge.domain.", token)`.
4. Pebble validates, issues cert.

```rust
// Spawns a background task that polls pebble management API and satisfies
// DNS-01 challenges automatically via the given DnsServer.
pub async fn start_dns_challenge_responder(
    &self,              // PebbleHandle
    dns: &DnsServer,
) -> Result<JoinHandle<()>>
```

### HTTP-01 challenge flow (simpler)

For HTTP-01 (port 80), pebble reaches into the device namespace directly — the
binary under test serves the challenge token at `http://device-ip/.well-known/acme-challenge/<token>`.
No extra infrastructure needed; works as long as pebble (in root_ns) can reach the
device namespace via the IX bridge routing. This is the default for most ACME clients.

### Lab API (phase 3)

```rust
// Downloads (or locates) pebble, starts it in root_ns, fetches root CA.
// Returns handle. Requires phase 2 DNS server if DNS-01 is needed.
pub async fn start_acme(&mut self) -> Result<PebbleHandle>

// Set pebble's validation delay to 0 (default is randomised 0–3 s).
// Must be called before start_acme.
pub fn set_acme_validation_delay(&mut self, secs: u64)

// Run a command on a device with pebble env vars injected.
pub fn run_with_acme(&self, pebble: &PebbleHandle, device: &str, cmd: Command)
    -> Result<ExitStatus>
```

### TOML (phase 3)

```toml
[acme]
enabled = true
# validation_delay_secs = 0  # default 0 for tests
# inject_system_ca = true    # bind-mount CA into system bundle (distro-specific)
```

### Tests

```
acme_cert_issued_http01     — start_acme; run a simple ACME client in device ns;
                               client obtains cert signed by pebble root
acme_cert_trusted           — cert chain validates against injected CA (openssl verify)
acme_dns01_challenge        — start_dns + start_acme + start_dns_challenge_responder;
                               ACME client uses DNS-01; cert issued
acme_env_injection          — run_with_acme injects SSL_CERT_FILE; curl to pebble HTTPS works
acme_system_ca_injection    — inject_system_ca; process that ignores SSL_CERT_FILE still trusts
```

**Effort: ~3 dev-days** (0.5 d pebble lifecycle, 0.5 d cert injection, 0.5 d challenge
responder, 1.5 d tests).

---

## Total Effort

| Phase | Feature | Estimate |
|-------|---------|----------|
| 1 | Bind-mount infra + `/etc/hosts` + `/etc/resolv.conf` per device | ~2 d |
| 2 | hickory-server in-lab DNS | ~3.5 d |
| 3 | pebble ACME + cert trust injection | ~3 d |
| **Total** | | **~8.5 d** |

---

## What Could Go Wrong

- **`mount --bind` in user namespace**: verify that `unshare(CLONE_NEWNS)` followed by
  `mount --bind` works without capabilities in the rootless setup. The kernel allows
  this when the bind-mount stays within the user's own mount namespace (created by the
  same `unshare`). One existing integration test should probe this before any of the
  above is built.
- **hickory-server API churn**: pinned to `0.25`; `InMemoryAuthority` API has been
  relatively stable since `0.23`. Track release notes on upgrade.
- **pebble random validation delay**: must set `PEBBLE_VA_NOSLEEP=1` or pebble config
  `httpPort`/`tlsPort` delays to 0; default behaviour adds up to 3 s per challenge,
  making tests slow. Expose `set_acme_validation_delay(0)`.
- **System CA bundle path for `inject_system_ca`**: Debian uses
  `/etc/ssl/certs/ca-certificates.crt`, RHEL uses `/etc/pki/tls/certs/ca-bundle.crt`,
  Alpine uses `/etc/ssl/cert.pem`. Detect at runtime via a priority list; fall back to
  env var injection if none found.
- **DNS-01 polling race**: the challenge-responder polls pebble's management API; if
  pebble validates before the poller fires there will be an error. Use a short poll
  interval (100 ms) and retry on challenge-not-found.
