//! Hands-on playground for the update engine, with no daemon and no network.
//!
//! `updaterd`'s socket server and `robotctl`'s transport are not wired up yet, so
//! this drives [`Engine`] directly. It also does the **publisher** side (keygen,
//! build, sign, manifest) which `robotctl` will never do — that belongs to CI — so
//! parts of this stay useful even once the daemon is live.
//!
//! ```text
//!   cargo run -p updater --example playground -- init      /tmp/duck
//!   cargo run -p updater --example playground -- publish   /tmp/duck 1.0.0
//!   cargo run -p updater --example playground -- check     /tmp/duck
//!   cargo run -p updater --example playground -- apply     /tmp/duck
//! ```
//!
//! Everything lives under one playground directory:
//! ```text
//!   <root>/keys/prod.pub          trusted key (what the robot ships with)
//!   <root>/secret.key             signing key (in reality: CI secret, offline)
//!   <root>/published/             the "remote": signed manifests + artifacts
//!   <root>/opt/robot/daemon/      the robot's install tree
//!   <root>/var/lib/robot/updater/ engine state: log, lock, boot counter
//!   <root>/updater.toml           config
//! ```

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use updater::config::Config;
use updater::engine::{ApplyOptions, Engine};
use updater::faults::Faults;
use updater::proto::{ApplyResult, Target};
use updater::robot::{Health, RobotClient, SafeToRestart};
use updater::verify::KeyRing;

#[derive(Parser)]
#[command(about = "Drive the update engine locally, without a daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the playground: keypair, directories, config.
    Init {
        root: PathBuf,
        /// Wipe an existing playground first.
        #[arg(long)]
        force: bool,
    },

    /// Build, sign and publish a release into the fake remote.
    Publish {
        root: PathBuf,
        version: String,
        /// Embed a post-install hook that succeeds.
        #[arg(long)]
        hook: bool,
        /// Embed a post-install hook that fails — should trigger a rollback.
        #[arg(long)]
        bad_hook: bool,
        /// Corrupt the artifact after signing — should be refused.
        #[arg(long)]
        tamper: bool,
    },

    /// Is an update available?
    Check { root: PathBuf },

    /// Install latest, or an exact version.
    Apply {
        root: PathBuf,
        #[arg(long)]
        version: Option<semver::Version>,
        #[arg(long)]
        dry_run: bool,
        /// Pretend the robot comes up unhealthy — should trigger a rollback.
        #[arg(long)]
        unhealthy: bool,
        /// Inject a fault: fail_health, fail_post_hook, hang_health,
        /// abort_after_swap, corrupt_artifact, simulate_disk_full, fail_rollback.
        #[arg(long = "fault")]
        faults: Vec<String>,
    },

    /// What's installed, and what happened last.
    Status { root: PathBuf },

    /// Return to the previous release.
    Rollback { root: PathBuf },

    /// Boot-time recovery. Run twice after `--fault abort_after_swap` to see the
    /// boot counter revert an update that never confirmed healthy.
    Recover { root: PathBuf },

    /// Recent attempts and outcomes.
    Log { root: PathBuf },
}

/// Stands in for `robotd`. `healthy: false` is how you exercise the rollback path.
struct FakeRobot {
    healthy: bool,
}

#[async_trait::async_trait]
impl RobotClient for FakeRobot {
    async fn safe_to_restart(&self, _t: std::time::Duration) -> SafeToRestart {
        SafeToRestart::Yes
    }
    async fn health(&self, _t: std::time::Duration) -> Health {
        if self.healthy {
            Health::Healthy
        } else {
            Health::Unhealthy("fake robot reports unhealthy".into())
        }
    }
    async fn model_api(&self, _t: std::time::Duration) -> Option<u32> {
        Some(1)
    }
    async fn remote_session_active(&self, _t: std::time::Duration) -> bool {
        false
    }
}

/// Restore default `SIGPIPE` handling.
///
/// Rust ignores `SIGPIPE` at startup, so writing to a closed stdout returns `EPIPE`
/// and `println!` **panics** — meaning `playground log | head` dies with a
/// backtrace instead of exiting quietly like every other unix tool. Resetting it makes
/// the process terminate the way `ls | head` does.
///
/// Found by the board test, which pipes output through `head`.
fn restore_sigpipe() {
    // Safety: setting a signal disposition to the default is always valid, and this
    // runs before any threads exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    restore_sigpipe();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Printed with Display, not Debug: returning `Result` from `main` would escape
    // newlines and wrap the message in quotes.
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Init { root, force } => init(&root, force)?,
        Command::Publish {
            root,
            version,
            hook,
            bad_hook,
            tamper,
        } => publish(&root, &version, hook, bad_hook, tamper)?,
        Command::Check { root } => {
            let engine = engine(&root, true, &[])?;
            println!("{:#?}", engine.check("daemon").await?);
        }
        Command::Apply {
            root,
            version,
            dry_run,
            unhealthy,
            faults,
        } => {
            let mut engine = engine(&root, !unhealthy, &faults)?;
            let target = version.map(Target::Exact).unwrap_or(Target::Latest);

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<updater::proto::Progress>();
            let printer = tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    match p.percent {
                        Some(pct) => println!("  [{:?}] {pct}%", p.phase),
                        None => println!("  [{:?}]", p.phase),
                    }
                }
            });

            let result = engine
                .apply(
                    "daemon",
                    target,
                    ApplyOptions {
                        dry_run,
                        interrupt_sessions: false,
                    },
                    tx,
                )
                .await;
            printer.await.ok();

            match result {
                Ok(outcome) => {
                    describe(&outcome);
                    show_live(&root);
                }
                Err(e) => {
                    // Refusals are the interesting case: they should leave nothing
                    // behind. The error code is what `robotctl` maps to an exit code.
                    println!("REFUSED (code {}): {e}", e.code());
                    show_live(&root);
                }
            }
        }
        Command::Status { root } => {
            let engine = engine(&root, true, &[])?;
            for status in engine.status().await? {
                println!(
                    "{}: {} ({})",
                    status.component,
                    show(&status.installed),
                    match status.healthy {
                        Some(true) => "healthy",
                        Some(false) => "UNHEALTHY",
                        None => "no probe",
                    }
                );
                if let Some(pinned) = &status.pinned {
                    println!("  pinned to {pinned}");
                }
                if let Some(last) = status.last_attempt {
                    println!(
                        "  last attempt: {} -> {}: {}",
                        show(&last.from),
                        show(&last.to),
                        describe_outcome(&last.outcome)
                    );
                }
            }
            println!("\ninstalled releases:");
            for release in engine.list_installed("daemon")? {
                let mut tags = Vec::new();
                if release.active {
                    tags.push("active");
                }
                if release.golden {
                    tags.push("golden");
                }
                println!("  {} {}", release.version, tags.join(","));
            }
        }
        Command::Rollback { root } => {
            let mut engine = engine(&root, true, &[])?;
            describe(&engine.rollback("daemon").await?);
            show_live(&root);
        }
        Command::Recover { root } => {
            let mut engine = engine(&root, true, &[])?;
            let outcomes = engine.recover_on_start().await?;
            if outcomes.is_empty() {
                println!("nothing to recover (update still on trial, or none pending)");
            } else {
                for outcome in &outcomes {
                    describe(outcome);
                }
            }
            show_live(&root);
        }
        Command::Log { root } => {
            let engine = engine(&root, true, &[])?;
            for entry in engine.log(20)? {
                println!(
                    "{} {} {} -> {}: {}",
                    entry.at,
                    entry.component,
                    show(&entry.from),
                    show(&entry.to),
                    describe_outcome(&entry.outcome)
                );
            }
        }
    }
    Ok(())
}

/// Versions with `Display`, not `Debug`: `Some(Version { major: 1, .. })` is struct
/// internals leaking into output someone has to read.
fn show(version: &Option<semver::Version>) -> String {
    match version {
        Some(v) => v.to_string(),
        None => "none".into(),
    }
}

fn describe_outcome(outcome: &updater::proto::Outcome) -> String {
    use updater::proto::Outcome;
    match outcome {
        Outcome::Success => "success".into(),
        Outcome::RolledBack { reason } => format!("rolled back ({reason})"),
        Outcome::Aborted { reason } => format!("aborted ({reason})"),
    }
}

fn describe(outcome: &ApplyResult) {
    match outcome {
        ApplyResult::Applied { from, to } => println!("APPLIED {} -> {to}", show(from)),
        ApplyResult::AlreadyCurrent { version } => println!("ALREADY CURRENT at {version}"),
        ApplyResult::DryRunPassed { candidate } => {
            println!("DRY RUN PASSED for {candidate} (nothing swapped)")
        }
        ApplyResult::RolledBack {
            attempted,
            reverted_to,
            reason,
        } => println!(
            "ROLLED BACK from {attempted} to {}: {reason}",
            show(reverted_to)
        ),
        ApplyResult::Stuck { version, reason } => {
            println!("STUCK on {version} (nothing was reverted): {reason}")
        }
    }
}

/// Reads through the symlink, so it shows what the robot would actually load —
/// not merely where the link points.
fn show_live(root: &Path) {
    let current = root.join("opt/robot/daemon/current");
    match std::fs::read_to_string(current.join("version.toml")) {
        Ok(text) => println!("live content: {}", text.trim()),
        Err(_) => println!("live content: <nothing installed>"),
    }
}

fn init(root: &Path, force: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Reusing a playground is a trap: `init` mints a new keypair, so any release
    // already in `published/` becomes unverifiable, and a stale install tree makes
    // `latest` resolve to a version you never published in this session. Refuse
    // rather than produce a confusing mismatch.
    if root.exists() && std::fs::read_dir(root)?.next().is_some() {
        if !force {
            return Err(format!(
                "{} already exists and is not empty.\n\
                 `init` generates a new signing key, which would leave existing releases \
                 unverifiable.\n\
                 Re-run with --force to wipe it, or pick a different directory.",
                root.display()
            )
            .into());
        }
        std::fs::remove_dir_all(root)?;
    }

    for dir in [
        "keys",
        "published",
        "opt/robot/daemon",
        "var/lib/robot/updater",
    ] {
        std::fs::create_dir_all(root.join(dir))?;
    }

    let keypair = minisign::KeyPair::generate_unencrypted_keypair()?;
    std::fs::write(root.join("keys/prod.pub"), keypair.pk.to_box()?.to_string())?;
    // In production this never touches the robot: it lives in CI secrets, ideally
    // with an offline master. It's here only so the playground can publish.
    std::fs::write(
        root.join("secret.key"),
        keypair.sk.to_box(None)?.to_string(),
    )?;

    let config = format!(
        r#"# Generated by the playground example.
trusted_keys_dir = "{keys}"
hw_rev = 1
state_dir = "{state}"

[component.daemon]
install_dir = "{install}"
source = {{ type = "local_dir", path = "{published}" }}
# `none` because there are no systemd units here. On a robot this restarts
# robotd/mediad — never updaterd or btd (docs/updater-design.md §4).
on_apply = {{ action = "none" }}
health = {{ probe = "socket", timeout = "5s" }}
keep_previous = 2
"#,
        keys = root.join("keys").display(),
        state = root.join("var/lib/robot/updater").display(),
        install = root.join("opt/robot/daemon").display(),
        published = root.join("published").display(),
    );
    std::fs::write(root.join("updater.toml"), config)?;

    println!("playground ready at {}", root.display());
    println!("  keys/prod.pub   trusted key (ships with the robot)");
    println!("  secret.key      signing key (in reality: a CI secret)");
    println!("  updater.toml     config");
    println!("\nnext: publish a release");
    Ok(())
}

fn publish(
    root: &Path,
    version: &str,
    hook: bool,
    bad_hook: bool,
    tamper: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let published = root.join("published");
    // `into_unencrypted_secret_key`, not `into_secret_key(None)`: the latter means
    // "encrypted key, prompt for the passphrase" and rejects an unencrypted one.
    let sk =
        minisign::SecretKeyBox::from_string(&std::fs::read_to_string(root.join("secret.key"))?)?
            .into_unencrypted_secret_key()?;

    let artifact_name = format!("daemon-{version}.tar.zst");
    let artifact = published.join(&artifact_name);

    // Build the .tar.zst the robot will install.
    {
        let out = std::fs::File::create(&artifact)?;
        let enc = zstd::Encoder::new(out, 3)?.auto_finish();
        let mut builder = tar::Builder::new(enc);

        let marker = format!("version={version}\n");
        append(&mut builder, "version.toml", marker.as_bytes(), 0o644);

        if hook || bad_hook {
            let script = if bad_hook {
                "#!/bin/sh\necho 'migration failed' >&2\nexit 1\n"
            } else {
                "#!/bin/sh\necho \"hook ran: $UPDATE_OLD_VERSION -> $UPDATE_NEW_VERSION\"\n"
            };
            append(&mut builder, "hooks/postinstall", script.as_bytes(), 0o755);
        }

        builder.finish()?;
        // The zstd frame is only completed when the encoder is dropped.
        drop(builder);
    }

    // Sign the artifact, then describe it in a manifest, then sign that too.
    let bytes = std::fs::read(&artifact)?;
    let sig = minisign::sign(None, &sk, bytes.as_slice(), None, None)?.to_string();
    std::fs::write(published.join(format!("{artifact_name}.minisig")), &sig)?;

    let manifest = serde_json::json!({
        "channel": "daemon",
        "version": version,
        "url": artifact_name,
        "sha256": sha256_hex(&bytes),
        "sig_url": format!("{artifact_name}.minisig"),
        "size": bytes.len(),
        "schema_version": 1,
        "source_revision": "playground",
        "changelog": format!("playground release {version}"),
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(
        published.join(format!("{version}.manifest.json")),
        &manifest_bytes,
    )?;
    std::fs::write(
        published.join(format!("{version}.manifest.json.minisig")),
        minisign::sign(None, &sk, manifest_bytes.as_slice(), None, None)?.to_string(),
    )?;

    if tamper {
        // Corrupt *after* signing: exactly what a tampered mirror or a truncated
        // transfer looks like. The hash check must catch it.
        let mut corrupted = std::fs::read(&artifact)?;
        corrupted.push(0xff);
        std::fs::write(&artifact, corrupted)?;
        println!("published {version} and TAMPERED with it (apply should be refused)");
    } else {
        println!("published {version} ({} bytes)", bytes.len());
    }
    Ok(())
}

fn append(builder: &mut tar::Builder<impl std::io::Write>, name: &str, body: &[u8], mode: u32) {
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(mode);
    header.set_cksum();
    builder.append_data(&mut header, name, body).unwrap();
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn engine(
    root: &Path,
    healthy: bool,
    faults: &[String],
) -> Result<Engine, Box<dyn std::error::Error>> {
    let config = Config::load(&root.join("updater.toml"))?;
    let keys = KeyRing::load(&config.trusted_keys_dir, config.allow_dev_keys)?;

    let mut injected = Faults::none();
    for name in faults {
        match name.as_str() {
            "corrupt_artifact" => injected.corrupt_artifact = true,
            "fail_post_hook" => injected.fail_post_hook = true,
            "fail_health" => injected.fail_health = true,
            "hang_health" => injected.hang_health = true,
            "abort_after_swap" => injected.abort_after_swap = true,
            "simulate_disk_full" => injected.simulate_disk_full = true,
            "fail_rollback" => injected.fail_rollback = true,
            other => return Err(format!("unknown fault: {other}").into()),
        }
    }

    Ok(Engine::new(
        config,
        keys,
        Box::new(FakeRobot { healthy }),
        injected,
    )?)
}
