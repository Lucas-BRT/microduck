//! End-to-end tests for the update state machine, over the `LocalDir` source.
//!
//! These are the Tier-1 mechanism tests from `docs/updater-design.md` §16.2: they
//! drive the **real engine code path** with no network and no robot, so they cannot
//! drift from production behaviour. This is the suite that replaces manually
//! reverting a robot, applying an update, and eyeballing the result.
//!
//! Rollback is the thing most likely to be quietly broken, because it only runs
//! when something else already went wrong — so most of these tests deliberately
//! break something.

use std::path::{Path, PathBuf};

use updater::config::Config;
use updater::engine::{ApplyOptions, Engine};
use updater::faults::Faults;
use updater::proto::{ApplyResult, CheckResult, Outcome, Target};
use updater::robot::{AbsentRobot, Health, RobotClient, SafeToRestart};
use updater::verify::KeyRing;

// ── fixture ──────────────────────────────────────────────────────────────────

/// A robot that answers exactly what a test wants. The reason [`RobotClient`] is a
/// trait: degraded paths must be testable without staging a real crash.
struct FakeRobot {
    healthy: bool,
    model_api: Option<u32>,
}

impl FakeRobot {
    fn healthy() -> Self {
        Self {
            healthy: true,
            model_api: Some(1),
        }
    }
    fn unhealthy() -> Self {
        Self {
            healthy: false,
            model_api: Some(1),
        }
    }
}

/// `robotd` is down *now* (so its model API is unknown and nothing is moving), but
/// comes up healthy once a good release is linked.
///
/// This is the realistic recovery scenario, and distinct from [`AbsentRobot`],
/// which models a robot that never comes up at all — where rolling back wouldn't
/// help either.
struct DeadThenHealthy;

#[async_trait::async_trait]
impl RobotClient for DeadThenHealthy {
    async fn safe_to_restart(&self, _t: std::time::Duration) -> SafeToRestart {
        SafeToRestart::Unreachable
    }
    async fn health(&self, _t: std::time::Duration) -> Health {
        Health::Healthy
    }
    async fn model_api(&self, _t: std::time::Duration) -> Option<u32> {
        None
    }
    async fn remote_session_active(&self, _t: std::time::Duration) -> bool {
        false
    }
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
            Health::Unhealthy("motors not responding".into())
        }
    }
    async fn model_api(&self, _t: std::time::Duration) -> Option<u32> {
        self.model_api
    }
    async fn remote_session_active(&self, _t: std::time::Duration) -> bool {
        false
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    releases: PathBuf,
    install: PathBuf,
    keypair: minisign::KeyPair,
    /// Kept so a test can compare or restore the trusted key.
    #[allow(dead_code)]
    public_key: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let releases = root.join("published");
        let install = root.join("opt/robot/daemon");
        std::fs::create_dir_all(&releases).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        // Created up front so a test can swap the trusted key before building an
        // engine.
        std::fs::create_dir_all(root.join("keys")).unwrap();

        let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = keypair
            .pk
            .to_box()
            .unwrap()
            .to_string()
            .lines()
            .next_back()
            .unwrap()
            .to_owned();

        std::fs::write(root.join("keys/prod.pub"), &public_key).unwrap();

        Self {
            _dir: dir,
            root,
            releases,
            install,
            keypair,
            public_key,
        }
    }

    fn sign(&self, data: &[u8]) -> Vec<u8> {
        minisign::sign(None, &self.keypair.sk, data, None, None)
            .unwrap()
            .to_string()
            .into_bytes()
    }

    /// Publish a signed release into the fake "remote" directory.
    ///
    /// `hook` optionally embeds a `hooks/postinstall` script in the artifact.
    fn publish(&self, version: &str, hook: Option<&str>) {
        self.publish_with(version, hook, |_| {});
    }

    /// As [`Self::publish`], but lets the caller mutate the manifest before it is
    /// signed — for compatibility and floor tests.
    fn publish_with(
        &self,
        version: &str,
        hook: Option<&str>,
        edit: impl FnOnce(&mut serde_json::Value),
    ) {
        let artifact_name = format!("daemon-{version}.tar.zst");
        let artifact = self.releases.join(&artifact_name);

        // Build the .tar.zst.
        let out = std::fs::File::create(&artifact).unwrap();
        let enc = zstd::Encoder::new(out, 1).unwrap().auto_finish();
        let mut builder = tar::Builder::new(enc);

        let marker = format!("version={version}\n");
        append(&mut builder, "version.toml", marker.as_bytes(), 0o644);
        if let Some(script) = hook {
            append(&mut builder, "hooks/postinstall", script.as_bytes(), 0o755);
        }
        builder.finish().unwrap();
        drop(builder); // completes the zstd frame

        let bytes = std::fs::read(&artifact).unwrap();
        std::fs::write(
            self.releases.join(format!("{artifact_name}.minisig")),
            self.sign(&bytes),
        )
        .unwrap();

        let mut manifest = serde_json::json!({
            "channel": "daemon",
            "version": version,
            "url": artifact_name,
            "sha256": sha256_hex(&bytes),
            "sig_url": format!("{artifact_name}.minisig"),
            "size": bytes.len(),
            "schema_version": 1,
        });
        edit(&mut manifest);

        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_path = self.releases.join(format!("{version}.manifest.json"));
        std::fs::write(&manifest_path, &manifest_bytes).unwrap();
        std::fs::write(
            self.releases
                .join(format!("{version}.manifest.json.minisig")),
            self.sign(&manifest_bytes),
        )
        .unwrap();
    }

    /// Where model releases are published. Separate from the daemon's remote: one
    /// shared directory would let the daemon's `latest` resolve to a model manifest.
    fn model_releases(&self) -> PathBuf {
        let dir = self.root.join("published-model");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Publish a signed release on the `model` channel, for multi-component tests.
    fn publish_model(&self, version: &str) {
        let releases = self.model_releases();
        let artifact_name = format!("model-{version}.tar.zst");
        let artifact = releases.join(&artifact_name);

        let out = std::fs::File::create(&artifact).unwrap();
        let enc = zstd::Encoder::new(out, 1).unwrap().auto_finish();
        let mut builder = tar::Builder::new(enc);
        append(&mut builder, "walk.onnx", b"weights", 0o644);
        builder.finish().unwrap();
        drop(builder);

        let bytes = std::fs::read(&artifact).unwrap();
        std::fs::write(
            releases.join(format!("{artifact_name}.minisig")),
            self.sign(&bytes),
        )
        .unwrap();

        let manifest = serde_json::json!({
            "channel": "model",
            "version": version,
            "url": artifact_name,
            "sha256": sha256_hex(&bytes),
            "sig_url": format!("{artifact_name}.minisig"),
            "size": bytes.len(),
            "schema_version": 1,
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        std::fs::write(
            releases.join(format!("{version}.manifest.json")),
            &manifest_bytes,
        )
        .unwrap();
        std::fs::write(
            releases.join(format!("{version}.manifest.json.minisig")),
            self.sign(&manifest_bytes),
        )
        .unwrap();
    }

    /// Remove a published release from the fake remote, so `latest` resolves to an
    /// older one — a stale or reverted mirror.
    fn unpublish(&self, version: &str) {
        for name in [
            format!("{version}.manifest.json"),
            format!("{version}.manifest.json.minisig"),
            format!("daemon-{version}.tar.zst"),
            format!("daemon-{version}.tar.zst.minisig"),
        ] {
            let _ = std::fs::remove_file(self.releases.join(name));
        }
    }

    /// Is a boot-counter trial outstanding?
    fn pending_file_exists(&self) -> bool {
        self.root
            .join("var/lib/robot/updater/pending.json")
            .exists()
    }

    /// Corrupt a published artifact *after* signing, simulating a tampered mirror
    /// or a truncated transfer.
    fn tamper(&self, version: &str) {
        let path = self.releases.join(format!("daemon-{version}.tar.zst"));
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0xff);
        std::fs::write(&path, bytes).unwrap();
    }

    /// Keys are written once at construction, never here — otherwise a test that
    /// swaps the trusted key would have it silently restored on the next
    /// `engine()` call.
    fn config(&self, extra: &str) -> Config {
        let keys_dir = self.root.join("keys");

        Config::from_toml(&format!(
            r#"
trusted_keys_dir = "{keys}"
hw_rev = 1
state_dir = "{state}"

[component.daemon]
install_dir = "{install}"
source = {{ type = "local_dir", path = "{published}" }}
on_apply = {{ action = "none" }}
health = {{ probe = "socket", timeout = "2s" }}
{extra}
"#,
            keys = keys_dir.display(),
            state = self.root.join("var/lib/robot/updater").display(),
            install = self.install.display(),
            published = self.releases.display(),
        ))
        .expect("fixture config must be valid")
    }

    fn engine(&self, robot: Box<dyn RobotClient>, faults: Faults, extra: &str) -> Engine {
        let config = self.config(extra);
        let keys = KeyRing::load(&config.trusted_keys_dir, config.allow_dev_keys).unwrap();
        Engine::new(config, keys, robot, faults).unwrap()
    }

    fn engine_healthy(&self) -> Engine {
        self.engine(Box::new(FakeRobot::healthy()), Faults::none(), "")
    }

    /// Version the `current` symlink points at.
    fn live_version(&self) -> Option<String> {
        let target = std::fs::read_link(self.install.join("current")).ok()?;
        Some(target.file_name()?.to_str()?.to_owned())
    }

    /// Reads the marker file through the symlink — proves the *content* switched,
    /// not merely the link.
    fn live_marker(&self) -> Option<String> {
        std::fs::read_to_string(self.install.join("current/version.toml")).ok()
    }

    fn release_exists(&self, version: &str) -> bool {
        self.install.join("releases").join(version).is_dir()
    }

    fn staging_leftovers(&self) -> usize {
        let dir = self.install.join("releases");
        std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
                    .count()
            })
            .unwrap_or(0)
    }
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
    let digest = Sha256::digest(bytes);
    digest.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Drains progress so senders never block, and returns the phases seen.
fn progress_channel() -> (
    updater::engine::ProgressTx,
    tokio::sync::mpsc::UnboundedReceiver<updater::proto::Progress>,
) {
    tokio::sync::mpsc::unbounded_channel()
}

async fn apply_exact(engine: &mut Engine, version: &str) -> Result<ApplyResult, updater::Error> {
    let (tx, _rx) = progress_channel();
    engine
        .apply(
            "daemon",
            Target::Exact(semver::Version::parse(version).unwrap()),
            ApplyOptions::default(),
            tx,
        )
        .await
}

async fn apply_latest(engine: &mut Engine) -> Result<ApplyResult, updater::Error> {
    let (tx, _rx) = progress_channel();
    engine
        .apply("daemon", Target::Latest, ApplyOptions::default(), tx)
        .await
}

// ── happy path ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn installs_from_scratch_and_reports_progress() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();

    let (tx, mut rx) = progress_channel();
    let result = engine
        .apply("daemon", Target::Latest, ApplyOptions::default(), tx)
        .await
        .unwrap();

    assert!(matches!(result, ApplyResult::Applied { from: None, .. }));
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
    assert_eq!(fx.live_marker().as_deref(), Some("version=1.0.0\n"));

    // The app's progress bar depends on these arriving in order.
    let mut phases = Vec::new();
    while let Ok(p) = rx.try_recv() {
        phases.push(p.phase);
    }
    use updater::proto::Phase;
    for expected in [
        Phase::Checking,
        Phase::Preflight,
        Phase::Verifying,
        Phase::Swapping,
        Phase::HealthGate,
        Phase::Committing,
    ] {
        assert!(
            phases.contains(&expected),
            "missing {expected:?} in {phases:?}"
        );
    }
}

#[tokio::test]
async fn upgrades_and_keeps_previous_for_rollback() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    fx.publish("1.1.0", None);
    let result = apply_latest(&mut engine).await.unwrap();

    match result {
        ApplyResult::Applied { from, to } => {
            assert_eq!(from, Some(semver::Version::new(1, 0, 0)));
            assert_eq!(to, semver::Version::new(1, 1, 0));
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert_eq!(fx.live_version().as_deref(), Some("1.1.0"));
    assert!(
        fx.release_exists("1.0.0"),
        "previous must be kept for rollback"
    );
}

#[tokio::test]
async fn applying_the_same_version_is_a_no_op() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    let result = apply_latest(&mut engine).await.unwrap();
    assert!(matches!(result, ApplyResult::AlreadyCurrent { .. }));
}

#[tokio::test]
async fn applies_an_exact_version_not_just_latest() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.publish("2.0.0", None);
    let mut engine = fx.engine_healthy();

    let (tx, _rx) = progress_channel();
    engine
        .apply(
            "daemon",
            Target::Exact(semver::Version::new(1, 0, 0)),
            ApplyOptions::default(),
            tx,
        )
        .await
        .unwrap();

    // This is the primitive that makes release testing scriptable.
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
}

#[tokio::test]
async fn check_reports_availability_without_changing_anything() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let engine = fx.engine_healthy();

    let result = engine.check("daemon").await.unwrap();
    assert!(matches!(
        result,
        CheckResult::Available {
            installed: None,
            ..
        }
    ));
    assert_eq!(fx.live_version(), None, "check must not install anything");
}

#[tokio::test]
async fn dry_run_verifies_everything_but_does_not_swap() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();

    let (tx, _rx) = progress_channel();
    let result = engine
        .apply(
            "daemon",
            Target::Latest,
            ApplyOptions {
                dry_run: true,
                ..Default::default()
            },
            tx,
        )
        .await
        .unwrap();

    assert!(matches!(result, ApplyResult::DryRunPassed { .. }));
    assert_eq!(fx.live_version(), None, "dry run must not swap");
    assert_eq!(
        fx.staging_leftovers(),
        0,
        "dry run must clean up after itself"
    );
}

// ── refusals: nothing changes ────────────────────────────────────────────────

#[tokio::test]
async fn refuses_tampered_artifact() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.tamper("1.0.0");

    let mut engine = fx.engine_healthy();
    let err = apply_latest(&mut engine).await.unwrap_err();

    assert!(
        matches!(err, updater::Error::Verification(_)),
        "got {err:?}"
    );
    assert_eq!(fx.live_version(), None, "nothing may be installed");
    assert_eq!(fx.staging_leftovers(), 0);
}

#[tokio::test]
async fn refuses_artifact_signed_by_an_untrusted_key() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);

    // Replace the trusted key with an unrelated one.
    let other = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let other_pk = other
        .pk
        .to_box()
        .unwrap()
        .to_string()
        .lines()
        .next_back()
        .unwrap()
        .to_owned();
    std::fs::write(fx.root.join("keys/prod.pub"), other_pk).unwrap();

    let mut engine = fx.engine_healthy();
    let err = apply_latest(&mut engine).await.unwrap_err();
    assert!(
        matches!(err, updater::Error::Verification(_)),
        "got {err:?}"
    );
    assert_eq!(fx.live_version(), None);
}

#[tokio::test]
async fn refuses_release_requiring_newer_hardware() {
    let fx = Fixture::new();
    fx.publish_with("2.0.0", None, |m| {
        m["min_hw_rev"] = serde_json::json!(9);
    });

    let mut engine = fx.engine_healthy();
    let err = apply_latest(&mut engine).await.unwrap_err();
    assert!(
        matches!(err, updater::Error::Incompatible(_)),
        "got {err:?}"
    );
    assert_eq!(fx.live_version(), None);
}

#[tokio::test]
async fn refuses_manifest_from_the_wrong_channel() {
    let fx = Fixture::new();
    fx.publish_with("1.0.0", None, |m| {
        m["channel"] = serde_json::json!("model");
    });

    let mut engine = fx.engine_healthy();
    let err = apply_latest(&mut engine).await.unwrap_err();
    // A misconfigured URL must not be able to install a model as the daemon.
    assert!(
        matches!(err, updater::Error::Incompatible(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn refuses_when_pinned_to_another_version() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.publish("1.1.0", None);

    let mut engine = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults::none(),
        r#"pinned = "1.0.0""#,
    );
    let err = apply_latest(&mut engine).await.unwrap_err();
    assert!(
        matches!(err, updater::Error::Incompatible(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn refuses_when_disk_is_full() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);

    let mut engine = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults {
            simulate_disk_full: true,
            ..Faults::none()
        },
        "",
    );
    let err = apply_latest(&mut engine).await.unwrap_err();
    assert!(matches!(err, updater::Error::Preflight(_)), "got {err:?}");
    assert_eq!(fx.live_version(), None, "must abort before downloading");
}

// ── rollback ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rolls_back_when_the_new_release_is_unhealthy() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);

    // Install 1.0.0 with a healthy robot.
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    // Now 1.1.0 comes up sick.
    fx.publish("1.1.0", None);
    let mut engine = fx.engine(Box::new(FakeRobot::unhealthy()), Faults::none(), "");
    let result = apply_latest(&mut engine).await.unwrap();

    match result {
        ApplyResult::RolledBack {
            attempted,
            reverted_to,
            ..
        } => {
            assert_eq!(attempted, semver::Version::new(1, 1, 0));
            assert_eq!(reverted_to, Some(semver::Version::new(1, 0, 0)));
        }
        other => panic!("expected RolledBack, got {other:?}"),
    }

    // The robot must be running the *old* release, content and all.
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
    assert_eq!(fx.live_marker().as_deref(), Some("version=1.0.0\n"));
}

#[tokio::test]
async fn rolls_back_when_the_post_install_hook_fails() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    // A hook that fails is exactly as fatal as a failed health probe.
    fx.publish(
        "1.1.0",
        Some("#!/bin/sh\necho migration failed >&2\nexit 1\n"),
    );
    let result = apply_latest(&mut engine).await.unwrap();

    assert!(
        matches!(result, ApplyResult::RolledBack { .. }),
        "{result:?}"
    );
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
}

#[tokio::test]
async fn a_passing_hook_does_not_block_the_update() {
    let fx = Fixture::new();
    fx.publish("1.0.0", Some("#!/bin/sh\nexit 0\n"));
    let mut engine = fx.engine_healthy();

    let result = apply_latest(&mut engine).await.unwrap();
    assert!(matches!(result, ApplyResult::Applied { .. }), "{result:?}");
}

#[tokio::test]
async fn rolls_back_when_the_health_probe_hangs() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    fx.publish("1.1.0", None);
    // Socket open, no reply — the case that stalls naive clients. A timeout must
    // count as failure: unproven is not healthy.
    let mut engine = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults {
            hang_health: true,
            ..Faults::none()
        },
        "",
    );
    let result = apply_latest(&mut engine).await.unwrap();
    assert!(
        matches!(result, ApplyResult::RolledBack { .. }),
        "{result:?}"
    );
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
}

/// The worst case: the update failed *and* recovery failed. It must surface
/// distinctly, not as an ordinary failure.
#[tokio::test]
async fn failed_rollback_is_reported_distinctly() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    fx.publish("1.1.0", None);
    let mut engine = fx.engine(
        Box::new(FakeRobot::unhealthy()),
        Faults {
            fail_rollback: true,
            ..Faults::none()
        },
        "",
    );
    let err = apply_latest(&mut engine).await.unwrap_err();

    assert!(
        matches!(err, updater::Error::RollbackFailed(_)),
        "got {err:?}"
    );
    assert_eq!(
        err.code(),
        updater::proto::code::ROLLBACK_FAILED,
        "support must be able to spot this immediately"
    );
}

// ── crash recovery ───────────────────────────────────────────────────────────

/// Simulates `kill -9` right after the symlink swap: the new release is live and
/// the boot counter is still armed. The in-process health gate cannot help here —
/// it died with the process — so the boot counter must catch it.
#[tokio::test]
async fn crash_after_swap_is_reverted_by_boot_counter() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    fx.publish("1.1.0", None);
    let mut crashing = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults {
            abort_after_swap: true,
            ..Faults::none()
        },
        "",
    );
    let _ = apply_latest(&mut crashing).await;

    // Post-crash state: 1.1.0 is live but never confirmed healthy.
    assert_eq!(fx.live_version().as_deref(), Some("1.1.0"));

    // Restarts: the first is still "on trial", the second exhausts the budget.
    let mut engine = fx.engine_healthy();
    assert!(engine.recover_on_start().await.unwrap().is_empty());
    let recovered = engine.recover_on_start().await.unwrap();

    assert_eq!(recovered.len(), 1, "should have reverted");
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
    assert_eq!(fx.live_marker().as_deref(), Some("version=1.0.0\n"));
}

#[tokio::test]
async fn recovery_confirms_a_healthy_update_and_does_nothing() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    // A committed update leaves no pending trial, so restarts are no-ops.
    for _ in 0..3 {
        assert!(engine.recover_on_start().await.unwrap().is_empty());
    }
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
}

#[tokio::test]
async fn recovery_cleans_staging_leftovers() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    // Fake an interrupted extract.
    let leftover = fx.install.join("releases/.staging-9.9.9/root");
    std::fs::create_dir_all(&leftover).unwrap();
    assert_eq!(fx.staging_leftovers(), 1);

    engine.recover_on_start().await.unwrap();
    assert_eq!(
        fx.staging_leftovers(),
        0,
        "a kill -9 must not leak disk forever"
    );
}

// ── explicit transitions ─────────────────────────────────────────────────────

#[tokio::test]
async fn explicit_rollback_returns_to_the_previous_release() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.publish("1.1.0", None);
    let mut engine = fx.engine_healthy();

    let (tx, _rx) = progress_channel();
    engine
        .apply(
            "daemon",
            Target::Exact(semver::Version::new(1, 0, 0)),
            ApplyOptions::default(),
            tx,
        )
        .await
        .unwrap();
    apply_latest(&mut engine).await.unwrap();
    assert_eq!(fx.live_version().as_deref(), Some("1.1.0"));

    let result = engine.rollback("daemon").await.unwrap();
    assert!(matches!(result, ApplyResult::Applied { .. }), "{result:?}");
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
}

#[tokio::test]
async fn reset_to_golden_works_when_robotd_is_dead() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.publish("1.1.0", None);

    // Install both so golden is on disk.
    let mut engine = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults::none(),
        r#"golden = "1.0.0""#,
    );
    let (tx, _rx) = progress_channel();
    engine
        .apply(
            "daemon",
            Target::Exact(semver::Version::new(1, 0, 0)),
            ApplyOptions::default(),
            tx,
        )
        .await
        .unwrap();
    apply_latest(&mut engine).await.unwrap();
    assert_eq!(fx.live_version().as_deref(), Some("1.1.0"));

    // Now robotd is down. Recovery must still work — this is the whole point of the
    // updater not depending on the thing it repairs. It comes back up once golden
    // is linked, which is what the gate then observes.
    let mut engine = fx.engine(
        Box::new(DeadThenHealthy),
        Faults::none(),
        r#"golden = "1.0.0""#,
    );
    let result = engine.reset_to_golden("daemon").await.unwrap();

    assert!(matches!(result, ApplyResult::Applied { .. }), "{result:?}");
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
}

/// A robot that never comes up is a different case: the gate cannot pass, so the
/// engine must report a rollback rather than claim success. Recovery has to be
/// honest about not having worked.
#[tokio::test]
async fn robot_that_never_comes_up_is_reported_not_papered_over() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.publish("1.1.0", None);

    let mut engine = fx.engine_healthy();
    let (tx, _rx) = progress_channel();
    engine
        .apply(
            "daemon",
            Target::Exact(semver::Version::new(1, 0, 0)),
            ApplyOptions::default(),
            tx,
        )
        .await
        .unwrap();
    apply_latest(&mut engine).await.unwrap();

    let mut engine = fx.engine(Box::new(AbsentRobot), Faults::none(), r#"golden = "1.0.0""#);
    let result = engine.reset_to_golden("daemon").await.unwrap();

    assert!(
        matches!(result, ApplyResult::RolledBack { .. }),
        "unreachable health must fail the gate, got {result:?}"
    );
}

#[tokio::test]
async fn select_activates_an_installed_release() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.publish("1.1.0", None);
    let mut engine = fx.engine_healthy();

    let (tx, _rx) = progress_channel();
    engine
        .apply(
            "daemon",
            Target::Exact(semver::Version::new(1, 0, 0)),
            ApplyOptions::default(),
            tx,
        )
        .await
        .unwrap();
    apply_latest(&mut engine).await.unwrap();

    let result = engine
        .select("daemon", &semver::Version::new(1, 0, 0))
        .await
        .unwrap();
    assert!(matches!(result, ApplyResult::Applied { .. }), "{result:?}");
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
}

#[tokio::test]
async fn select_refuses_a_version_that_is_not_installed() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    assert!(
        engine
            .select("daemon", &semver::Version::new(9, 9, 9))
            .await
            .is_err()
    );
}

// ── bookkeeping ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn prune_keeps_the_configured_number_of_previous_releases() {
    let fx = Fixture::new();
    let mut engine = fx.engine_healthy();

    for v in ["1.0.0", "1.1.0", "1.2.0", "1.3.0"] {
        fx.publish(v, None);
        apply_latest(&mut engine).await.unwrap();
    }

    // keep_previous defaults to 1, so only the live release plus one survive.
    let installed = engine.list_installed("daemon").unwrap();
    assert_eq!(installed.len(), 2, "{installed:?}");
    assert!(installed.iter().any(|r| r.active));
}

#[tokio::test]
async fn golden_is_never_pruned() {
    let fx = Fixture::new();
    let mut engine = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults::none(),
        r#"golden = "1.0.0""#,
    );

    for v in ["1.0.0", "1.1.0", "1.2.0", "1.3.0"] {
        fx.publish(v, None);
        apply_latest(&mut engine).await.unwrap();
    }

    assert!(
        fx.release_exists("1.0.0"),
        "the never-brick guarantee must not expire as versions accumulate"
    );
    let installed = engine.list_installed("daemon").unwrap();
    assert!(installed.iter().any(|r| r.golden));
}

#[tokio::test]
async fn log_records_success_rollback_and_refusal() {
    let fx = Fixture::new();

    // Success.
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    // Refusal (tampered).
    fx.publish("1.1.0", None);
    fx.tamper("1.1.0");
    let _ = apply_latest(&mut engine).await;

    // Rollback (unhealthy).
    fx.publish("1.2.0", None);
    let mut sick = fx.engine(Box::new(FakeRobot::unhealthy()), Faults::none(), "");
    let _ = apply_latest(&mut sick).await;

    let log = engine.log(10).unwrap();
    let kinds: Vec<_> = log
        .iter()
        .map(|e| match &e.outcome {
            Outcome::Success => "success",
            Outcome::RolledBack { .. } => "rolled_back",
            Outcome::Aborted { .. } => "aborted",
        })
        .collect();

    // Newest first. All three classes must be present — a log of only successes is
    // useless to support.
    assert!(kinds.contains(&"success"), "{kinds:?}");
    assert!(kinds.contains(&"aborted"), "{kinds:?}");
    assert!(kinds.contains(&"rolled_back"), "{kinds:?}");
}

#[tokio::test]
async fn concurrent_updates_report_busy_rather_than_colliding() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);

    let mut first = fx.engine_healthy();
    let mut second = fx.engine_healthy();

    // Hold the lock by starting an update that blocks in the health gate.
    let mut blocking = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults {
            hang_health: true,
            ..Faults::none()
        },
        "",
    );

    let (tx, _rx) = progress_channel();
    let handle = tokio::spawn(async move {
        blocking
            .apply("daemon", Target::Latest, ApplyOptions::default(), tx)
            .await
    });

    // Give it time to take the lock and reach the gate.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let err = apply_latest(&mut second).await.unwrap_err();
    assert!(matches!(err, updater::Error::Busy), "got {err:?}");

    let _ = handle.await;
    // And once it's released, the lock is usable again.
    assert!(apply_latest(&mut first).await.is_ok());
}

#[tokio::test]
async fn unknown_component_is_rejected() {
    let fx = Fixture::new();
    let engine = fx.engine_healthy();
    assert!(matches!(
        engine.check("nope").await.unwrap_err(),
        updater::Error::UnknownComponent(_)
    ));
}

#[tokio::test]
async fn mandatory_flag_is_surfaced_by_check() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    // 1.2.0 declares that anything below it must upgrade.
    fx.publish_with("1.2.0", None, |m| {
        m["min_supported"] = serde_json::json!("1.2.0");
    });

    match engine.check("daemon").await.unwrap() {
        CheckResult::Available { mandatory, .. } => {
            assert!(mandatory, "floor must make this non-optional");
        }
        other => panic!("expected Available, got {other:?}"),
    }
}

/// The embedded manifest gives provenance without a network round-trip.
#[tokio::test]
async fn installed_release_reports_source_revision() {
    let fx = Fixture::new();
    fx.publish_with("1.0.0", None, |m| {
        m["source_revision"] = serde_json::json!("abc1234");
    });
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    let installed = engine.list_installed("daemon").unwrap();
    assert_eq!(
        installed[0].source_revision.as_deref(),
        Some("abc1234"),
        "needed to reproduce a client's exact build in the lab"
    );
}

fn _unused(_: &Path) {}

// ── regressions ──────────────────────────────────────────────────────────────
//
// One test per bug found in review. Each reproduces the original failure, so a
// regression shows up as a specific named failure rather than a vague one.

/// **#1** `rollback` walked *forward* onto the release that had just failed.
///
/// After an auto-rollback the bad release is still on disk — the failure path
/// deliberately doesn't prune — so "newest that isn't current" picked it. That made
/// the one command support reaches for after a bad update reinstall the bad update.
#[tokio::test]
async fn rollback_does_not_walk_forward_onto_the_failed_release() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    // 1.1.0 fails its gate and is auto-rolled-back, but stays on disk.
    fx.publish("1.1.0", None);
    let mut sick = fx.engine(Box::new(FakeRobot::unhealthy()), Faults::none(), "");
    let result = apply_latest(&mut sick).await.unwrap();
    assert!(
        matches!(result, ApplyResult::RolledBack { .. }),
        "{result:?}"
    );
    assert!(
        fx.release_exists("1.1.0"),
        "precondition: bad release still present"
    );
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));

    // An explicit rollback must not land back on 1.1.0. With only 1.0.0 below it and
    // 1.0.0 already live, there is nowhere older to go.
    let err = engine.rollback("daemon").await.unwrap_err();
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"), "must not move");
    assert!(
        err.to_string().contains("no older, known-good release"),
        "got: {err}"
    );
}

/// **#1b** Rollback skips a release the journal recorded as rolled back, even when
/// it is the newest one below current.
#[tokio::test]
async fn rollback_skips_known_bad_releases() {
    let fx = Fixture::new();
    for v in ["1.0.0", "1.1.0", "1.2.0"] {
        fx.publish(v, None);
    }
    // keep_previous is raised so the older good release survives pruning — the
    // point of the test is which target rollback *chooses*, not what prune keeps.
    let keep = "keep_previous = 5";
    let mut engine = fx.engine(Box::new(FakeRobot::healthy()), Faults::none(), keep);

    // Install 1.0.0 then 1.2.0, so 1.1.0 can be installed-but-bad in between.
    apply_exact(&mut engine, "1.0.0").await.unwrap();

    // 1.1.0 fails: recorded as rolled back, left on disk.
    let mut sick = fx.engine(Box::new(FakeRobot::unhealthy()), Faults::none(), keep);
    let result = apply_exact(&mut sick, "1.1.0").await.unwrap();
    assert!(
        matches!(result, ApplyResult::RolledBack { .. }),
        "{result:?}"
    );

    apply_exact(&mut engine, "1.2.0").await.unwrap();
    assert_eq!(fx.live_version().as_deref(), Some("1.2.0"));

    // Newest below current is 1.1.0, but it is known-bad, so 1.0.0 wins.
    engine.rollback("daemon").await.unwrap();
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
}

/// **#3** An unrecoverable trial was re-reported on every boot, appending a bogus
/// rollback entry each time and never converging.
#[tokio::test]
async fn unrecoverable_first_install_reports_stuck_once() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);

    // First install, no previous release, no golden — and it crashes after the swap.
    let mut crashing = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults {
            abort_after_swap: true,
            ..Faults::none()
        },
        "",
    );
    let _ = apply_latest(&mut crashing).await;

    let mut engine = fx.engine_healthy();
    assert!(
        engine.recover_on_start().await.unwrap().is_empty(),
        "on trial"
    );

    // Budget exhausted: reported as Stuck, because nothing was reverted. Calling it
    // a rollback would be a lie.
    let outcomes = engine.recover_on_start().await.unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(&outcomes[0], ApplyResult::Stuck { .. }),
        "got {:?}",
        outcomes[0]
    );

    // And it must not be re-reported forever.
    for _ in 0..3 {
        assert!(
            engine.recover_on_start().await.unwrap().is_empty(),
            "a stuck update must be reported once, not on every boot"
        );
    }
    assert!(
        !fx.pending_file_exists(),
        "the trial must be cleared so it stops repeating"
    );
}

/// **#4** The boot counter was a single global slot, so any other component's
/// transition destroyed a daemon update's trial — losing exactly the record the
/// never-brick guarantee rests on.
#[tokio::test]
async fn a_second_component_does_not_consume_the_daemon_trial() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.publish_model("3.0.0");

    let extra = format!(
        r#"
[component.model]
install_dir = "{}"
source = {{ type = "local_dir", path = "{}" }}
on_apply = {{ action = "none" }}
health = {{ probe = "none" }}
"#,
        fx.root.join("opt/robot/model").display(),
        fx.model_releases().display(),
    );

    // Daemon: install 1.0.0, then crash after the swap of 1.1.0.
    let mut engine = fx.engine(Box::new(FakeRobot::healthy()), Faults::none(), &extra);
    apply_latest(&mut engine).await.unwrap();

    fx.publish("1.1.0", None);
    let mut crashing = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults {
            abort_after_swap: true,
            ..Faults::none()
        },
        &extra,
    );
    let _ = apply_latest(&mut crashing).await;
    assert_eq!(fx.live_version().as_deref(), Some("1.1.0"));

    // Now the model updates successfully. Under the old single-slot counter this
    // silently cleared the daemon's trial.
    let mut engine = fx.engine(Box::new(FakeRobot::healthy()), Faults::none(), &extra);
    let (tx, _rx) = progress_channel();
    engine
        .apply("model", Target::Latest, ApplyOptions::default(), tx)
        .await
        .unwrap();

    // The daemon trial must have survived, and still revert.
    assert!(
        engine.recover_on_start().await.unwrap().is_empty(),
        "on trial"
    );
    let outcomes = engine.recover_on_start().await.unwrap();
    assert_eq!(outcomes.len(), 1, "daemon trial was lost: {outcomes:?}");
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
}

/// **#5** `transition_to` armed the boot counter before validating the target, so a
/// failed swap left a trial for a version that was never live — self-healing later
/// only via a spurious rollback and a bogus log entry.
#[tokio::test]
async fn failed_transition_leaves_no_armed_trial() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    // Golden is configured but was never installed.
    let mut engine = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults::none(),
        r#"golden = "9.9.9""#,
    );
    let err = engine.reset_to_golden("daemon").await.unwrap_err();

    assert_eq!(
        err.code(),
        updater::proto::code::NOT_INSTALLED,
        "got {err:?}"
    );
    assert!(
        !fx.pending_file_exists(),
        "a refused transition must not leave a trial armed"
    );
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));

    // And a later boot must not invent a recovery from it.
    assert!(engine.recover_on_start().await.unwrap().is_empty());
}

/// **#7** `select` on a version that isn't installed reported UNKNOWN_COMPONENT,
/// telling clients the wrong thing — the component exists, the version doesn't.
#[tokio::test]
async fn select_missing_version_reports_not_installed() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    let err = engine
        .select("daemon", &semver::Version::new(9, 9, 9))
        .await
        .unwrap_err();
    assert_eq!(err.code(), updater::proto::code::NOT_INSTALLED);
    assert_ne!(err.code(), updater::proto::code::UNKNOWN_COMPONENT);

    // A genuinely unknown component still reports UNKNOWN_COMPONENT.
    let err = engine
        .select("nope", &semver::Version::new(1, 0, 0))
        .await
        .unwrap_err();
    assert_eq!(err.code(), updater::proto::code::UNKNOWN_COMPONENT);
}

/// **#8** Nothing refused a downgrade, so a stale or reverted mirror serving an old
/// but still-validly-signed manifest would walk the fleet backwards — the classic
/// rollback attack on a signed-artifact scheme.
#[tokio::test]
async fn latest_refuses_to_downgrade() {
    let fx = Fixture::new();
    fx.publish("2.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    // The mirror reverts to only offering 1.0.0 — properly signed, just old.
    fx.unpublish("2.0.0");
    fx.publish("1.0.0", None);

    let err = apply_latest(&mut engine).await.unwrap_err();
    assert_eq!(
        err.code(),
        updater::proto::code::WOULD_DOWNGRADE,
        "got {err:?}"
    );
    assert_eq!(fx.live_version().as_deref(), Some("2.0.0"));

    // `check` reports it rather than offering it as an available update.
    match engine.check("daemon").await.unwrap() {
        CheckResult::Incompatible { reason, .. } => {
            assert!(reason.contains("downgrade"), "got: {reason}");
        }
        other => panic!("expected Incompatible, got {other:?}"),
    }
}

/// **#8b** An *explicit* older version is still allowed: that is how a targeted
/// revert works, and it is a deliberate operator action rather than something a
/// mirror can induce.
#[tokio::test]
async fn exact_version_may_still_go_backwards() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.publish("2.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();
    assert_eq!(fx.live_version().as_deref(), Some("2.0.0"));

    apply_exact(&mut engine, "1.0.0").await.unwrap();
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
}

// ── §16.2 Tier-1 gaps closed ────────────────────────────────────────────────

/// **§5.7** Robot-specific state must survive an update *and* a rollback.
///
/// Called out as Tier-1 in §16.2 and previously untested. This is the promise that
/// a client never has to recalibrate because of an update.
#[tokio::test]
async fn robot_specific_state_survives_update_and_rollback() {
    let fx = Fixture::new();

    // State that belongs to *this* robot, deliberately outside any release dir.
    let etc = fx.root.join("etc/robot");
    let var = fx.root.join("var/lib/robot");
    std::fs::create_dir_all(&etc).unwrap();
    std::fs::create_dir_all(&var).unwrap();
    std::fs::write(etc.join("imu_calibration.bin"), b"calibration").unwrap();
    std::fs::write(var.join("maploc_map.bin"), b"learned-map").unwrap();
    std::fs::write(etc.join("prefs.toml"), b"name = \"Ducky\"\n").unwrap();

    let intact = |stage: &str| {
        assert_eq!(
            std::fs::read(etc.join("imu_calibration.bin")).unwrap(),
            b"calibration",
            "calibration lost after {stage}"
        );
        assert_eq!(
            std::fs::read(var.join("maploc_map.bin")).unwrap(),
            b"learned-map",
            "learned state lost after {stage}"
        );
        assert_eq!(
            std::fs::read(etc.join("prefs.toml")).unwrap(),
            b"name = \"Ducky\"\n",
            "user prefs lost after {stage}"
        );
    };

    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();
    intact("first install");

    fx.publish("1.1.0", None);
    apply_latest(&mut engine).await.unwrap();
    intact("upgrade");

    engine.rollback("daemon").await.unwrap();
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
    intact("rollback");

    // And through a failed update, which is when a naive implementation would clean
    // up too aggressively.
    fx.publish("1.2.0", None);
    let mut sick = fx.engine(Box::new(FakeRobot::unhealthy()), Faults::none(), "");
    let result = apply_latest(&mut sick).await.unwrap();
    assert!(
        matches!(result, ApplyResult::RolledBack { .. }),
        "{result:?}"
    );
    intact("auto-rollback");
}

/// **§9** A `schema_version` bump runs the hook with both old and new versions, so a
/// migration can tell which transition it is in.
#[tokio::test]
async fn schema_version_bump_gives_the_hook_both_versions() {
    let fx = Fixture::new();
    let migrated = fx.root.join("etc/robot/migrated.txt");
    std::fs::create_dir_all(migrated.parent().unwrap()).unwrap();

    fx.publish_with("1.0.0", None, |m| {
        m["schema_version"] = serde_json::json!(1);
    });
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    // 1.1.0 bumps the schema and migrates, recording what it saw.
    let hook = format!(
        "#!/bin/sh\nprintf '%s->%s\\n' \"$UPDATE_OLD_SCHEMA_VERSION\" \
         \"$UPDATE_NEW_SCHEMA_VERSION\" > {}\n",
        migrated.display()
    );
    fx.publish_with("1.1.0", Some(&hook), |m| {
        m["schema_version"] = serde_json::json!(2);
    });

    apply_latest(&mut engine).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(&migrated).unwrap().trim(),
        "1->2",
        "the hook must see both schema versions to know which migration to run"
    );
}

/// A schema bump must be **installable**, not refused.
///
/// Gating on `schema_version` looks prudent and is self-defeating: the engine judging
/// a manifest is always the previous release's engine, so refusing a higher schema
/// would make every bump undeliverable — including the release carrying the engine
/// that understands it. The hook does the migration; the engine only passes the
/// numbers through.
#[tokio::test]
async fn a_schema_bump_is_deliverable() {
    let fx = Fixture::new();
    fx.publish_with("2.0.0", None, |m| {
        m["schema_version"] = serde_json::json!(99);
    });

    let mut engine = fx.engine_healthy();
    let result = apply_latest(&mut engine).await.unwrap();
    assert!(matches!(result, ApplyResult::Applied { .. }), "{result:?}");
    assert_eq!(fx.live_version().as_deref(), Some("2.0.0"));
}

/// A pin survives being read back, and blocks an otherwise-valid update.
#[tokio::test]
async fn pin_persists_and_blocks_updates() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    engine
        .pin("daemon", Some(semver::Version::new(1, 0, 0)))
        .await
        .unwrap();

    fx.publish("1.1.0", None);
    let err = apply_latest(&mut engine).await.unwrap_err();
    assert!(err.to_string().contains("pinned"), "got {err}");

    // Visible in status, and it outlives this Engine instance.
    let fresh = fx.engine_healthy();
    let status = fresh.status().await.unwrap();
    assert_eq!(status[0].pinned, Some(semver::Version::new(1, 0, 0)));

    // Unpinning lets the update through.
    engine.pin("daemon", None).await.unwrap();
    apply_latest(&mut engine).await.unwrap();
    assert_eq!(fx.live_version().as_deref(), Some("1.1.0"));
}

/// A pin nothing can satisfy would be an invisible update freeze.
#[tokio::test]
async fn pin_refuses_an_unobtainable_version() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    let mut engine = fx.engine_healthy();
    apply_latest(&mut engine).await.unwrap();

    let err = engine
        .pin("daemon", Some(semver::Version::new(7, 7, 7)))
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        updater::proto::code::NOT_INSTALLED,
        "got {err:?}"
    );
}

/// Recovery must escalate to golden when the recorded `previous` is no longer on
/// disk, or a robot whose previous release has been pruned never reaches golden
/// (§8.2).
#[tokio::test]
async fn recovery_escalates_to_golden_when_previous_is_gone() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.publish("1.1.0", None);
    let golden = "golden = \"1.0.0\"";

    let mut engine = fx.engine(Box::new(FakeRobot::healthy()), Faults::none(), golden);
    apply_exact(&mut engine, "1.0.0").await.unwrap();
    apply_exact(&mut engine, "1.1.0").await.unwrap();

    // Crash after swapping to a third release, so `previous` is recorded as 1.1.0.
    fx.publish("1.2.0", None);
    let mut crashing = fx.engine(
        Box::new(FakeRobot::healthy()),
        Faults {
            abort_after_swap: true,
            ..Faults::none()
        },
        golden,
    );
    let _ = apply_latest(&mut crashing).await;

    // Now 1.1.0 disappears (pruned, or a corrupted directory removed by hand).
    std::fs::remove_dir_all(fx.install.join("releases/1.1.0")).unwrap();

    let mut engine = fx.engine(Box::new(FakeRobot::healthy()), Faults::none(), golden);
    engine.recover_on_start().await.unwrap();
    engine.recover_on_start().await.unwrap();

    assert_eq!(
        fx.live_version().as_deref(),
        Some("1.0.0"),
        "recovery should escalate to golden when previous is gone"
    );
}

/// **Journal `to` semantics.** Three sites wrote rollback entries and disagreed about
/// what `to` meant: `apply` recorded the version that *failed*, while `select` /
/// `rollback` / `reset-to-golden` / `recover_on_start` recorded the version landed
/// *on*. `known_bad` reads that field, so the second group blacklisted the
/// known-**good** release — and a later `rollback` then refused to land on it.
///
/// No test covered a *failing* select, which is why it survived.
#[tokio::test]
async fn a_failed_select_does_not_blacklist_the_release_it_reverted_to() {
    let fx = Fixture::new();
    fx.publish("1.0.0", None);
    fx.publish("1.1.0", None);
    let keep = "keep_previous = 5";

    let mut engine = fx.engine(Box::new(FakeRobot::healthy()), Faults::none(), keep);
    apply_exact(&mut engine, "1.0.0").await.unwrap();
    apply_exact(&mut engine, "1.1.0").await.unwrap();
    assert_eq!(fx.live_version().as_deref(), Some("1.1.0"));

    // Select back to 1.0.0, but the robot comes up unhealthy, so it reverts to 1.1.0.
    let mut sick = fx.engine(Box::new(FakeRobot::unhealthy()), Faults::none(), keep);
    let result = sick
        .select("daemon", &semver::Version::new(1, 0, 0))
        .await
        .unwrap();
    match &result {
        ApplyResult::RolledBack {
            attempted,
            reverted_to,
            ..
        } => {
            assert_eq!(*attempted, semver::Version::new(1, 0, 0));
            assert_eq!(*reverted_to, Some(semver::Version::new(1, 1, 0)));
        }
        other => panic!("expected RolledBack, got {other:?}"),
    }
    assert_eq!(fx.live_version().as_deref(), Some("1.1.0"));

    // 1.1.0 is the release we are *running* and it is healthy. It must not have been
    // recorded as bad.
    let bad = engine.known_bad("daemon");
    assert!(
        !bad.contains(&semver::Version::new(1, 1, 0)),
        "the release we reverted TO must not be blacklisted; known_bad = {bad:?}"
    );
    assert!(
        bad.contains(&semver::Version::new(1, 0, 0)),
        "the release that FAILED should be blacklisted; known_bad = {bad:?}"
    );
}

/// The same disagreement, seen through its user-visible consequence: `rollback` must
/// still be able to land on a release that a failed `select` merely reverted to.
#[tokio::test]
async fn rollback_still_works_after_a_failed_select() {
    let fx = Fixture::new();
    for v in ["1.0.0", "1.1.0", "1.2.0"] {
        fx.publish(v, None);
    }
    let keep = "keep_previous = 5";

    // All three must be *installed* for `select` to be able to reach 1.1.0 —
    // publishing alone would make select return NotInstalled instead.
    let mut engine = fx.engine(Box::new(FakeRobot::healthy()), Faults::none(), keep);
    apply_exact(&mut engine, "1.0.0").await.unwrap();
    apply_exact(&mut engine, "1.1.0").await.unwrap();
    apply_exact(&mut engine, "1.2.0").await.unwrap();

    // A failed select to 1.1.0 reverts to 1.2.0.
    let mut sick = fx.engine(Box::new(FakeRobot::unhealthy()), Faults::none(), keep);
    let _ = sick.select("daemon", &semver::Version::new(1, 1, 0)).await;
    assert_eq!(fx.live_version().as_deref(), Some("1.2.0"));

    // Rolling back must reach 1.0.0. Under the bug, 1.2.0 was blacklisted (harmless
    // here, it's current) *and* the entry claimed 1.2.0 rolled back, which is the
    // wrong story for support to read.
    engine.rollback("daemon").await.unwrap();
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));

    // And the log must attribute the failure to 1.1.0, not to 1.2.0.
    let log = engine.log(10).unwrap();
    let rollback_entry = log
        .iter()
        .find(|e| matches!(e.outcome, Outcome::RolledBack { .. }))
        .expect("a rollback should be recorded");
    assert_eq!(
        rollback_entry.to,
        Some(semver::Version::new(1, 1, 0)),
        "a RolledBack entry's `to` must name the version that failed"
    );
}
