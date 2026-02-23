# No-Sudo Netsim Plan

## TODO

- [x] Write plan
- [x] Phase 0: capability/policy diagnostics (`check_caps()` implemented; `netsim doctor` deferred)
- [x] Phase 1: namespace backend abstraction — superseded by fd-only rootless approach (`rootless.md`)
- [x] Phase 2: netns executor refactor (`NetnsManager` worker-thread executor)
- [x] Phase 3: lab-root isolation hardening — implemented; guard tests for host-route leakage deferred
- [x] Phase 4: cleanup guarantees — cleanup/resource tracking exists; full leak-regression suite deferred
- [x] Phase 5: test matrix + docs — deferred; superseded by rootless plan
- [ ] Final review

## Goal
Make `netsim-rs` run and test without `sudo`, with capability-based privilege only, and with strong guarantees against host-root namespace leakage.

## Success Criteria
- `cargo test` works as non-root on supported hosts after one-time capability setup.
- Lab dataplane operations never execute in host root netns by design (not by best effort).
- Failures are explicit and early when host policy makes no-sudo impossible.
- Cleanup never leaves host routes, links, or namespace side effects.

## Research Findings (Primary Sources)
1. File capabilities cannot help if launcher sets `no_new_privs=1`.
- Kernel docs: `execve()` with `no_new_privs` will not grant file capabilities.
- Source: https://docs.kernel.org/userspace-api/no_new_privs.html

2. `setns()` is thread-local and requires `CAP_SYS_ADMIN` for target netns ownership context.
- Source: https://man7.org/linux/man-pages/man2/setns.2.html
- Source: https://man7.org/linux/man-pages/man7/capabilities.7.html

3. `ip netns add` is mount-heavy and may fail under mount policy even with caps.
- `iproute2` performs:
  - `mount --make-shared $NETNS_RUN_DIR`
  - `mount --bind $NETNS_RUN_DIR $NETNS_RUN_DIR`
  - bind-mount of `/proc/self/ns/net` into `/run/netns/<name>`
- Source: https://sources.debian.org/src/iproute2/5.10.0-4/ip/ipnetns.c/
- Man page confirms named netns are `/var/run/netns/NAME` mount objects.
- Source: https://man7.org/linux/man-pages/man8/ip-netns.8.html

4. Unprivileged `CLONE_NEWUSER` can bootstrap non-user namespaces on hosts that allow it.
- Source: https://man7.org/linux/man-pages/man7/user_namespaces.7.html

## Key Implication
There is no single universal no-sudo path across all environments. We need capability + host-policy detection + mode selection.

## Architecture Options

### Option A: Named netns only (`ip netns add`)
Pros:
- Easy introspection, stable handles, good operability.
Cons:
- Depends on mount propagation operations; often blocked in containers/restricted hosts.
- Tightly coupled to `ip` behavior.

### Option B: FD-only netns (unshare + fd registry)
Pros:
- Avoids `/run/netns` mount operations entirely.
- Works where mount operations are blocked but netns syscalls are allowed.
Cons:
- Harder introspection from external tools.
- Requires robust lifecycle and cleanup bookkeeping.

### Option C: Hybrid (Recommended)
- Prefer named netns when available.
- Fall back to FD-only mode when mount setup is denied.
- Same internal execution model for both.

## Recommended Execution Model (Leakage Prevention)

### 1) Eliminate ad-hoc `setns` from async paths
- Replace scattered `with_netns(...)` calls with a namespace executor model:
  - One dedicated OS thread per namespace worker (or short-lived per-op thread).
  - Thread enters target netns once, runs all requested ops from channel.
  - No Tokio task migration risk after entry.

### 2) Dedicated lab-root namespace as mandatory control plane
- IX and transit setup occurs only inside lab-root namespace.
- Host root netns is never a target namespace for lab dataplane operations.
- Remove all host-root fallback code paths.

### 3) Strict capability bootstrap checks (fast fail)
At process start:
- Check `NoNewPrivs` from `/proc/self/status`; fail with actionable message if set.
- Check effective caps include `CAP_SYS_ADMIN`, `CAP_NET_ADMIN`, `CAP_NET_RAW`.
- Probe namespace mode:
  - Try named-netns bootstrap (`ip netns add` tiny probe).
  - If denied with mount-related errors, switch to FD-only mode automatically.

### 4) Cleanup model
- Track all lab resources per lab instance (links/routes/netns objects).
- Cleanup in reverse creation order.
- In hybrid mode, cleanup both name handle and fd registry when both are present.

## Implementation Plan

### Phase 0: Capability + Policy Diagnostics
- Add `EnvDiagnostics` module:
  - `no_new_privs` detector
  - cap detector
  - named-netns probe
  - userns probe (optional)
- Add `netsim doctor` command to print actionable host readiness report.

### Phase 1: Namespace Backend Abstraction
- Introduce trait `NetnsBackend`:
  - `create(name)`
  - `open(name)`
  - `delete(name)`
  - `list_owned(prefix)`
- Implement:
  - `NamedBackend` (`ip netns add`)
  - `FdBackend` (unshare + fd store)
- Add runtime mode selection: `auto | named | fd`.

### Phase 2: Netns Executor Refactor
- Create `NetnsExecutor` API:
  - `exec_in(ns, closure)`
  - `spawn_in(ns, command)`
- Ensure each operation runs on a dedicated thread that performs `setns` before work.
- Remove direct `setns` calls from async/Tokio contexts.

### Phase 3: Lab-root Isolation Hardening
- Guarantee all IX/root operations are bound to lab-root ns.
- Remove any API path that allows host-root reflector or host-root netlink operations.
- Add guard tests that fail if host route table changes during lab build/teardown.

### Phase 4: Cleanup Guarantees
- Add idempotent cleanup ledger.
- Add panic/abort-safe cleanup paths.
- Add regression tests for:
  - leaked default route
  - leaked links
  - leaked namespaces

### Phase 5: Test Matrix and Documentation
- Add CI matrix docs for:
  - bare metal with file caps
  - restrictive container (`no_new_privs=1` expected fail-fast)
  - mount-restricted host (FD fallback expected)
- Document operator expectations and supported host modes.

## Open Questions
1. Should FD backend become default and named backend opt-in for operability tooling?
2. Do we support environments with `no_new_privs=1` at all, or fail hard with message?
3. Is unprivileged user namespaces (`kernel.unprivileged_userns_clone`) acceptable as fallback dependency?

## Immediate Next Step
Implement Phase 0 and Phase 1 first so we stop guessing host capability and policy behavior.
