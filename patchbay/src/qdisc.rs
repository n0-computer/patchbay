use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Max retries for transient EAGAIN (os error 11) when spawning `tc` commands.
const SPAWN_RETRIES: u32 = 3;

/// Rate-limiter queue depth used when [`LinkCondition::buffer_ms`] is unset.
const DEFAULT_BUFFER_MS: u32 = 400;

/// Mean burst length selected by [`LinkCondition::bursty_loss`].
const DEFAULT_LOSS_BURST_PKTS: u32 = 5;

/// Link impairment profile, applied with `tc netem` and, when a rate cap is
/// set, `tc tbf`.
///
/// Start from a named preset such as [`wifi`](Self::wifi) or
/// [`mobile_4g`](Self::mobile_4g), or from an unimpaired link with
/// [`new`](Self::new), then refine with the chainable setters. Every setter
/// takes and returns `self`, and when two setters touch the same field the
/// later call wins.
///
/// Two numbers describe most real links: a rate cap and a round-trip time.
/// [`rate_mbit`](Self::rate_mbit) and [`rtt_ms`](Self::rtt_ms) express both
/// without requiring any knowledge of the underlying `netem` and `tbf` knobs.
/// `rtt_ms` also sizes the rate-limiter buffer, which is what makes packet loss
/// emerge from congestion the way it does on a real bottleneck.
///
/// In TOML a link condition is either a preset name (`condition = "wifi"`) or a
/// table of these fields. Numeric fields also accept string representations
/// (`latency_ms = 200` and `latency_ms = "200"` are equivalent) so sim files can
/// substitute matrix variables.
///
/// # Examples
///
/// ```
/// # use patchbay::LinkCondition;
/// // A named last-mile profile.
/// let mobile = LinkCondition::mobile_4g();
///
/// // A 40 Mbit link with a 50 ms round trip, no netem knowledge required.
/// let bottleneck = LinkCondition::new().rate_mbit(40).rtt_ms(50);
///
/// // A preset with a single property changed; everything else stays put.
/// let lossy = LinkCondition::wifi().bursty_loss(5.0);
/// ```
#[derive(Debug, Clone, Copy, Default, derive_more::PartialEq, Serialize)]
#[non_exhaustive]
pub struct LinkCondition {
    /// Rate cap in kbit/s. `None` leaves the link uncapped.
    pub rate_kbit: Option<u32>,
    /// One-way latency in milliseconds.
    pub latency_ms: u32,
    /// Jitter in milliseconds, applied as a uniform spread around the latency.
    pub jitter_ms: u32,
    /// Packet loss percentage, from 0.0 to 100.0.
    pub loss_pct: f32,
    /// Mean loss-burst length in packets.
    ///
    /// `None` selects `netem`'s independent per-packet (Bernoulli) loss.
    /// `Some(n)` with `n >= 2` selects the Gilbert-Elliott model, a two-state
    /// Markov chain that drops packets in bursts of mean length `n` while
    /// holding the long-run rate at `loss_pct`. Real links lose in bursts (wifi
    /// fades, cellular handovers), and clustered loss stresses congestion
    /// control more than the same rate spread evenly. Prefer
    /// [`bursty_loss`](Self::bursty_loss) and [`random_loss`](Self::random_loss)
    /// over setting this directly.
    pub loss_burst_pkts: Option<u32>,
    /// Rate-limiter queue depth in milliseconds.
    ///
    /// Bounds how long a packet may wait in the `tbf` buffer before it is
    /// dropped, and applies only when `rate_kbit` is set. `None` selects the
    /// historical 400 ms buffer. Sizing it near the link's round-trip time makes
    /// loss emerge from buffer overflow, as on a real bottleneck, rather than
    /// only from `loss_pct`; [`rtt_ms`](Self::rtt_ms) does that for you.
    pub buffer_ms: Option<u32>,
    /// Packet reordering percentage, from 0.0 to 100.0.
    pub reorder_pct: f32,
    /// Packet duplication percentage, from 0.0 to 100.0.
    pub duplicate_pct: f32,
    /// Bit-error corruption percentage, from 0.0 to 100.0.
    pub corrupt_pct: f32,
    /// Human-readable name, set by the preset constructors.
    ///
    /// Carried into lab events and the devtools timeline so a link reads as
    /// "wifi" rather than as a row of numbers. Setters preserve it, so a tweaked
    /// preset keeps the name it started from and the UI shows the name next to
    /// the effective values. Excluded from equality, and never read back from
    /// TOML.
    #[partial_eq(skip)]
    pub label: Option<&'static str>,
}

impl LinkCondition {
    /// Creates an unimpaired link: uncapped, no latency, no loss.
    pub const fn new() -> Self {
        Self {
            rate_kbit: None,
            latency_ms: 0,
            jitter_ms: 0,
            loss_pct: 0.0,
            loss_burst_pkts: None,
            buffer_ms: None,
            reorder_pct: 0.0,
            duplicate_pct: 0.0,
            corrupt_pct: 0.0,
            label: None,
        }
    }

    /// Creates a wired LAN link.
    ///
    /// 1 ms one-way delay (2 ms round trip), uncapped, no loss. Models
    /// same-rack or same-building Ethernet, where the switch hop is the only
    /// meaningful impairment.
    pub const fn lan() -> Self {
        Self {
            latency_ms: 1,
            label: Some("lan"),
            ..Self::new()
        }
    }

    /// Creates a good WiFi link, 5 GHz and close to the access point.
    ///
    /// 200 Mbit, 4 ms one-way delay (8 ms round trip), 2 ms jitter, 0.1 %
    /// bursty loss, 15 ms buffer.
    pub const fn wifi() -> Self {
        Self {
            rate_kbit: Some(200_000),
            latency_ms: 4,
            jitter_ms: 2,
            loss_pct: 0.1,
            loss_burst_pkts: Some(5),
            buffer_ms: Some(15),
            label: Some("wifi"),
            ..Self::new()
        }
    }

    /// Creates a congested WiFi link, 2.4 GHz with interference.
    ///
    /// 25 Mbit, 25 ms one-way delay, 15 ms jitter, 1.5 % bursty loss, 60 ms
    /// buffer.
    pub const fn wifi_bad() -> Self {
        Self {
            rate_kbit: Some(25_000),
            latency_ms: 25,
            jitter_ms: 15,
            loss_pct: 1.5,
            loss_burst_pkts: Some(8),
            buffer_ms: Some(60),
            label: Some("wifi-bad"),
            ..Self::new()
        }
    }

    /// Creates a 4G/LTE link with good signal.
    ///
    /// 40 Mbit, 25 ms one-way delay (50 ms round trip), 10 ms jitter, 0.3 %
    /// bursty loss, 100 ms buffer.
    pub const fn mobile_4g() -> Self {
        Self {
            rate_kbit: Some(40_000),
            latency_ms: 25,
            jitter_ms: 10,
            loss_pct: 0.3,
            loss_burst_pkts: Some(6),
            buffer_ms: Some(100),
            label: Some("mobile-4g"),
            ..Self::new()
        }
    }

    /// Creates a 3G or degraded 4G link.
    ///
    /// 3 Mbit, 80 ms one-way delay, 25 ms jitter, 0.5 % bursty loss, 250 ms
    /// buffer. The deep buffer models the bufferbloat typical of 3G.
    pub const fn mobile_3g() -> Self {
        Self {
            rate_kbit: Some(3_000),
            latency_ms: 80,
            jitter_ms: 25,
            loss_pct: 0.5,
            loss_burst_pkts: Some(6),
            buffer_ms: Some(250),
            label: Some("mobile-3g"),
            ..Self::new()
        }
    }

    /// Creates a low-earth-orbit satellite link, Starlink class.
    ///
    /// 100 Mbit, 22 ms one-way delay (45 ms round trip), 10 ms jitter, 0.5 %
    /// bursty loss, 50 ms buffer.
    pub const fn satellite() -> Self {
        Self {
            rate_kbit: Some(100_000),
            latency_ms: 22,
            jitter_ms: 10,
            loss_pct: 0.5,
            loss_burst_pkts: Some(4),
            buffer_ms: Some(50),
            label: Some("satellite"),
            ..Self::new()
        }
    }

    /// Creates a geostationary satellite link, HughesNet or Viasat class.
    ///
    /// 25 Mbit, 300 ms one-way delay (600 ms round trip), 20 ms jitter, 0.3 %
    /// bursty loss, 600 ms buffer.
    pub const fn satellite_geo() -> Self {
        Self {
            rate_kbit: Some(25_000),
            latency_ms: 300,
            jitter_ms: 20,
            loss_pct: 0.3,
            loss_burst_pkts: Some(4),
            buffer_ms: Some(600),
            label: Some("satellite-geo"),
            ..Self::new()
        }
    }

    /// Returns the preset matching a TOML preset name, or `None` if unknown.
    pub fn from_preset_name(name: &str) -> Option<Self> {
        match name {
            "lan" => Some(Self::lan()),
            "wifi" => Some(Self::wifi()),
            "wifi-bad" => Some(Self::wifi_bad()),
            "mobile-4g" | "mobile" => Some(Self::mobile_4g()),
            "mobile-3g" => Some(Self::mobile_3g()),
            "satellite" => Some(Self::satellite()),
            "satellite-geo" => Some(Self::satellite_geo()),
            _ => None,
        }
    }

    /// Sets the rate cap in kbit/s.
    pub const fn rate_kbit(mut self, kbit: u32) -> Self {
        self.rate_kbit = Some(kbit);
        self
    }

    /// Sets the rate cap in Mbit/s.
    pub const fn rate_mbit(mut self, mbit: u32) -> Self {
        self.rate_kbit = Some(mbit * 1_000);
        self
    }

    /// Removes the rate cap, leaving the link uncapped.
    pub const fn uncapped(mut self) -> Self {
        self.rate_kbit = None;
        self
    }

    /// Sets the one-way latency in milliseconds.
    pub const fn latency_ms(mut self, ms: u32) -> Self {
        self.latency_ms = ms;
        self
    }

    /// Sets the jitter in milliseconds.
    pub const fn jitter_ms(mut self, ms: u32) -> Self {
        self.jitter_ms = ms;
        self
    }

    /// Sets the latency from a round-trip time, and sizes the buffer to match.
    ///
    /// Halves `rtt` into the one-way [`latency_ms`](Self::latency_ms), and sets
    /// [`buffer_ms`](Self::buffer_ms) to the full round trip if no buffer has
    /// been set yet. A buffer near one round trip is what makes loss emerge from
    /// congestion on a capped link. Presets already carry a buffer, so
    /// `wifi().rtt_ms(80)` keeps the preset's buffer; pass
    /// [`buffer_ms`](Self::buffer_ms) explicitly to override it.
    pub const fn rtt_ms(mut self, rtt: u32) -> Self {
        self.latency_ms = rtt / 2;
        if self.buffer_ms.is_none() {
            self.buffer_ms = Some(rtt);
        }
        self
    }

    /// Sets a bursty loss rate, the way real radio links lose packets.
    ///
    /// Sets [`loss_pct`](Self::loss_pct) to `pct` and selects Gilbert-Elliott
    /// loss with a mean burst of 5 packets, matching the clustered drops of a
    /// wifi fade or a cellular handover. Both fields are fully specified, so a
    /// later [`random_loss`](Self::random_loss) or
    /// [`loss_burst_pkts`](Self::loss_burst_pkts) call overrides this one.
    pub const fn bursty_loss(mut self, pct: f32) -> Self {
        self.loss_pct = pct;
        self.loss_burst_pkts = Some(DEFAULT_LOSS_BURST_PKTS);
        self
    }

    /// Sets an independent per-packet loss rate.
    ///
    /// Sets [`loss_pct`](Self::loss_pct) to `pct` and selects Bernoulli loss,
    /// where every packet is dropped independently. More repeatable run to run
    /// than [`bursty_loss`](Self::bursty_loss), and the right choice when you
    /// want a controlled loss rate rather than a faithful radio link.
    pub const fn random_loss(mut self, pct: f32) -> Self {
        self.loss_pct = pct;
        self.loss_burst_pkts = None;
        self
    }

    /// Sets the packet loss percentage, leaving the loss model unchanged.
    pub const fn loss_pct(mut self, pct: f32) -> Self {
        self.loss_pct = pct;
        self
    }

    /// Sets the mean loss-burst length in packets, selecting bursty loss.
    pub const fn loss_burst_pkts(mut self, pkts: u32) -> Self {
        self.loss_burst_pkts = Some(pkts);
        self
    }

    /// Sets the rate-limiter queue depth in milliseconds.
    pub const fn buffer_ms(mut self, ms: u32) -> Self {
        self.buffer_ms = Some(ms);
        self
    }

    /// Sets the packet reordering percentage.
    pub const fn reorder_pct(mut self, pct: f32) -> Self {
        self.reorder_pct = pct;
        self
    }

    /// Sets the packet duplication percentage.
    pub const fn duplicate_pct(mut self, pct: f32) -> Self {
        self.duplicate_pct = pct;
        self
    }

    /// Sets the bit-error corruption percentage.
    pub const fn corrupt_pct(mut self, pct: f32) -> Self {
        self.corrupt_pct = pct;
        self
    }

    /// Sets the human-readable name carried into events and the devtools UI.
    pub const fn label(mut self, label: &'static str) -> Self {
        self.label = Some(label);
        self
    }
}

impl<'de> Deserialize<'de> for LinkCondition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Table form, mirroring the public fields minus the label.
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Fields {
            #[serde(deserialize_with = "coerce::opt_u32_or_string")]
            rate_kbit: Option<u32>,
            #[serde(deserialize_with = "coerce::u32_or_string")]
            latency_ms: u32,
            #[serde(deserialize_with = "coerce::u32_or_string")]
            jitter_ms: u32,
            #[serde(deserialize_with = "coerce::f32_or_string")]
            loss_pct: f32,
            #[serde(deserialize_with = "coerce::opt_u32_or_string")]
            loss_burst_pkts: Option<u32>,
            #[serde(deserialize_with = "coerce::opt_u32_or_string")]
            buffer_ms: Option<u32>,
            #[serde(deserialize_with = "coerce::f32_or_string")]
            reorder_pct: f32,
            #[serde(deserialize_with = "coerce::f32_or_string")]
            duplicate_pct: f32,
            #[serde(deserialize_with = "coerce::f32_or_string")]
            corrupt_pct: f32,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Preset(String),
            Fields(Fields),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Preset(name) => LinkCondition::from_preset_name(&name).ok_or_else(|| {
                serde::de::Error::custom(format!("unknown link condition preset '{name}'"))
            }),
            Repr::Fields(f) => Ok(LinkCondition {
                rate_kbit: f.rate_kbit,
                latency_ms: f.latency_ms,
                jitter_ms: f.jitter_ms,
                loss_pct: f.loss_pct,
                loss_burst_pkts: f.loss_burst_pkts,
                buffer_ms: f.buffer_ms,
                reorder_pct: f.reorder_pct,
                duplicate_pct: f.duplicate_pct,
                corrupt_pct: f.corrupt_pct,
                label: None,
            }),
        }
    }
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

    /// Accepts a number, a string, or null, mapping null and the empty string
    /// to `None` so an unset knob stays unset.
    pub(super) fn opt_u32_or_string<'de, D: Deserializer<'de>>(
        de: D,
    ) -> Result<Option<u32>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Val {
            Num(u32),
            Str(String),
        }
        match Option::<Val>::deserialize(de)? {
            None => Ok(None),
            Some(Val::Num(n)) => Ok(Some(n)),
            Some(Val::Str(s)) if s.is_empty() => Ok(None),
            Some(Val::Str(s)) => s.parse().map(Some).map_err(serde::de::Error::custom),
        }
    }
}

/// Applies netem impairment on `ifname`. Caller must already be in the target ns.
///
/// # Errors
///
/// Returns an error when the rate cap is `Some(0)`, which `tbf` cannot express,
/// or when a `tc` command fails.
pub(crate) async fn apply_impair(ifname: &str, cond: LinkCondition) -> Result<()> {
    remove_qdisc(ifname).await;
    let qdisc = Qdisc::new(ifname);
    qdisc.add_netem_root(cond).await?;
    if let Some(rate_kbit) = cond.rate_kbit {
        if rate_kbit == 0 {
            bail!(
                "link condition has a rate cap of 0 kbit/s, which tbf cannot express; \
                 leave rate_kbit unset (or call `uncapped()`) for an uncapped link"
            );
        }
        qdisc.add_tbf(rate_kbit, cond.buffer_ms).await?;
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

    async fn add_netem_root(&self, limits: LinkCondition) -> Result<()> {
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
            match limits.loss_burst_pkts {
                // Gilbert-Elliott: a good state (no loss) and a bad state
                // (always lose), so losses arrive in bursts. With `p` = good->bad
                // and `r` = bad->good transition probabilities, the mean bad run
                // (burst length) is 1/r and the long-run loss rate is p/(p+r).
                // Invert for a target loss L and mean burst length B:
                //   r = 1/B,  p = L / (B * (1 - L)).
                Some(burst) if burst >= 2 => {
                    let l = (limits.loss_pct as f64 / 100.0).clamp(0.0, 0.999);
                    let b = burst as f64;
                    let r = 100.0 / b;
                    let p = 100.0 * l / (b * (1.0 - l));
                    args.push("gemodel".into());
                    args.push(format!("{p:.4}%"));
                    args.push(format!("{r:.4}%"));
                }
                _ => args.push(format!("{:.3}%", limits.loss_pct)),
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

    async fn add_tbf(&self, rate_kbit: u32, buffer_ms: Option<u32>) -> Result<()> {
        // An unset buffer keeps the historical 400ms depth; a smaller value makes
        // the rate limiter drop on overflow near one RTT, so loss emerges from
        // congestion as on a real bottleneck link. `tc` behaves erratically with
        // a zero latency, so keep at least 1ms.
        let latency = buffer_ms.unwrap_or(DEFAULT_BUFFER_MS).max(1);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Deserialization target, since a bare `LinkCondition` is not a TOML document.
    #[derive(Debug, Deserialize)]
    struct Holder {
        condition: LinkCondition,
    }

    fn parse(toml_str: &str) -> LinkCondition {
        toml::from_str::<Holder>(toml_str)
            .expect("valid link condition")
            .condition
    }

    #[test]
    fn label_is_excluded_from_equality() {
        // A preset and a hand-built condition with the same numbers compare
        // equal, so tests can assert against `LinkCondition::wifi()`.
        let relabelled = LinkCondition::wifi().label("something else");
        assert_eq!(LinkCondition::wifi(), relabelled);
        assert_eq!(relabelled.label, Some("something else"));
    }

    #[test]
    fn setters_preserve_the_preset_label() {
        let tweaked = LinkCondition::mobile_4g().bursty_loss(5.0);
        assert_eq!(tweaked.label, Some("mobile-4g"));
        assert_eq!(tweaked.loss_pct, 5.0);
    }

    #[test]
    fn rtt_ms_halves_latency_and_sizes_the_buffer() {
        let cond = LinkCondition::new().rate_mbit(40).rtt_ms(50);
        assert_eq!(cond.latency_ms, 25);
        assert_eq!(cond.buffer_ms, Some(50));
        assert_eq!(cond.rate_kbit, Some(40_000));
    }

    #[test]
    fn explicit_buffer_wins_over_rtt_in_either_order() {
        let before = LinkCondition::new().buffer_ms(100).rtt_ms(50);
        let after = LinkCondition::new().rtt_ms(50).buffer_ms(100);
        assert_eq!(before.buffer_ms, Some(100));
        assert_eq!(after.buffer_ms, Some(100));
        assert_eq!(before, after);
    }

    #[test]
    fn rtt_ms_keeps_a_preset_buffer() {
        // Presets carry a buffer, so `rtt_ms` only moves the latency.
        let cond = LinkCondition::wifi().rtt_ms(80);
        assert_eq!(cond.latency_ms, 40);
        assert_eq!(cond.buffer_ms, LinkCondition::wifi().buffer_ms);
    }

    #[test]
    fn loss_helpers_select_the_loss_model() {
        assert_eq!(
            LinkCondition::new().bursty_loss(1.5).loss_burst_pkts,
            Some(DEFAULT_LOSS_BURST_PKTS)
        );
        assert_eq!(LinkCondition::new().random_loss(1.5).loss_burst_pkts, None);
        // Later call wins: each helper fully specifies the pair.
        assert_eq!(
            LinkCondition::new().bursty_loss(1.5).random_loss(2.0),
            LinkCondition::new().random_loss(2.0)
        );
    }

    #[test]
    fn uncapped_clears_a_preset_rate() {
        assert_eq!(LinkCondition::wifi().uncapped().rate_kbit, None);
    }

    #[test]
    fn presets_round_trip_through_their_names() {
        let presets = [
            LinkCondition::lan(),
            LinkCondition::wifi(),
            LinkCondition::wifi_bad(),
            LinkCondition::mobile_4g(),
            LinkCondition::mobile_3g(),
            LinkCondition::satellite(),
            LinkCondition::satellite_geo(),
        ];
        for preset in presets {
            let name = preset.label.expect("preset carries a label");
            let parsed = LinkCondition::from_preset_name(name).expect("name resolves");
            assert_eq!(parsed, preset, "preset '{name}' did not round-trip");
            assert_eq!(parsed.label, Some(name));
        }
    }

    #[test]
    fn deserializes_a_preset_name() {
        assert_eq!(parse(r#"condition = "wifi""#), LinkCondition::wifi());
        // The legacy alias for the 4G preset still resolves.
        assert_eq!(parse(r#"condition = "mobile""#), LinkCondition::mobile_4g());
    }

    #[test]
    fn rejects_an_unknown_preset_name() {
        let err = toml::from_str::<Holder>(r#"condition = "dial-up""#).unwrap_err();
        assert!(
            err.to_string().contains("dial-up"),
            "error should name the bad preset, got: {err}"
        );
    }

    #[test]
    fn deserializes_a_field_table() {
        let cond = parse(r#"condition = { latency_ms = 200, loss_pct = 5.0, rate_kbit = 500 }"#);
        assert_eq!(cond.latency_ms, 200);
        assert_eq!(cond.loss_pct, 5.0);
        assert_eq!(cond.rate_kbit, Some(500));
        // Unset optional knobs stay unset rather than becoming zero.
        assert_eq!(cond.buffer_ms, None);
        assert_eq!(cond.loss_burst_pkts, None);
        assert_eq!(cond.label, None);
    }

    #[test]
    fn deserializes_numbers_given_as_strings() {
        // Sim files substitute matrix variables as strings.
        let cond = parse(r#"condition = { latency_ms = "200", rate_kbit = "500" }"#);
        assert_eq!(cond.latency_ms, 200);
        assert_eq!(cond.rate_kbit, Some(500));
    }

    #[test]
    fn netem_args_use_gemodel_only_for_bursty_loss() {
        // Guards the Gilbert-Elliott branch selection without invoking `tc`.
        let bursty = LinkCondition::new().bursty_loss(1.5);
        let random = LinkCondition::new().random_loss(1.5);
        assert!(matches!(bursty.loss_burst_pkts, Some(n) if n >= 2));
        assert!(random.loss_burst_pkts.is_none());
    }
}
