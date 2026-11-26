//! TOML configuration structures used by [`crate::Lab::load`].

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

use crate::{IpSupport, Nat, NatV6Mode, PullPolicy};

/// Parsed lab configuration from TOML.
#[derive(Deserialize, Clone, Default)]
#[non_exhaustive]
pub struct LabConfig {
    /// Optional region-latency map.
    pub region: Option<HashMap<String, RegionConfig>>,
    /// Router entries.
    #[serde(default)]
    pub router: Vec<RouterConfig>,
    /// Raw device tables; post-processed by [`LabBuilder::config`](crate::LabBuilder::config).
    #[serde(default)]
    pub device: HashMap<String, toml::Value>,
}

/// Per-region latency configuration.
#[derive(Deserialize, Clone, Default)]
#[non_exhaustive]
pub struct RegionConfig {
    /// Map of target-region name to one-way latency in ms.
    #[serde(default)]
    pub latencies: HashMap<String, u32>,
}

/// Router configuration entry.
#[derive(Deserialize, Clone)]
#[non_exhaustive]
pub struct RouterConfig {
    /// Router name.
    pub name: String,
    /// Optional region tag (used for inter-region latency rules).
    pub region: Option<String>,
    /// Name of the upstream router.  If absent the router attaches to the IX switch.
    pub upstream: Option<String>,
    /// NAT mode.  Defaults to `"none"` (public downstream, no NAT).
    #[serde(default)]
    pub nat: Nat,
    /// IP address family support.  Defaults to `"v4-only"`.
    #[serde(default)]
    pub ip_support: IpSupport,
    /// IPv6 NAT mode.  Defaults to `"none"`.
    #[serde(default)]
    pub nat_v6: NatV6Mode,
    /// Optional override for RA emission in RA-driven provisioning mode.
    pub ra_enabled: Option<bool>,
    /// Optional RA interval in seconds.
    pub ra_interval_secs: Option<u64>,
    /// Optional RA lifetime in seconds.
    pub ra_lifetime_secs: Option<u64>,
}

impl RouterConfig {
    /// Creates a router entry named `name` with the defaults of the TOML format.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            region: None,
            upstream: None,
            nat: Nat::default(),
            ip_support: IpSupport::default(),
            nat_v6: NatV6Mode::default(),
            ra_enabled: None,
            ra_interval_secs: None,
            ra_lifetime_secs: None,
        }
    }
}

/// Container table of a device, `[device.<name>.container]`.
///
/// A device with this table runs the image as a container, see
/// [`Lab::add_container`](crate::Lab::add_container). The fields map to the
/// methods of [`ContainerBuilder`](crate::ContainerBuilder).
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContainerConfig {
    /// OCI image reference, for example `"docker.io/library/nginx:alpine"`.
    pub image: String,
    /// Replaces the image's entrypoint. An empty list clears it.
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    /// Arguments of the container's command, after the image name.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables inside the container.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Bind mounts as `"host:container[:ro|:rw]"`.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Paths inside the container to mount a fresh tmpfs at.
    #[serde(default)]
    pub tmpfs: Vec<String>,
    /// Extra flags for `podman run`, before the image name.
    #[serde(default)]
    pub run_args: Vec<String>,
    /// When to pull the image. Defaults to `"missing"`.
    #[serde(default)]
    pub pull: PullPolicy,
    /// TCP port that must accept a connection before the lab is ready.
    #[serde(default, deserialize_with = "coerce::opt_num_or_string")]
    pub ready_tcp: Option<u16>,
    /// How long to wait for `ready_tcp`, in seconds. Defaults to 30.
    #[serde(default, deserialize_with = "coerce::opt_num_or_string")]
    pub ready_timeout_secs: Option<u64>,
}

/// Serde helpers that accept a number or its string form.
///
/// Matrix substitution in a sim produces strings, so a placeholder such as
/// `"${matrix.port}"` must work for a numeric field.
pub(crate) mod coerce {
    use std::{any::type_name, fmt, marker::PhantomData, str::FromStr};

    use serde::{
        de::{Error, Visitor},
        Deserializer,
    };

    /// Accepts a number or its string form, and the empty string as the default.
    pub(crate) fn num_or_string<'de, D, T>(de: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr + Default,
        T::Err: fmt::Display,
    {
        Ok(opt_num_or_string(de)?.unwrap_or_default())
    }

    /// Like [`num_or_string`], but optional, with the empty string as `None`.
    pub(crate) fn opt_num_or_string<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: fmt::Display,
    {
        de.deserialize_any(NumOrString(PhantomData))
    }

    /// Parses numbers through their string form, so errors name the type.
    struct NumOrString<T>(PhantomData<T>);

    impl<T: FromStr> NumOrString<T>
    where
        T::Err: fmt::Display,
    {
        fn parse<E: Error>(value: &str) -> Result<Option<T>, E> {
            let parsed = value.parse().map(Some);
            parsed
                .map_err(|err| E::custom(format!("invalid {} `{value}`: {err}", type_name::<T>())))
        }
    }

    impl<'de, T: FromStr> Visitor<'de> for NumOrString<T>
    where
        T::Err: fmt::Display,
    {
        type Value = Option<T>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a {} or a string with one", type_name::<T>())
        }

        fn visit_i64<E: Error>(self, value: i64) -> Result<Self::Value, E> {
            Self::parse(&value.to_string())
        }

        fn visit_u64<E: Error>(self, value: u64) -> Result<Self::Value, E> {
            Self::parse(&value.to_string())
        }

        fn visit_f64<E: Error>(self, value: f64) -> Result<Self::Value, E> {
            // `{:?}` keeps the `.0` of a whole float, which integer types reject.
            Self::parse(&format!("{value:?}"))
        }

        fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
            match value.trim() {
                "" => Ok(None),
                value => Self::parse(value),
            }
        }

        fn visit_none<E: Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
            de.deserialize_any(self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<ContainerConfig, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn container_config_parses_all_keys() {
        let cfg = parse(
            r#"
            image = "img"
            entrypoint = ["/bin/sh", "-c"]
            args = ["a"]
            env = { A = "1" }
            volumes = ["/a:/b:rw"]
            tmpfs = ["/t"]
            run_args = ["--x"]
            pull = "never"
            ready_tcp = 80
            ready_timeout_secs = 5
            "#,
        )
        .unwrap();
        assert_eq!(cfg.pull, PullPolicy::Never);
        assert_eq!(
            cfg.entrypoint.as_deref(),
            Some(&["/bin/sh".to_string(), "-c".to_string()][..])
        );
        assert_eq!(cfg.env["A"], "1");
        assert_eq!(cfg.ready_tcp, Some(80));
        assert_eq!(cfg.ready_timeout_secs, Some(5));
    }

    #[test]
    fn container_config_defaults() {
        let cfg = parse(r#"image = "img""#).unwrap();
        assert_eq!(cfg.pull, PullPolicy::Missing);
        assert_eq!(cfg.entrypoint, None);
        assert_eq!(cfg.ready_tcp, None);
        assert_eq!(cfg.ready_timeout_secs, None);
    }

    /// Matrix substitution produces strings, so numeric fields accept them.
    #[test]
    fn container_config_numbers_accept_strings() {
        let cfg = parse(
            r#"
            image = "img"
            ready_tcp = "8080"
            ready_timeout_secs = "60"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.ready_tcp, Some(8080));
        assert_eq!(cfg.ready_timeout_secs, Some(60));
        let cfg = parse("image = \"img\"\nready_tcp = \"\"").unwrap();
        assert_eq!(cfg.ready_tcp, None);
        // The errors name the type.
        for value in ["\"http\"", "70000", "-1", "80.0", "true"] {
            let err = parse(&format!("image = \"img\"\nready_tcp = {value}")).unwrap_err();
            assert!(err.to_string().contains("u16"), "{value}: {err}");
        }
    }

    #[test]
    fn container_config_rejects_bad_input() {
        let err = parse("image = \"img\"\nports = [80]").unwrap_err();
        assert!(err.to_string().contains("unknown field `ports`"), "{err}");
        let err = parse("image = \"img\"\nenv = { A = 1 }").unwrap_err();
        assert!(err.to_string().contains("expected a string"), "{err}");
        let err = parse("image = \"img\"\npull = \"Never\"").unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "{err}");
        assert!(parse("args = []").is_err());
    }
}
