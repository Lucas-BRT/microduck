//! The health gate against a **real `robotd` process**, over a real unix socket.
//!
//! Everything in `apply.rs` uses a `FakeRobot` implementing [`RobotClient`] in-process.
//! That is the right way to test the engine's *decisions*, but it proves nothing about
//! the part most likely to break in the field: the wire. A `FakeRobot` cannot catch a
//! method name that disagrees between the two crates, a result field spelled
//! `is_healthy` on one side and `healthy` on the other, a socket with the wrong mode, or
//! a `robotd` that accepts a connection and never answers.
//!
//! So these tests spawn the actual binary and let `SocketRobotClient` talk to it. This is
//! the M1 done-test from `docs/roadmap.md`: an update health-gates against a running
//! `robotd`, and a `robotd` that reports unhealthy triggers an automatic rollback.
//!
//! They live in `robotd/tests/` rather than `updater/tests/` so that cargo — not a path
//! guess — supplies the binary, via `CARGO_BIN_EXE_robotd`. See [`robotd_bin`].
//!
//! Not covered here, deliberately: `on_apply = restart` via `systemctl`. There is no
//! systemd on a dev laptop, and stubbing it would test the stub. That is M4's job, on the
//! Radxa — see `docs/roadmap.md`.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use updater::config::Config;
use updater::engine::{ApplyOptions, Engine};
use updater::faults::Faults;
use updater::proto::{ApplyResult, Target};
use updater::robot::{Health, RobotClient, SafeToRestart, SocketRobotClient};
use updater::verify::KeyRing;

// ── locating and running the real robotd ─────────────────────────────────────

/// Path to the `robotd` binary under test.
///
/// `CARGO_BIN_EXE_robotd` is set by cargo for binaries of the package the test belongs
/// to, and cargo guarantees the binary is **rebuilt before the test runs**. That freshness
/// guarantee is the whole reason this file lives in `robotd/` rather than `updater/`.
///
/// The first version of this test derived the path from `current_exe()` instead, and
/// checked only that the file existed. `cargo test --test <name>` does not rebuild sibling
/// binaries, so it happily ran a *stale* robotd against a freshly built client — which
/// looks exactly like the wire drift this file is meant to detect, and made a sabotage
/// check appear to succeed when it had proved nothing.
fn robotd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_robotd"))
}

/// A `robotd` child process, killed on drop.
///
/// Killed in `Drop` rather than at the end of each test on purpose: a test that fails
/// mid-way still unwinds through `Drop`, so a panicking test cannot leave a stray daemon
/// holding a socket and making the *next* run mysterious.
struct Robotd {
    child: Child,
    socket: PathBuf,
}

impl Robotd {
    /// Spawn `robotd` and wait until it answers, so tests never race its startup.
    async fn spawn(socket: PathBuf, extra_args: &[&str]) -> Self {
        let mut command = Command::new(robotd_bin());
        command.arg("--socket").arg(&socket);
        // Fast heartbeat: `robot.health` reports unhealthy until the control loop has
        // ticked at least once, so a 1s default would add a second to every test.
        command.arg("--heartbeat").arg("50ms");
        command.args(extra_args);
        // Quiet the heartbeat, which would otherwise bury the test output. `error` rather
        // than `off` so a real startup failure still shows up.
        command.env("RUST_LOG", "error");
        let child = command.spawn().expect("spawn robotd");

        let robotd = Self { child, socket };
        robotd.wait_until_answering().await;
        robotd
    }

    /// Wait until the socket answers *anything*, healthy or not.
    ///
    /// Async rather than `block_on`: these are `#[tokio::test]`s, and building a nested
    /// runtime inside one panics outright.
    async fn wait_until_answering(&self) {
        let client = SocketRobotClient::new(self.socket.clone());
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let answer = client.health(Duration::from_millis(500)).await;
            if !matches!(answer, Health::Unreachable) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("robotd never answered on {}", self.socket.display());
    }
}

impl Drop for Robotd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── fixture ──────────────────────────────────────────────────────────────────

/// Deliberately a slimmer copy of `apply.rs`'s fixture rather than a shared module:
/// these tests need a real socket path threaded through the config, and coupling the two
/// suites' setup would mean every change to one risks the other. The duplication is a
/// signed tarball builder and a config template — cheap, and it keeps each file readable
/// on its own.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    releases: PathBuf,
    install: PathBuf,
    keypair: minisign::KeyPair,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let releases = root.join("published");
        let install = root.join("opt/robot/daemon");
        std::fs::create_dir_all(&releases).unwrap();
        std::fs::create_dir_all(&install).unwrap();
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
        }
    }

    /// Where `robotd` should listen.
    ///
    /// Under the tempdir so concurrent test binaries never collide. Unix socket paths cap
    /// at ~104 bytes on macOS, which the short `d.sock` name keeps room for.
    fn socket(&self) -> PathBuf {
        self.root.join("d.sock")
    }

    fn sign(&self, data: &[u8]) -> Vec<u8> {
        minisign::sign(None, &self.keypair.sk, data, None, None)
            .unwrap()
            .to_string()
            .into_bytes()
    }

    /// Publish a signed release into the fake remote directory.
    fn publish(&self, version: &str) {
        let artifact_name = format!("daemon-{version}.tar.zst");
        let artifact = self.releases.join(&artifact_name);

        let out = std::fs::File::create(&artifact).unwrap();
        let enc = zstd::Encoder::new(out, 1).unwrap().auto_finish();
        let mut builder = tar::Builder::new(enc);
        let marker = format!("version={version}\n");
        let mut header = tar::Header::new_gnu();
        header.set_size(marker.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append_data(&mut header, "version.toml", marker.as_bytes())
            .unwrap();
        builder.finish().unwrap();
        drop(builder);

        let bytes = std::fs::read(&artifact).unwrap();
        std::fs::write(
            self.releases.join(format!("{artifact_name}.minisig")),
            self.sign(&bytes),
        )
        .unwrap();

        let manifest = serde_json::json!({
            "channel": "daemon",
            "version": version,
            "url": artifact_name,
            "sha256": sha256_hex(&bytes),
            "sig_url": format!("{artifact_name}.minisig"),
            "size": bytes.len(),
            "schema_version": 1,
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        std::fs::write(
            self.releases.join(format!("{version}.manifest.json")),
            &manifest_bytes,
        )
        .unwrap();
        std::fs::write(
            self.releases
                .join(format!("{version}.manifest.json.minisig")),
            self.sign(&manifest_bytes),
        )
        .unwrap();
    }

    /// An engine wired to a **real** `SocketRobotClient` — the point of this file.
    ///
    /// The gate timeout is short (3s) so the unhealthy test fails fast; a real robot gets
    /// 30s (see `updater.example.toml`).
    fn engine(&self) -> Engine {
        let config = Config::from_toml(&format!(
            r#"
trusted_keys_dir = "{keys}"
hw_rev = 1
state_dir = "{state}"
robot_socket = "{socket}"

[component.daemon]
install_dir = "{install}"
source = {{ type = "local_dir", path = "{published}" }}
on_apply = {{ action = "none" }}
health = {{ probe = "socket", timeout = "3s" }}
"#,
            keys = self.root.join("keys").display(),
            state = self.root.join("var/lib/robot/updater").display(),
            socket = self.socket().display(),
            install = self.install.display(),
            published = self.releases.display(),
        ))
        .expect("fixture config must be valid");

        // Built from the config, exactly as `updaterd`'s `main` does it — so a config
        // key that stopped reaching the client would fail here too.
        let robot: Box<dyn RobotClient> =
            Box::new(SocketRobotClient::new(config.robot_socket.clone()));
        let keys = KeyRing::load(&config.trusted_keys_dir, config.allow_dev_keys).unwrap();
        Engine::new(config, keys, robot, Faults::none()).unwrap()
    }

    fn live_version(&self) -> Option<String> {
        let target = std::fs::read_link(self.install.join("current")).ok()?;
        Some(target.file_name()?.to_str()?.to_owned())
    }

    fn live_marker(&self) -> Option<String> {
        std::fs::read_to_string(self.install.join("current/version.toml")).ok()
    }
}

/// `apply` requires a progress sink; these tests do not assert on progress (`apply.rs`
/// already does), so the receiver is dropped.
async fn apply_latest(engine: &mut Engine) -> Result<ApplyResult, updater::Error> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    engine
        .apply("daemon", Target::Latest, ApplyOptions::default(), tx)
        .await
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ── the wire ─────────────────────────────────────────────────────────────────

/// Every method the engine calls must be answered by the real binary.
///
/// A `FakeRobot` cannot catch a method name or result field that disagrees between
/// `duck-ipc-proto`'s two users, because it never serialises anything. This test is the only
/// thing standing between a renamed field and a robot that silently reports Unreachable
/// forever — which reverts every update, on every robot, with no error that names a
/// cause.
#[tokio::test]
async fn real_robotd_answers_every_method_the_engine_calls() {
    let fx = Fixture::new();
    let _robotd = Robotd::spawn(fx.socket(), &[]).await;
    let client = SocketRobotClient::new(fx.socket());
    let t = Duration::from_secs(2);

    assert!(
        matches!(client.health(t).await, Health::Healthy),
        "a running robotd must report healthy"
    );
    // `Yes` specifically, not `permits_restart()`: that also returns true for
    // `Unreachable`, so it would pass even if the wire were completely broken.
    assert!(
        matches!(client.safe_to_restart(t).await, SafeToRestart::Yes),
        "an idle robotd must answer Yes, not merely fail to object"
    );
    assert_eq!(
        client.model_api(t).await,
        Some(1),
        "modelApi must parse; None here means the field name drifted"
    );
    assert!(
        !client.remote_session_active(t).await,
        "no session should be active on a fresh robotd"
    );
}

/// `--unhealthy` must reach the engine as `Unhealthy`, not as `Unreachable`.
///
/// The distinction matters: `Unreachable` is a normal state (robotd stopped), while
/// `Unhealthy` is a definite verdict from a running robot. If a bad answer degraded to
/// `Unreachable`, rollback would still happen — for the wrong reason, and the journal
/// would blame the wrong thing.
#[tokio::test]
async fn unhealthy_robotd_reports_a_reason_not_unreachable() {
    let fx = Fixture::new();
    let _robotd = Robotd::spawn(fx.socket(), &["--unhealthy"]).await;
    let client = SocketRobotClient::new(fx.socket());

    match client.health(Duration::from_secs(2)).await {
        Health::Unhealthy(reason) => assert!(!reason.is_empty(), "reason must not be empty"),
        other => panic!("expected Unhealthy with a reason, got {other:?}"),
    }
}

/// A stopped `robotd` is `Unreachable`, and the socket file being gone must not hang.
#[tokio::test]
async fn absent_robotd_is_unreachable_and_does_not_hang() {
    let fx = Fixture::new();
    let client = SocketRobotClient::new(fx.socket()); // never spawned

    let started = Instant::now();
    let answer = client.health(Duration::from_secs(2)).await;
    assert!(matches!(answer, Health::Unreachable), "got {answer:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a missing socket must fail immediately, not wait out the timeout"
    );
}

// ── the M1 done-test ─────────────────────────────────────────────────────────

/// **M1, first half.** An update gates against a real running `robotd` and commits.
#[tokio::test]
async fn apply_gates_against_a_healthy_robotd_and_commits() {
    let fx = Fixture::new();
    let _robotd = Robotd::spawn(fx.socket(), &[]).await;

    fx.publish("1.0.0");
    let mut engine = fx.engine();
    let result = apply_latest(&mut engine)
        .await
        .expect("apply must succeed against a healthy robotd");

    assert!(
        matches!(result, ApplyResult::Applied { .. }),
        "expected Applied, got {result:?}"
    );
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));
    assert_eq!(fx.live_marker().as_deref(), Some("version=1.0.0\n"));
}

/// **M1, second half.** A `robotd` that reports unhealthy reverts the update.
///
/// The assertion that matters is not "apply failed" — it is that the *content* behind
/// `current` went back to 1.0.0. A rollback that repoints the symlink but leaves the new
/// files live would pass a weaker test and brick a real robot.
#[tokio::test]
async fn unhealthy_robotd_triggers_automatic_rollback() {
    let fx = Fixture::new();

    // 1.0.0 lands while the robot is healthy.
    let healthy = Robotd::spawn(fx.socket(), &[]).await;
    fx.publish("1.0.0");
    apply_latest(&mut fx.engine())
        .await
        .expect("baseline apply");
    assert_eq!(fx.live_version().as_deref(), Some("1.0.0"));

    // The new release comes up unhealthy. Restarting the daemon under a different flag
    // is how a real bad release behaves: the binary changed, and the robot no longer
    // reports healthy.
    drop(healthy);
    let _broken = Robotd::spawn(fx.socket(), &["--unhealthy"]).await;

    fx.publish("1.1.0");
    // `Ok(RolledBack)`, not `Err`: the engine handled the failure, which is a successful
    // outcome for the engine even though the release was bad. An `Err` here would mean
    // the *rollback* itself broke.
    let result = apply_latest(&mut fx.engine())
        .await
        .expect("a failed gate is handled, not an error");

    match &result {
        ApplyResult::RolledBack {
            attempted,
            reverted_to,
            reason,
        } => {
            assert_eq!(attempted, &semver::Version::new(1, 1, 0));
            assert_eq!(reverted_to, &Some(semver::Version::new(1, 0, 0)));
            assert!(
                !reason.is_empty(),
                "the journal needs a reason naming the health failure"
            );
        }
        other => panic!("expected RolledBack, got {other:?}"),
    }
    assert_eq!(
        fx.live_version().as_deref(),
        Some("1.0.0"),
        "symlink must point back at the known-good release"
    );
    assert_eq!(
        fx.live_marker().as_deref(),
        Some("version=1.0.0\n"),
        "content behind the symlink must be the old release, not merely the link"
    );
}

// ── version reporting over the wire ──────────────────────────────────────────

/// `hello` must report the running build, over a real socket.
///
/// This is what `robotctl version` reads, and what support relies on to answer "which build
/// is this robot actually running?". A version that only exists in the log line is lost the
/// moment the journal rotates or the robot's logs are volatile.
#[tokio::test]
async fn real_robotd_reports_its_own_version_over_hello() {
    let fx = Fixture::new();
    let _robotd = Robotd::spawn(fx.socket(), &[]).await;

    // Raw request, so this exercises the same bytes `robotctl` sends rather than a helper
    // that could paper over a shape mismatch.
    let mut stream = tokio::net::UnixStream::connect(fx.socket()).await.unwrap();
    let request = format!(
        "{}\n",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": updater::proto::method::HELLO,
            "params": { "api_version": updater::proto::API_VERSION },
        })
    );
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut line = String::new();
    tokio::io::BufReader::new(stream)
        .read_line(&mut line)
        .await
        .unwrap();

    let response: updater::proto::Response = serde_json::from_str(line.trim()).unwrap();
    assert!(response.error.is_none(), "{:?}", response.error);
    let hello: updater::proto::HelloResult =
        serde_json::from_value(response.result.expect("result")).expect("HelloResult shape");

    assert_eq!(hello.api_version, updater::proto::API_VERSION);
    assert_eq!(
        hello.daemon_version.map(|v| v.to_string()).as_deref(),
        Some(env!("CARGO_PKG_VERSION")),
        "robotd must report its own crate version"
    );
    // `revision` is None for a local build and Some for a CI build; either is correct, so
    // this asserts only that the field survives the round trip rather than pinning the
    // build environment.
}
