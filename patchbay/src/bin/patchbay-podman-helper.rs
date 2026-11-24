//! Internal control process for local rootless Podman.

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let create_namespace = match args.next().as_deref() {
        None => false,
        Some(arg) if arg == "--create-userns" && args.next().is_none() => true,
        _ => anyhow::bail!("invalid Podman helper argument"),
    };
    patchbay::podman::run_helper(create_namespace)
}

#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("patchbay-podman-helper requires Linux")
}
