# Plan: Virtual Time / Time Acceleration

Status: **stub** — needs further research before implementation.

## Problem

Tests run in wall-clock time. A NAT mapping timeout test (5 min) takes 5+ min.
ICE restart timers, DTLS retransmit backoff, keepalive intervals — all require
waiting real seconds/minutes. This limits how many timeout-sensitive scenarios
can be covered in a test suite that needs to finish in reasonable time.

## Approaches

### 1. Shadow-style syscall interception (LD_PRELOAD + seccomp)

Shadow intercepts `clock_gettime`, `gettimeofday`, `nanosleep`, `timerfd_*`,
`poll`/`epoll_wait` timeouts via an LD_PRELOAD shim + seccomp filter. The
simulated clock only advances when the event queue dictates. A 60-minute
scenario completes in seconds.

**Pros**: Works with unmodified binaries. Proven at scale (6000+ Tor nodes).
**Cons**: Fragile with Rust binaries that use vDSO for `clock_gettime` (bypasses
LD_PRELOAD). Requires ptrace fallback for vDSO, which adds overhead. Complex to
implement. ~400 syscalls need interception. Cannot coexist with real kernel NAT
(Shadow implements its own conntrack internally).

### 2. Kernel-level nf_conntrack timeout override

Instead of virtualizing time globally, just make conntrack timeouts very short:
```bash
sysctl -w net.netfilter.nf_conntrack_udp_timeout=2          # 2 seconds
sysctl -w net.netfilter.nf_conntrack_udp_timeout_stream=5   # 5 seconds
```

**Pros**: No binary modification. Works with real nftables NAT. Trivial.
**Cons**: Only accelerates conntrack-related behavior. Application-level timers
still run at wall-clock speed. Tests must be written to expect short timeouts.

### 3. Application-level timeout parameterization

Pass test-mode configuration to the application under test that shortens all
relevant timeouts (NAT keepalive: 1s instead of 25s, ICE restart: 500ms
instead of 5s, etc.).

**Pros**: No simulator changes needed. Application controls its own timing.
**Cons**: Requires the application to support test-mode timeouts. Not all
applications are configurable this way. Doesn't test the real timeout values.

### 4. Hybrid: short conntrack + application test mode

Combine approaches 2 and 3. Set conntrack timeouts to 2-5s in the simulator,
configure the application with matching short keepalive intervals.

**Pros**: Practical today with minimal effort. Tests real NAT behavior.
**Cons**: Doesn't test production timeout values. Still requires wall-clock waits
of a few seconds per scenario.

## Recommendation

Start with approach 4 (hybrid) as part of the NAT preset work — each NAT preset
already proposes conntrack timeout values. Making these configurable and
defaulting to short values in test mode gives us 90% of the benefit.

Research approach 1 (Shadow-style) as a longer-term project if the test suite
grows to need it. Key question: can we use `ptrace` to intercept `clock_gettime`
vDSO calls without unacceptable overhead for Rust async runtimes?

## Open Questions

- What is the overhead of ptrace-based time interception on a tokio runtime?
- Can we selectively intercept only in the spawned application process, leaving
  the simulator itself on real time?
- Is there a lighter-weight approach using `CLOCK_MONOTONIC` namespacing (Linux
  time namespaces, `CLONE_NEWTIME`)? This is available since kernel 5.6 but
  only offsets the clock, doesn't allow acceleration.
