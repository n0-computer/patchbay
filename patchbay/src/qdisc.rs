use anyhow::{bail, Context, Result};
use tokio::process::Command;

/// Max retries for transient EAGAIN (os error 11) when spawning `tc` commands.
const SPAWN_RETRIES: u32 = 3;

/// Parameters for `tc netem` impairment.
///
/// All fields default to zero (no impairment). Set only the fields you need.
/// Fields accept both native TOML types and string representations
/// (e.g. `latency_ms = 200` and `latency_ms = "200"` are equivalent).
/// This enables matrix variable substitution in sim TOML files.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LinkLimits {
    /// Rate limit in kbit/s (0 = unlimited).
    #[serde(deserialize_with = "coerce::u32_or_string")]
    pub rate_kbit: u32,
    /// Packet loss percentage (0.0–100.0).
    #[serde(deserialize_with = "coerce::f32_or_string")]
    pub loss_pct: f32,
    /// One-way latency in milliseconds.
    #[serde(deserialize_with = "coerce::u32_or_string")]
    pub latency_ms: u32,
    /// Jitter in milliseconds (uniform ±jitter around latency).
    #[serde(deserialize_with = "coerce::u32_or_string")]
    pub jitter_ms: u32,
    /// Packet reordering percentage (0.0–100.0).
    #[serde(deserialize_with = "coerce::f32_or_string")]
    pub reorder_pct: f32,
    /// Packet duplication percentage (0.0–100.0).
    #[serde(deserialize_with = "coerce::f32_or_string")]
    pub duplicate_pct: f32,
    /// Bit-error corruption percentage (0.0–100.0).
    #[serde(deserialize_with = "coerce::f32_or_string")]
    pub corrupt_pct: f32,
    /// tbf queue depth in milliseconds -- how long a packet may wait in the rate
    /// limiter's buffer before it is dropped. Only applies when `rate_kbit > 0`.
    /// `0` (the default) keeps the historical 400ms buffer. Sizing this near the
    /// link's RTT makes packet loss emerge from buffer overflow (congestion),
    /// as on a real bottleneck, rather than only from `loss_pct`.
    #[serde(default, deserialize_with = "coerce::u32_or_string")]
    pub buffer_ms: u32,
    /// Mean loss-burst length, in packets, for the `loss_pct` random loss.
    ///
    /// `0` or `1` (the default) uses `tc netem`'s independent per-packet
    /// (Bernoulli) loss -- `loss <pct>%`. A value `>= 2` switches to the
    /// Gilbert-Elliott model (`loss gemodel`), a two-state (good/bad) Markov
    /// chain that drops packets in bursts of this mean length while holding the
    /// long-run loss rate at `loss_pct`. Real links (wifi fades, cellular
    /// handovers) lose in bursts, and the same loss rate concentrated into
    /// bursts hurts congestion control more than when it is spread out, so this
    /// is the more faithful model; it is also more variable run to run, so keep
    /// bursts short when repeatability matters.
    #[serde(default, deserialize_with = "coerce::u32_or_string")]
    pub loss_burst_pkts: u32,
}

/// Serde helpers that accept both native types and string representations.
mod coerce {
    use serde::{Deserialize, Deserializer};

    pub(super) fn u32_or_string<'de, D: Deserializer<'de>>(de: D) -> Result<u32, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Val {
            Num(u32),
            Str(String),
        }
        match Val::deserialize(de)? {
            Val::Num(n) => Ok(n),
            Val::Str(s) if s.is_empty() => Ok(0),
            Val::Str(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }

    pub(super) fn f32_or_string<'de, D: Deserializer<'de>>(de: D) -> Result<f32, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Val {
            Num(f32),
            Str(String),
        }
        match Val::deserialize(de)? {
            Val::Num(n) => Ok(n),
            Val::Str(s) if s.is_empty() => Ok(0.0),
            Val::Str(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

/// Applies netem impairment on `ifname`. Caller must already be in the target ns.
pub(crate) async fn apply_impair(ifname: &str, limits: LinkLimits) -> Result<()> {
    remove_qdisc(ifname).await;
    let qdisc = Qdisc::new(ifname);
    qdisc.add_netem_root(limits).await?;
    if limits.rate_kbit > 0 {
        qdisc.add_tbf(limits.rate_kbit, limits.buffer_ms).await?;
    }
    Ok(())
}

pub(crate) async fn remove_qdisc(ifname: &str) {
    let qdisc = Qdisc::new(ifname);
    qdisc.clear_root().await;
}

struct Qdisc<'a> {
    ifname: &'a str,
}

impl<'a> Qdisc<'a> {
    fn new(ifname: &'a str) -> Self {
        Self { ifname }
    }

    async fn clear_root(&self) {
        let mut cmd = Command::new("tc");
        cmd.args(["qdisc", "del", "dev", self.ifname, "root"]);
        let _ = ensure_success(cmd, "tc qdisc del root").await;
    }

    async fn add_netem_root(&self, limits: LinkLimits) -> Result<()> {
        let mut args = vec![
            "qdisc".to_string(),
            "add".into(),
            "dev".into(),
            self.ifname.to_string(),
            "root".into(),
            "handle".into(),
            "1:".into(),
            "netem".into(),
        ];

        if limits.latency_ms > 0 || limits.jitter_ms > 0 {
            args.push("delay".into());
            args.push(format!("{}ms", limits.latency_ms));
            if limits.jitter_ms > 0 {
                args.push(format!("{}ms", limits.jitter_ms));
            }
        }
        if limits.loss_pct > 0.0 {
            args.push("loss".into());
            if limits.loss_burst_pkts >= 2 {
                // Gilbert-Elliott: a good state (no loss) and a bad state
                // (always lose), so losses arrive in bursts. With `p` = good->bad
                // and `r` = bad->good transition probabilities, the mean bad run
                // (burst length) is 1/r and the long-run loss rate is p/(p+r).
                // Invert for a target loss L and mean burst length B:
                //   r = 1/B,  p = L / (B * (1 - L)).
                let l = (limits.loss_pct as f64 / 100.0).clamp(0.0, 0.999);
                let b = limits.loss_burst_pkts as f64;
                let r = 100.0 / b;
                let p = 100.0 * l / (b * (1.0 - l));
                args.push("gemodel".into());
                args.push(format!("{p:.4}%"));
                args.push(format!("{r:.4}%"));
            } else {
                args.push(format!("{:.3}%", limits.loss_pct));
            }
        }
        if limits.reorder_pct > 0.0 {
            args.push("reorder".into());
            args.push(format!("{:.3}%", limits.reorder_pct));
        }
        if limits.duplicate_pct > 0.0 {
            args.push("duplicate".into());
            args.push(format!("{:.3}%", limits.duplicate_pct));
        }
        if limits.corrupt_pct > 0.0 {
            args.push("corrupt".into());
            args.push(format!("{:.3}%", limits.corrupt_pct));
        }

        let mut cmd = Command::new("tc");
        cmd.args(&args);
        ensure_success(cmd, "tc qdisc netem add").await?;
        Ok(())
    }

    async fn add_tbf(&self, rate_kbit: u32, buffer_ms: u32) -> Result<()> {
        // `buffer_ms = 0` keeps the historical 400ms buffer; a smaller value
        // makes the rate limiter drop on overflow near one RTT, so loss emerges
        // from congestion as on a real bottleneck link.
        let latency = if buffer_ms == 0 { 400 } else { buffer_ms };
        let mut cmd = Command::new("tc");
        cmd.args([
            "qdisc",
            "add",
            "dev",
            self.ifname,
            "parent",
            "1:1",
            "handle",
            "10:",
            "tbf",
            "rate",
            &format!("{}kbit", rate_kbit),
            "burst",
            "32kbit",
            "latency",
            &format!("{}ms", latency),
        ]);
        ensure_success(cmd, "tc qdisc tbf add").await?;
        Ok(())
    }
}

async fn ensure_success(mut cmd: Command, context: &str) -> Result<()> {
    // Retry on transient EAGAIN (os error 11) which can happen on
    // resource-constrained CI runners when many namespaces are being
    // created/torn down in quick succession.
    cmd.stderr(std::process::Stdio::piped());
    for attempt in 0..=SPAWN_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(50 * attempt as u64)).await;
        }
        match cmd.output().await {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                bail!("{context} failed: {stderr}");
            }
            Err(e) if e.raw_os_error() == Some(11) && attempt < SPAWN_RETRIES => {
                tracing::debug!(%context, attempt, "EAGAIN, retrying");
            }
            Err(e) => {
                return Err(e).with_context(|| format!("{context}: spawn"));
            }
        }
    }
    bail!("{context}: spawn: EAGAIN after {SPAWN_RETRIES} retries");
}
