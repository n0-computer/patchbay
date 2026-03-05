use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use patchbay::config::LabConfig;

use crate::sim::SimFile;

/// Load topology config for a sim.
///
/// If `sim.topology` is set, load from `../topos/<name>.toml` relative to the sim file,
/// with a fallback to `<cwd>/topos/<name>.toml`.
pub fn load_topology(sim: &SimFile, sim_path: &Path) -> Result<LabConfig> {
    if let Some(name) = &sim.sim.topology {
        if !sim.topology.router.is_empty()
            || !sim.topology.device.is_empty()
            || sim.topology.region.is_some()
        {
            bail!(
                "sim.topology is set to '{}'; inline router/device/region tables are not allowed",
                name
            );
        }
        let fallback_root = std::env::current_dir()
            .context("resolve current dir for topology fallback")?
            .join("topos")
            .join(format!("{name}.toml"));
        let topo_file = sim_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!("../topos/{name}.toml"));
        let chosen = if topo_file.exists() {
            topo_file
        } else if fallback_root.exists() {
            fallback_root
        } else {
            bail!(
                "topology '{}' not found in '{}' or '{}'",
                name,
                sim_path.parent().unwrap_or(Path::new(".")).display(),
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("topos")
                    .display()
            );
        };
        let text = std::fs::read_to_string(&chosen)
            .with_context(|| format!("read topology file {}", chosen.display()))?;
        toml::from_str::<LabConfig>(&text).context("parse topology file")
    } else {
        Ok(sim.topology.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimMeta;

    fn write_temp_file(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&path, body).expect("write temp file");
        path
    }

    #[test]
    fn prefers_adjacent_parent_topos() {
        let root = std::env::temp_dir().join(format!(
            "patchbay-topology-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let sim_file = write_temp_file(
            &root,
            "sims/one/sim.toml",
            "[sim]\nname='x'\ntopology='a'\n",
        );
        let topo = r#"
[[router]]
name = "r1"
[device.d1.eth0]
gateway = "r1"
"#;
        write_temp_file(&root, "sims/topos/a.toml", topo);

        let sim = SimFile {
            sim: SimMeta {
                name: "x".into(),
                topology: Some("a".into()),
                binaries: None,
            },
            ..Default::default()
        };

        let cfg = load_topology(&sim, &sim_file).expect("load topology");
        assert_eq!(cfg.router.len(), 1);
        assert!(cfg.device.contains_key("d1"));
    }

    #[test]
    fn rejects_inline_when_topology_ref_set() {
        let sim = SimFile {
            sim: SimMeta {
                name: "x".into(),
                topology: Some("a".into()),
                binaries: None,
            },
            topology: patchbay::config::LabConfig {
                router: vec![patchbay::config::RouterConfig {
                    name: "r1".into(),
                    region: None,
                    upstream: None,
                    nat: patchbay::Nat::None,
                    ip_support: patchbay::IpSupport::V4Only,
                    nat_v6: patchbay::NatV6Mode::None,
                    ra_enabled: None,
                    ra_interval_secs: None,
                    ra_lifetime_secs: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let err = match load_topology(&sim, Path::new("sims/sim.toml")) {
            Ok(_) => panic!("expected inline-topology rejection"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("inline router/device/region"));
    }
}
