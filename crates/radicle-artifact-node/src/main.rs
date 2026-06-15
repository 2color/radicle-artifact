//! `rad-artifact-node`: the artifact seeding daemon, run in the
//! foreground.
//!
//! This binary is a pure daemon: it binds the control socket and serves
//! until shut down. All lifecycle UX (detaching, log rotation, status,
//! stop) lives in the `rad-artifact` CLI, which spawns this binary and
//! drives it over the control socket.
//!
//! For encrypted keystores the passphrase is read from `RAD_PASSPHRASE`
//! — the spawning CLI resolves prompts; this process never asks.

use radicle::prelude::Profile;
use radicle_artifact_core::keys::radicle_secret_to_iroh;
use radicle_artifact_node::node;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("rad-artifact-node {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help" | "-h") => {
            println!(
                "rad-artifact-node {}\n\nRuns the artifact seeding daemon in the foreground.\n\
                 Drive it with the rad-artifact CLI (`rad-artifact node ...`).\n\n\
                 Set RAD_PASSPHRASE for encrypted keystores; RUST_LOG controls logging.",
                env!("CARGO_PKG_VERSION")
            );
            return;
        }
        Some(other) => {
            eprintln!("rad-artifact-node: unexpected argument '{other}' (takes none)");
            std::process::exit(2);
        }
        None => {}
    }
    if let Err(e) = run() {
        eprintln!("rad-artifact-node: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // JSON-format tracing to stderr, which the spawning CLI redirects
    // into node.log. Iroh uses tracing too, so this subscriber catches
    // both our events and iroh's, gated by RUST_LOG. Default keeps iroh
    // quiet and our own crates at info.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "warn,iroh=warn,iroh_blobs=warn,radicle_artifact_node=info,radicle_artifact_core=info",
        )
    });
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();

    let profile = Profile::load()?;
    let passphrase = if profile.keystore.is_encrypted()? {
        match radicle::profile::env::passphrase() {
            Some(p) => Some(p),
            None => {
                return Err("encrypted keystore requires RAD_PASSPHRASE in the environment".into())
            }
        }
    } else {
        None
    };
    let secret = radicle_secret_to_iroh(&profile.keystore, passphrase)?;
    let home = profile.home.path().to_path_buf();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(node::run(&home, secret))?;
    Ok(())
}
