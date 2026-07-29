//! IPC contracts between the robot's services and their clients.
//!
//! Two namespaces, both over the same wire format:
//!
//!  - `update.*` — `updaterd`'s API, spoken by `robotctl` and later `btd`.
//!  - `robot.*`  — `robotd`'s API. Small on purpose: it is what `updaterd` needs to
//!    decide whether an update is safe and whether it worked.
//!
//! Extracted from `updater` once a second service needed it. Keeping it a separate
//! near-dependency-free crate is what stops `btd` and `robotd` inheriting the update
//! engine's http/tar/crypto tree.
//!
//! **Wire format: JSON-RPC 2.0, one object per line (NDJSON), over a unix
//! socket.** Deliberately not a bespoke protocol — JSON-RPC gives standard
//! request/response correlation, a standard error shape, and standard
//! notifications, so "what protocol is this?" has a real answer and any language
//! can speak it. Framing is a single newline, handled by
//! `tokio_util::codec::LinesCodec`.
//!
//! Progress is pushed as a JSON-RPC **notification** (a message with no `id`),
//! which is exactly what notifications are for. A client that reconnects
//! mid-update resubscribes and keeps receiving them.
//!
//! ```text
//! → {"jsonrpc":"2.0","id":1,"method":"update.apply","params":{...}}
//! ← {"jsonrpc":"2.0","method":"update.progress","params":{...}}   (no id)
//! ← {"jsonrpc":"2.0","method":"update.progress","params":{...}}
//! ← {"jsonrpc":"2.0","id":1,"result":{...}}
//! ```
//!
//! **Keep the dependency list minimal.** serde, serde_json and semver only — no http, tar,
//! crypto or async runtime. That constraint is the reason the crate exists: `robotd` and
//! `robotctl` sit on the recovery path, and a protocol crate that dragged in the update
//! engine's tree would defeat the split it was extracted to create.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

/// Protocol version, exchanged via [`method::HELLO`].
///
/// Bumped on any incompatible change. `robotctl` and `updaterd` ship together,
/// but a stale `robotctl` in someone's shell is normal — better a clear refusal
/// than a misparsed request.
///
/// v2 added `HelloResult::revision`. During prototyping the wire shape simply changes and
/// this bumps; no accommodation is made for peers that predate a field, because there are
/// none in the field yet and pretending otherwise means carrying compatibility code that
/// has never been exercised.
pub const API_VERSION: u32 = 2;

pub const DEFAULT_SOCKET: &str = "/run/updaterd.sock";

/// Method names.
///
/// Namespaced (`update.*`) so other namespaces can be added without collision,
/// matching `robotctl`'s command namespacing.
pub mod method {
    pub const HELLO: &str = "hello";

    pub const CHECK: &str = "update.check";
    pub const APPLY: &str = "update.apply";
    pub const ROLLBACK: &str = "update.rollback";
    pub const RESET_TO_GOLDEN: &str = "update.resetToGolden";
    pub const SELECT: &str = "update.select";
    pub const PIN: &str = "update.pin";
    pub const STATUS: &str = "update.status";
    pub const LIST_INSTALLED: &str = "update.listInstalled";
    pub const LOG: &str = "update.log";
    pub const SUBSCRIBE: &str = "update.subscribe";

    /// Server → client notification. Never carries an `id`.
    pub const PROGRESS: &str = "update.progress";

    // ── robotd's side ────────────────────────────────────────────────────────
    //
    // `updaterd` calls these. Every one must be answerable while the robot is in a bad
    // state — that is the whole point of asking.

    /// May the control loop be restarted right now?
    pub const ROBOT_SAFE_TO_RESTART: &str = "robot.safeToRestart";
    /// Did the robot come up correctly? The post-update health gate.
    pub const ROBOT_HEALTH: &str = "robot.health";
    /// Which model API version does this build implement?
    pub const ROBOT_MODEL_API: &str = "robot.modelApi";
    /// Is a telepresence session live?
    pub const ROBOT_SESSION_ACTIVE: &str = "robot.remoteSessionActive";
}

/// JSON-RPC error codes.
///
/// -32768..-32000 is spec-reserved; application errors use a private range. The
/// distinctions exist so clients can act programmatically — notably so a test can
/// assert "correctly refused" rather than "something broke", and a script can
/// retry on [`BUSY`] instead of failing.
pub mod code {
    // Spec-reserved.
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    // Application-specific.
    pub const BUSY: i32 = 1;
    pub const UNKNOWN_COMPONENT: i32 = 2;
    pub const PROTOCOL_MISMATCH: i32 = 3;
    pub const PREFLIGHT_FAILED: i32 = 4;
    pub const NETWORK: i32 = 5;
    pub const VERIFICATION_FAILED: i32 = 6;
    pub const INCOMPATIBLE: i32 = 7;
    pub const HOOK_FAILED: i32 = 8;
    pub const HEALTH_CHECK_FAILED: i32 = 9;
    /// Update failed *and* rollback failed. Kept distinct so support sees the
    /// most serious outcome immediately.
    pub const ROLLBACK_FAILED: i32 = 10;
    /// The component exists but the requested version is not installed. Distinct
    /// from UNKNOWN_COMPONENT so a client can tell "no such robot part" from "no
    /// such version of it".
    pub const NOT_INSTALLED: i32 = 11;
    /// A newer version is installed and the request would move backwards.
    pub const WOULD_DOWNGRADE: i32 = 12;
    /// Verified, but larger than the configured archive limits allow.
    pub const ARCHIVE_TOO_LARGE: i32 = 13;
    /// The caller may connect but is not allowed to perform this operation.
    /// Distinct from every failure mode so a client can say "ask an administrator"
    /// rather than "something broke".
    pub const PERMISSION_DENIED: i32 = 14;
}

/// Request identifier. `None` makes the message a notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(u64),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    /// Absent for notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub fn new(id: Id, method: &str, params: impl Serialize) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: Some(id),
            method: method.to_owned(),
            params: Some(serde_json::to_value(params)?),
        })
    }

    /// A notification: no `id`, so no response is expected.
    pub fn notification(method: &str, params: impl Serialize) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: None,
            method: method.to_owned(),
            params: Some(serde_json::to_value(params)?),
        })
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    /// Decode `params` into a typed struct.
    pub fn params_as<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.params.clone().unwrap_or(Value::Null))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    /// `None` when the request could not be parsed well enough to recover an id.
    pub id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl Response {
    pub fn ok(id: Option<Id>, result: impl Serialize) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: Some(serde_json::to_value(result)?),
            error: None,
        })
    }

    pub fn err(id: Option<Id>, error: Error) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }

    pub fn result_as<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.result.clone().unwrap_or(Value::Null))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: i32,
    /// Displayable in the app. Specific enough to diagnose from a support ticket.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Error {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

// ── Params and results ───────────────────────────────────────────────────────

/// Name of a component as declared in `updater.toml` (`daemon`, `model`).
///
/// A string, not an enum: the engine is config-driven so one binary serves
/// different robots.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ComponentId(pub String);

impl ComponentId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ComponentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ComponentId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloParams {
    pub api_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub api_version: u32,
    pub daemon_version: Option<semver::Version>,
    /// Source revision of the **running** binary.
    ///
    /// `None` means the binary was not built by CI — a real state (someone's laptop build),
    /// not a missing field. Always serialised, including as `null`, so the wire shape does
    /// not depend on the value.
    ///
    /// No `#[serde(default)]`: serde already maps an absent field to `None` for any
    /// `Option`, so the attribute would buy nothing. Worth stating because it looks like
    /// tolerance for older peers and is not — there is no such tolerance here by design.
    ///
    /// Support needs this over IPC and not only in the journal: on a robot whose logs were
    /// volatile or already rotated, asking the daemon what it is must still work.
    pub revision: Option<String>,
}

/// What an apply should move to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Whatever the source advertises as newest.
    Latest,
    /// An exact version — the primitive that makes release testing scriptable.
    Exact(semver::Version),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyOptions {
    /// Run every check (fetch, verify, compatibility, space) and stop before the
    /// symlink swap.
    #[serde(default)]
    pub dry_run: bool,
    /// Skip *only* the "no active remote session" preflight check. Never
    /// bypasses signature, hash, or compatibility — those have no override.
    #[serde(default)]
    pub interrupt_sessions: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentParams {
    pub component: ComponentId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyParams {
    pub component: ComponentId,
    pub target: Target,
    #[serde(default)]
    pub options: ApplyOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectParams {
    pub component: ComponentId,
    pub version: semver::Version,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinParams {
    pub component: ComponentId,
    /// `None` unpins.
    pub version: Option<semver::Version>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogParams {
    pub limit: usize,
}

/// Where an in-flight update has got to. Mirrors the state machine in
/// `docs/updater-design.md` §7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Preflight,
    Checking,
    Downloading,
    Verifying,
    Extracting,
    RunningPreHook,
    Swapping,
    RunningPostHook,
    Applying,
    HealthGate,
    Committing,
    RollingBack,
}

/// Payload of an [`method::PROGRESS`] notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub component: ComponentId,
    pub phase: Phase,
    /// 0-100 where meaningful (downloads); `None` otherwise.
    pub percent: Option<u8>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentStatus {
    pub component: ComponentId,
    pub installed: Option<semver::Version>,
    pub phase: Phase,
    /// `None` when no health probe is configured.
    pub healthy: Option<bool>,
    pub pinned: Option<semver::Version>,
    pub last_attempt: Option<LogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledRelease {
    pub version: semver::Version,
    pub active: bool,
    pub golden: bool,
    /// Git SHA of the build, for provenance.
    pub source_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CheckResult {
    UpToDate {
        installed: semver::Version,
    },
    Available {
        installed: Option<semver::Version>,
        candidate: semver::Version,
        /// True when `min_supported` makes this update non-optional.
        mandatory: bool,
        changelog: Option<String>,
    },
    /// A newer version exists but cannot be installed here.
    Incompatible {
        candidate: semver::Version,
        reason: String,
    },
}

/// Result of an apply / rollback / select.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ApplyResult {
    Applied {
        from: Option<semver::Version>,
        to: semver::Version,
    },
    AlreadyCurrent {
        version: semver::Version,
    },
    /// Everything verified; stopped before the swap because `dry_run` was set.
    DryRunPassed {
        candidate: semver::Version,
    },
    /// Applied, failed its gate, reverted. The robot is on `reverted_to`.
    RolledBack {
        attempted: semver::Version,
        reverted_to: Option<semver::Version>,
        reason: String,
    },
    /// Failed its gate and there was **nowhere to revert to** — a first install
    /// that never came up, with no previous release and no golden configured.
    ///
    /// Distinct from `RolledBack` because nothing was reverted: reporting a
    /// rollback that did not happen would be a lie, and the robot needs operator
    /// or factory intervention.
    Stuck {
        version: semver::Version,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Unix seconds.
    pub at: i64,
    pub component: ComponentId,
    pub from: Option<semver::Version>,
    pub to: Option<semver::Version>,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Success,
    RolledBack {
        reason: String,
    },
    /// Refused before anything changed.
    Aborted {
        reason: String,
    },
}

// ── robot.* results ──────────────────────────────────────────────────────────
//
// Typed rather than ad-hoc JSON so the two sides cannot drift: `updaterd` used to poke
// at `result["healthy"].as_bool()`, which compiles fine against a `robotd` that answers
// something else entirely.

/// Answer to [`method::ROBOT_SAFE_TO_RESTART`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeToRestartResult {
    pub safe: bool,
    /// Why not, when `safe` is false. Displayable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Answer to [`method::ROBOT_HEALTH`].
///
/// A robot that is up but *not* healthy must say so rather than fail to answer: the
/// difference decides whether an update rolls back for a known reason or for a timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResult {
    pub healthy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Answer to [`method::ROBOT_MODEL_API`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelApiResult {
    /// Sensor-input / actuator-output contract this build implements
    /// (`updater-design.md` §5.5).
    pub model_api: u32,
}

/// Answer to [`method::ROBOT_SESSION_ACTIVE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActiveResult {
    pub active: bool,
}

/// Re-exported so consumers spell protocol version types with the *same* `semver` this
/// crate compiled against. Without it, a crate that depends on `semver` separately can end
/// up with two incompatible copies of `Version` and a type error that reads as nonsense
/// ("expected Version, found Version").
pub use semver;

// ── build identity ───────────────────────────────────────────────────────────

/// What a binary reports about itself: version, source revision, build time.
///
/// Lives here so every service answers the question the same way. Support asks "what was
/// running when this happened?", and a version number alone does not answer it — two
/// builds of `0.2.0` from different commits are indistinguishable, which is exactly the
/// situation during the dev-branch installs of M2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    /// Crate version. All workspace crates share one version line because they ship in
    /// one artifact.
    pub version: &'static str,
    /// Git SHA, or `None` for a build that did not come from CI.
    ///
    /// Read from the `DUCK_REVISION` environment variable **at compile time**, not from
    /// git at runtime: a shipped robot has no git repository, and running `git` from a
    /// daemon to learn its own identity would be absurd. CI sets it; a laptop build
    /// honestly reports that it does not know.
    pub revision: Option<&'static str>,
    /// RFC 3339 build timestamp from `DUCK_BUILD_TIME`, or `None` locally.
    pub built_at: Option<&'static str>,
}

impl std::fmt::Display for BuildInfo {
    /// One line, greppable, and explicit about what is unknown.
    ///
    /// "unknown" rather than a silent omission: a support log that simply lacks a revision
    /// is ambiguous between "local build" and "we forgot to log it".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.version)?;
        match self.revision {
            Some(rev) => write!(f, " (rev {rev}")?,
            None => write!(f, " (rev unknown, not a CI build")?,
        }
        match self.built_at {
            Some(at) => write!(f, ", built {at})"),
            None => write!(f, ")"),
        }
    }
}

/// Build identity of the **calling crate**.
///
/// A macro rather than a function because `env!` must expand in the caller: called from a
/// function here it would report `robot-proto`'s version for everyone. The workspace does
/// share one version today, so that mistake would look correct and stay invisible until
/// the day a crate is versioned separately.
#[macro_export]
macro_rules! build_info {
    () => {
        $crate::BuildInfo {
            version: env!("CARGO_PKG_VERSION"),
            revision: option_env!("DUCK_REVISION"),
            built_at: option_env!("DUCK_BUILD_TIME"),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_valid_jsonrpc() {
        let req = Request::new(
            Id::Number(1),
            method::APPLY,
            ApplyParams {
                component: ComponentId::new("daemon"),
                target: Target::Exact(semver::Version::new(1, 4, 2)),
                options: ApplyOptions {
                    dry_run: true,
                    interrupt_sessions: false,
                },
            },
        )
        .unwrap();

        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains(r#""jsonrpc":"2.0""#));
        assert!(line.contains(r#""method":"update.apply""#));

        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);
        let params: ApplyParams = back.params_as().unwrap();
        assert!(params.options.dry_run);
    }

    #[test]
    fn notification_omits_id() {
        let note = Request::notification(
            method::PROGRESS,
            Progress {
                component: ComponentId::new("daemon"),
                phase: Phase::Downloading,
                percent: Some(42),
                detail: None,
            },
        )
        .unwrap();

        let line = serde_json::to_string(&note).unwrap();
        assert!(
            !line.contains("\"id\""),
            "notifications must not carry an id"
        );
        assert!(note.is_notification());
    }

    #[test]
    fn error_response_omits_result() {
        let resp = Response::err(
            Some(Id::Number(7)),
            Error::new(code::BUSY, "another update is in progress"),
        );
        let line = serde_json::to_string(&resp).unwrap();
        assert!(!line.contains("\"result\""));
        assert!(line.contains("\"code\":1"));
    }

    #[test]
    fn result_response_omits_error() {
        let resp = Response::ok(
            Some(Id::Number(7)),
            CheckResult::UpToDate {
                installed: semver::Version::new(1, 0, 0),
            },
        )
        .unwrap();
        let line = serde_json::to_string(&resp).unwrap();
        assert!(!line.contains("\"error\""));

        let back: Response = serde_json::from_str(&line).unwrap();
        let parsed: CheckResult = back.result_as().unwrap();
        assert!(matches!(parsed, CheckResult::UpToDate { .. }));
    }

    /// `robotd`'s answers must round-trip, and an omitted `reason` must stay omitted —
    /// `updaterd` distinguishes "unhealthy with a reason" from "unhealthy".
    #[test]
    fn robot_results_round_trip() {
        let healthy = HealthResult {
            healthy: true,
            reason: None,
        };
        let line = serde_json::to_string(&healthy).unwrap();
        assert!(!line.contains("reason"), "absent reason must not serialise");
        assert_eq!(
            serde_json::from_str::<HealthResult>(&line).unwrap(),
            healthy
        );

        let sick = HealthResult {
            healthy: false,
            reason: Some("motors not responding".into()),
        };
        let line = serde_json::to_string(&sick).unwrap();
        assert_eq!(serde_json::from_str::<HealthResult>(&line).unwrap(), sick);
    }

    /// A local build must say so, rather than looking like a release whose revision was
    /// simply not logged. Support reads this line; ambiguity in it costs a round trip.
    #[test]
    fn build_info_is_explicit_about_an_unknown_revision() {
        let local = BuildInfo {
            version: "0.2.0",
            revision: None,
            built_at: None,
        };
        assert_eq!(local.to_string(), "0.2.0 (rev unknown, not a CI build)");

        let released = BuildInfo {
            version: "0.2.0",
            revision: Some("abc1234"),
            built_at: Some("2026-07-28T12:00:00Z"),
        };
        assert_eq!(
            released.to_string(),
            "0.2.0 (rev abc1234, built 2026-07-28T12:00:00Z)"
        );
    }

    /// `build_info!()` must report the *calling* crate. Here the caller is `robot-proto`
    /// itself, so this only pins that the macro expands and reads a real version — the
    /// cross-crate property is what the macro form exists for.
    #[test]
    fn build_info_macro_reports_a_version() {
        let info = build_info!();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(!info.version.is_empty());
    }

    /// A local build reports no revision, and that must survive the wire as `null` rather
    /// than as an absent field — one shape whatever the value, so a reader never has to
    /// distinguish "no revision" from "field missing".
    #[test]
    fn hello_result_round_trips_with_and_without_a_revision() {
        let local = HelloResult {
            api_version: API_VERSION,
            daemon_version: Some(semver::Version::new(0, 1, 0)),
            revision: None,
        };
        let line = serde_json::to_string(&local).unwrap();
        assert!(line.contains("\"revision\":null"), "{line}");
        assert_eq!(serde_json::from_str::<HelloResult>(&line).unwrap(), local);

        let released = HelloResult {
            revision: Some("abc1234".into()),
            ..local.clone()
        };
        let line = serde_json::to_string(&released).unwrap();
        assert_eq!(
            serde_json::from_str::<HelloResult>(&line).unwrap(),
            released
        );
    }

    /// A message with no `id` must still parse when the server sends it back to a
    /// subscriber that reconnected — the id is genuinely absent, not null.
    #[test]
    fn notification_parses_without_id_field() {
        let line = r#"{"jsonrpc":"2.0","method":"update.progress","params":{"component":"model","phase":"health_gate","percent":null,"detail":null}}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert!(req.is_notification());
        let p: Progress = req.params_as().unwrap();
        assert_eq!(p.phase, Phase::HealthGate);
    }
}
