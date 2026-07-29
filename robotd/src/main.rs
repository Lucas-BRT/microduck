//! `robotd` — the robot control daemon.
//!
//! **This is a skeleton.** It logs a heartbeat and answers the four questions
//! `updaterd` asks. No motors, no kinematics, no gait. Motor control arrives in M3
//! (`docs/roadmap.md`), developed against the MuJoCo sim before hardware.
//!
//! It exists now for one reason: without it, `updaterd` has nothing to health-gate
//! against, so `updater.example.toml` had to ship with `health = none` and **no
//! auto-rollback** (`updater-design.md` §16.5). That is the weakest the update design
//! gets, and this removes it.
//!
//! The four methods are deliberately the whole API surface:
//!
//! | method | why `updaterd` asks |
//! |---|---|
//! | `robot.safeToRestart` | don't restart motor control mid-motion |
//! | `robot.health` | the post-update gate — did the new release come up? |
//! | `robot.modelApi` | can this daemon load a given model bundle? |
//! | `robot.remoteSessionActive` | don't drop a telepresence session |
//!
//! Every one of them must be answerable *while the robot is in a bad state*, since that
//! is exactly when it is being asked. So this server holds no locks, does no IO beyond
//! the socket, and never blocks on the control loop — a wedged control loop must show up
//! as `healthy: false`, not as a hung socket.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use clap::Parser;
use robot_proto as proto;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Model API version this build implements (`updater-design.md` §5.5). Bump when the
/// sensor-input / actuator-output contract a model sees changes.
const MODEL_API: u32 = 1;

/// Socket mode. Same reasoning as `updaterd`'s: the group decides who may ask.
const SOCKET_MODE: u32 = 0o660;

const MAX_LINE: usize = 64 * 1024;

#[derive(Parser, Debug)]
#[command(name = "robotd", about = "Robot control daemon (skeleton)", version)]
struct Args {
    /// Socket to serve the `robot.*` API on. `updaterd --robot-socket` must match.
    #[arg(long, default_value = "/run/robotd.sock")]
    socket: PathBuf,

    /// Heartbeat interval.
    #[arg(long, default_value = "1s", value_parser = parse_duration)]
    heartbeat: Duration,

    /// Report unhealthy. For exercising the updater's rollback path on a bench robot
    /// without having to break a real build.
    #[arg(long)]
    unhealthy: bool,

    /// Report that it is not safe to restart, as if the robot were moving.
    #[arg(long)]
    busy: bool,
}

fn parse_duration(raw: &str) -> Result<Duration, String> {
    let (value, scale) = match raw.strip_suffix("ms") {
        Some(v) => (v, 1u64),
        None => match raw.strip_suffix('s') {
            Some(v) => (v, 1000),
            None => (raw, 1000),
        },
    };
    value
        .parse::<u64>()
        .map(|n| Duration::from_millis(n * scale))
        .map_err(|_| format!("expected e.g. 500ms or 2s, got {raw:?}"))
}

/// What the control loop publishes about itself.
///
/// Atomics rather than a mutex on purpose: the IPC side must never be able to block on
/// the control loop. A robot whose control loop is wedged still has to be able to say
/// "I am not healthy" — if answering required the loop's lock, the one situation where
/// `updaterd` needs an answer is the situation it would hang in.
struct RobotState {
    /// Set once the loop has completed a cycle; the health gate's basic question.
    running: AtomicBool,
    /// Heartbeats since start. Lets a caller see progress, not just liveness.
    ticks: AtomicU64,
    /// Forced-unhealthy, for testing rollback.
    force_unhealthy: bool,
    /// Forced-busy, for testing the safe-to-restart refusal.
    force_busy: bool,
}

impl RobotState {
    fn health(&self) -> proto::HealthResult {
        if self.force_unhealthy {
            return proto::HealthResult {
                healthy: false,
                reason: Some("forced unhealthy by --unhealthy".into()),
            };
        }
        // A loop that has not completed a cycle yet is not healthy. During the health
        // gate this is the honest answer — "starting" is not "started" — and the gate
        // polls, so it will see the transition.
        if !self.running.load(Ordering::Relaxed) {
            return proto::HealthResult {
                healthy: false,
                reason: Some("control loop has not completed a cycle yet".into()),
            };
        }
        proto::HealthResult {
            healthy: true,
            reason: None,
        }
    }

    fn safe_to_restart(&self) -> proto::SafeToRestartResult {
        if self.force_busy {
            return proto::SafeToRestartResult {
                safe: false,
                reason: Some("forced busy by --busy".into()),
            };
        }
        // The skeleton never moves, so it is always safe. When real control lands this
        // must consult actual motion state — restarting mid-stride is how a robot falls
        // over (`updater-design.md` §7.2).
        proto::SafeToRestartResult {
            safe: true,
            reason: None,
        }
    }
}

/// The first line each daemon logs, before anything that can fail.
///
/// Support's opening question is always "what was running?", and three facts answer it
/// that a version number alone does not:
///
///   - **`exe`** — which release directory this process was launched from. After an update
///     `updaterd` is still running the *previous* binary by design (it never restarts
///     itself, `updater-design.md` §4.1), so `current` pointing at 0.3.0 while this line
///     says `releases/0.2.0` is normal and needs to be visible rather than deduced.
///   - **`revision`** — which commit. Two dev builds of the same version are otherwise
///     indistinguishable, which is the normal case once branch installs land (M2).
///   - **`pid`** — so a restart is distinguishable from a long-running process in a journal
///     that spans reboots.
///
/// Logged at `warn` rather than `info`: the identity of the running build must survive
/// `RUST_LOG=warn`, which is what a robot left running for weeks should be set to. It is
/// one line per process start.
fn log_startup_identity(service: &str) {
    tracing::warn!(
        service,
        build = %robot_proto::build_info!(),
        exe = %std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into()),
        pid = std::process::id(),
        "starting"
    );
}

#[tokio::main]
async fn main() -> ExitCode {
    // Rust ignores SIGPIPE, which turns `robotd ... | head` into a panic.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    log_startup_identity("robotd");

    let state = Arc::new(RobotState {
        running: AtomicBool::new(false),
        ticks: AtomicU64::new(0),
        force_unhealthy: args.unhealthy,
        force_busy: args.busy,
    });

    if args.unhealthy {
        tracing::warn!("--unhealthy: will report unhealthy, so updates will roll back");
    }
    if args.busy {
        tracing::warn!("--busy: will refuse restarts, so updates will be held off");
    }

    let control = tokio::spawn(control_loop(Arc::clone(&state), args.heartbeat));

    let serving = serve(Arc::clone(&state), args.socket.clone());
    tokio::select! {
        result = serving => {
            if let Err(e) = result {
                tracing::error!(error = %e, "IPC server stopped");
                return ExitCode::FAILURE;
            }
        }
        _ = shutdown() => tracing::info!("shutting down"),
    }

    control.abort();
    let _ = std::fs::remove_file(&args.socket);
    ExitCode::SUCCESS
}

/// Stands in for the control loop. M3 replaces the body, not the shape.
/// How often the loop logs a summary at `info`.
///
/// The per-tick line is at `debug`, because the shipped unit runs at `info` and a 1s
/// heartbeat would be ~86k journal lines a day from an idle robot. That is not merely
/// noise: under a journal size cap it is what *evicts* the logs support needs. One summary
/// every five minutes is ~288 lines a day and says strictly more.
const LOOP_SUMMARY_INTERVAL: Duration = Duration::from_secs(300);

/// How the control loop is actually keeping up, as a percentage of its target rate.
///
/// Separated out to be testable, and because it is the number M4 needs: on a non-RT kernel
/// the interesting question is not "is the loop running" but "is it running *on time*".
/// A loop at 60% of target is alive, passing its health check, and badly broken.
fn achieved_percent(ticks: u64, elapsed: Duration, interval: Duration) -> Option<u32> {
    let expected = elapsed.as_secs_f64() / interval.as_secs_f64();
    if expected < 1.0 {
        return None;
    }
    Some((ticks as f64 / expected * 100.0).round() as u32)
}

async fn control_loop(state: Arc<RobotState>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    // Skipped ticks must not be replayed in a burst: a loop that fell behind should
    // continue at its target rate, not sprint to catch up.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut last_summary = tokio::time::Instant::now();
    let mut ticks_at_last_summary = 0u64;

    loop {
        ticker.tick().await;
        let n = state.ticks.fetch_add(1, Ordering::Relaxed) + 1;
        // Only after a cycle completes, so `health` cannot claim readiness before the
        // loop has actually run once.
        state.running.store(true, Ordering::Relaxed);
        tracing::debug!(tick = n, "hello from robotd");
        if n == 1 {
            // One line at `info` the moment the loop is alive. Without it, a developer
            // running robotd by hand at the default level sees nothing between "starting"
            // and the first five-minute summary, which reads like a hang.
            tracing::info!(interval = ?interval, "control loop running");
        }

        let elapsed = last_summary.elapsed();
        if elapsed >= LOOP_SUMMARY_INTERVAL {
            let ticks = n - ticks_at_last_summary;
            tracing::info!(
                ticks,
                total = n,
                achieved_percent = achieved_percent(ticks, elapsed, interval),
                "control loop"
            );
            last_summary = tokio::time::Instant::now();
            ticks_at_last_summary = n;
        }
    }
}

async fn serve(state: Arc<RobotState>, socket_path: PathBuf) -> std::io::Result<()> {
    // A leftover socket from a killed process must not stop us coming up.
    if socket_path.exists() {
        tracing::warn!(path = %socket_path.display(), "removing stale socket");
        let _ = std::fs::remove_file(&socket_path);
    }
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let listener = UnixListener::bind(&socket_path)?;

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(SOCKET_MODE))?;

    tracing::info!(
        path = %socket_path.display(),
        mode = format!("{SOCKET_MODE:o}"),
        model_api = MODEL_API,
        "serving robot IPC"
    );

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle(state, stream).await {
                tracing::debug!(error = %e, "connection ended");
            }
        });
    }
}

async fn handle(state: Arc<RobotState>, stream: UnixStream) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_LINE {
            let response = proto::Response::err(
                None,
                proto::Error::new(proto::code::INVALID_REQUEST, "request too large"),
            );
            write_line(&mut write_half, &response).await?;
            continue;
        }

        let request: proto::Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                let response = proto::Response::err(
                    None,
                    proto::Error::new(proto::code::PARSE_ERROR, e.to_string()),
                );
                write_line(&mut write_half, &response).await?;
                continue;
            }
        };

        // Notifications get no reply, per the spec.
        let Some(id) = request.id.clone() else {
            continue;
        };

        let response = dispatch(&state, id, &request.method);
        write_line(&mut write_half, &response).await?;
    }
    Ok(())
}

/// Answer one request.
///
/// Synchronous and allocation-light on purpose: these answers must be available even
/// when everything else is broken.
fn dispatch(state: &RobotState, id: proto::Id, method: &str) -> proto::Response {
    let ok = |value: serde_json::Value| proto::Response {
        jsonrpc: proto::JSONRPC_VERSION.to_owned(),
        id: Some(id.clone()),
        result: Some(value),
        error: None,
    };

    let encode = |value: &dyn erased::Any| value.to_json();

    match method {
        proto::method::ROBOT_HEALTH => ok(encode(&state.health())),
        proto::method::ROBOT_SAFE_TO_RESTART => ok(encode(&state.safe_to_restart())),
        proto::method::ROBOT_MODEL_API => ok(encode(&proto::ModelApiResult {
            model_api: MODEL_API,
        })),
        // The skeleton has no media stack, so no session can be live. `mediad` owns the
        // real answer (architecture.md §5.2); reporting `false` here is honest for now,
        // and the updater treats unknown as false anyway.
        proto::method::ROBOT_SESSION_ACTIVE => {
            ok(encode(&proto::SessionActiveResult { active: false }))
        }
        // Typed rather than hand-built JSON, for the same reason the `robot.*` results
        // are: a hand-built object is where a field name drifts from what the client
        // parses, and nothing catches it until a robot reports the wrong thing.
        proto::method::HELLO => ok(encode(&proto::HelloResult {
            api_version: proto::API_VERSION,
            daemon_version: proto::semver::Version::parse(env!("CARGO_PKG_VERSION")).ok(),
            revision: proto::build_info!().revision.map(str::to_owned),
        })),
        other => proto::Response::err(
            Some(id),
            proto::Error::new(
                proto::code::METHOD_NOT_FOUND,
                format!("unknown method {other:?}"),
            ),
        ),
    }
}

/// Tiny shim so `dispatch` can serialise several concrete result types through one
/// closure without `dyn Serialize` (which isn't dyn-compatible).
mod erased {
    pub trait Any {
        fn to_json(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Any for T {
        fn to_json(&self) -> serde_json::Value {
            // These types are plain structs of bools/strings/ints; serialisation cannot
            // fail. Null would be a visible wrong answer rather than a silent one.
            serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
        }
    }
}

async fn write_line<T: serde::Serialize>(
    out: &mut tokio::net::unix::OwnedWriteHalf,
    message: &T,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(message)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    out.write_all(&line).await?;
    out.flush().await
}

/// Resolve on SIGTERM (systemd stop) or SIGINT (Ctrl-C).
async fn shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for SIGTERM");
            return std::future::pending().await;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(unhealthy: bool, busy: bool) -> RobotState {
        RobotState {
            running: AtomicBool::new(false),
            ticks: AtomicU64::new(0),
            force_unhealthy: unhealthy,
            force_busy: busy,
        }
    }

    /// Before the loop has run, health must be false. Claiming readiness early would let
    /// an update commit against a robot that never actually started.
    #[test]
    fn not_healthy_until_the_loop_has_ticked() {
        let s = state(false, false);
        assert!(!s.health().healthy);
        assert!(s.health().reason.unwrap().contains("not completed a cycle"));

        s.running.store(true, Ordering::Relaxed);
        assert!(s.health().healthy);
        assert!(s.health().reason.is_none());
    }

    /// Every method must come back off `dispatch` in the shape the updater parses.
    ///
    /// The other health tests call `state.health()` directly, which type-checks but says
    /// nothing about what goes over the socket — `dispatch` could return a completely
    /// different JSON shape and they would all still pass. (Verified: it can, and they do.)
    /// `tests/updater_gate.rs` catches that against a live process, but this runs in
    /// microseconds and fails on the exact method, so it is the first line of defence.
    #[test]
    fn dispatch_answers_every_method_in_the_typed_shape() {
        let s = state(false, false);
        s.running.store(true, Ordering::Relaxed);
        let id = || proto::Id::Number(1);

        let health: proto::HealthResult = dispatch(&s, id(), proto::method::ROBOT_HEALTH)
            .result_as()
            .expect("robot.health must deserialize as HealthResult");
        assert!(health.healthy);

        let safe: proto::SafeToRestartResult =
            dispatch(&s, id(), proto::method::ROBOT_SAFE_TO_RESTART)
                .result_as()
                .expect("robot.safeToRestart must deserialize as SafeToRestartResult");
        assert!(safe.safe);

        let session: proto::SessionActiveResult =
            dispatch(&s, id(), proto::method::ROBOT_SESSION_ACTIVE)
                .result_as()
                .expect("robot.remoteSessionActive must deserialize as SessionActiveResult");
        assert!(!session.active);
    }

    /// `--unhealthy` must win over a running loop: it exists to exercise rollback.
    #[test]
    fn forced_unhealthy_overrides_a_running_loop() {
        let s = state(true, false);
        s.running.store(true, Ordering::Relaxed);
        assert!(!s.health().healthy);
        assert!(s.health().reason.unwrap().contains("--unhealthy"));
    }

    #[test]
    fn safe_to_restart_unless_forced_busy() {
        assert!(state(false, false).safe_to_restart().safe);
        let busy = state(false, true).safe_to_restart();
        assert!(!busy.safe);
        assert!(busy.reason.unwrap().contains("--busy"));
    }

    #[test]
    fn unknown_methods_are_rejected_not_answered() {
        let s = state(false, false);
        let response = dispatch(&s, proto::Id::Number(1), "robot.doSomethingElse");
        assert_eq!(response.error.unwrap().code, proto::code::METHOD_NOT_FOUND);
    }

    #[test]
    fn model_api_is_reported() {
        let s = state(false, false);
        let response = dispatch(&s, proto::Id::Number(1), proto::method::ROBOT_MODEL_API);
        let result: proto::ModelApiResult = response.result_as().unwrap();
        assert_eq!(result.model_api, MODEL_API);
    }

    /// The summary must report a *late* loop as late. A loop that runs at 60% of its
    /// target rate is alive and passes its health check, so this number is the only thing
    /// that would show it.
    #[test]
    fn achieved_percent_reports_a_slow_loop() {
        let interval = Duration::from_millis(100);

        // 300 ticks in 30s at 100ms = exactly on time.
        assert_eq!(
            achieved_percent(300, Duration::from_secs(30), interval),
            Some(100)
        );
        // Half the ticks it should have managed.
        assert_eq!(
            achieved_percent(150, Duration::from_secs(30), interval),
            Some(50)
        );
        // Too early to say anything: less than one interval has passed.
        assert_eq!(
            achieved_percent(0, Duration::from_millis(50), interval),
            None
        );
    }

    #[test]
    fn durations_accept_seconds_and_millis() {
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("3").unwrap(), Duration::from_secs(3));
        assert!(parse_duration("soon").is_err());
    }
}
