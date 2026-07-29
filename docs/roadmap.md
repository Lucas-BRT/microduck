# Roadmap

Status: draft · Date: 2026-07-28 · Owner: pierre

Companion to [`architecture.md`](architecture.md) (what we're building) and
[`updater-design.md`](updater-design.md) (how it ships). This is *order and sequencing*
— it will change; the design docs shouldn't.

## Where we are

| | |
|---|---|
| `updater/` | engine, verification, store, journal, hooks, preflight, GitHub/HF/local sources, IPC server, systemd unit — **done** |
| `robot-proto/` | wire contract for `update.*` and `robot.*` — **done**; serde/serde_json/semver only, so nothing on the recovery path pulls the engine's tree |
| `robotd/` | heartbeat + the four `robot.*` methods, systemd unit — **skeleton done**; no control, no kinematics |
| `robotctl/` | CLI over the update socket — **done** for the `update` namespace; depends on `robot-proto`, not `updater` |
| `xtask/` | package · sign · promote — **done**, byte-identical promotion verified |
| `.github/` | ci · release · promote — **ci passing**; release/promote still unrun (needs secrets + the `release` environment) |
| bootstrap | `updaterd install` + `scripts/install.sh` — a robot installs its first release through the **ordinary engine**, so there is no bootstrap-only code path to drift |
| `deploy/` | shipped `updater.toml`, trust anchor, journald retention drop-in |
| `scripts/` | `install.sh` provisioning · `board-test.sh` — **passing in CI**: 13 checks on emulated aarch64 across Debian Trixie, Ubuntu Noble and Debian Bookworm (glibc 2.36, the floor) |
| tests | **229 passing**, including the health gate against a real `robotd` process |
| missing | `mediad`, `btd`, `robot-config`, app, SDK, hardware |

## The framing

**The hard part is productisation, not capability.** `microduck_runtime` already walks,
runs gait policies, does perception and mapping. What doesn't exist is a robot you can
hand to a stranger: app-driven updates, safety authority, privacy, provisioning,
recovery. Porting existing capability into the new architecture is laborious but
*known-feasible*; the unknowns are all on the productisation side.

**The updater is finished and instrumentally useless.** It has nothing real to ship. So
the first milestone is whatever gives it cargo — and, now that a team is arriving, gives
*them* a way to share work.

## What changed the order: the team arrives in ~2 weeks

Others will work on `robotd`/`mediad` and **share builds through the updater**. That
makes two things urgent that would otherwise have waited:

1. **Dev-channel installs** — install a specific branch or commit on a board, without
   cutting a release. This is now ahead of `btd`: teammates will use `robotctl`, not the
   phone app.
2. ~~**A repo and a dev signing key**~~ — **done.** `pollen-robotics/miniduck_daemon`
   (private), CI green on first fix; `team.dev` key generated. Still outstanding before a
   real release: the signing secrets and the `release` environment gate (`ci-setup.md`).

`btd` and the app path slip behind both. They matter for *customers*, not for the team.

## Milestones

Each has a test that says "done", because milestones without one drift.

### M1 — Close the loop  ·  no hardware needed  ·  **done**

The updater got something real to gate against, and the team got a shared crate boundary
to build on.

- ✅ **`robotd` skeleton.** A heartbeat loop plus a unix socket answering the four methods
  `updaterd` calls — `robot.health`, `robot.safeToRestart`, `robot.modelApi`,
  `robot.remoteSessionActive`. Its state is atomics rather than a mutex on purpose: a
  robot whose control loop is wedged must still be able to answer "I am not healthy",
  and if answering needed the loop's lock, the one case where `updaterd` needs an answer
  is the case it would hang in. `--unhealthy` / `--busy` exercise rollback on a bench
  robot without breaking a real build.
- ✅ **Extracted `robot-proto`.** `robotd` and `robotctl` depend on it; `robotctl` no
  longer depends on `updater` at all, which was the point — a support tool on the
  recovery path should not link the update engine, and now structurally cannot reach
  into its internals instead of going through the socket.
- ✅ **The gate is real.** `on_apply` and `health` in `updater.example.toml` are off their
  bootstrap values (§16.5); the test that pinned the inert state now asserts the opposite
  direction, so a regression to `probe = "none"` — which would silently disable
  auto-rollback and look like nothing in a diff — fails the build.
- ✅ **One source of truth for the robotd socket.** `HealthCheck::Socket` used to carry a
  `path` that was silently ignored (the client is built from a CLI flag), so a robot could
  be configured to probe one socket and actually probe another. The field is gone; the
  path is `robot_socket` at the top level of the config, and `--robot-socket` is a
  documented dev override.

**Done:** `updater_gate.rs` applies an update against a **real running `robotd` process**
over a real socket and commits; a `robotd --unhealthy` fails the gate and the content
behind `current` reverts to the previous release.

Two things that test taught, worth keeping:

- It lives in `robotd/tests/` rather than `updater/tests/` because only there does cargo
  define `CARGO_BIN_EXE_robotd` and **guarantee the binary is rebuilt** before the test
  runs. The first version guessed the path from `current_exe()` and merely checked the
  file existed — and `cargo test --test <name>` does not rebuild sibling binaries, so it
  silently tested a stale `robotd`. A sabotage check appeared to pass while proving
  nothing.
- Sabotaging `robotd`'s health reply was caught **only** by these tests: all 190 others
  passed, including `robot-proto`'s own round-trip test (both sides share the struct, so
  it cannot detect skew) and `robotd`'s unit tests (they called `state.health()` directly,
  bypassing dispatch). A `dispatch`-level unit test now closes that gap in microseconds;
  the process-level test stays, because it is the only thing covering the socket itself.

Running the three binaries together — rather than only the automated tests — also turned
up a live bug: the **unattended** path applied `Target::Latest` with no `known_bad` check,
so a broken *mandatory* release (one carrying `min_supported`) would loop forever on every
robot in the fleet — apply, fail the gate, roll back, wait, repeat, re-downloading and
restarting `robotd` each cycle. Fixed and tested (§8.1). Worth noting *why* the test suite
missed it: after each cycle the symlink is back on the good release, so nothing about the
robot's state looks wrong. The new test counts attempts in the journal; asserting on the
live version passes even with the guard removed.

A second gap the live run exposed: **`release.yml` never shipped `robotd`.** Its copy list
dated from the bootstrap era (updaterd + robotctl), and M1 flipped `on_apply` to restart
`robotd` without updating it — so the first real release would have installed cleanly, failed
`systemctl restart robotd` with "unit not found", and rolled itself back on every robot. The
cause was that "what the daemon artifact contains" was stated in three unlinked places: the
workflow's copy list, `on_apply`'s units, and each unit's `ExecStart`. A test now compares
them (`config.rs::every_unit_on_apply_restarts_is_actually_shipped`) and fails if a restarted
unit has no crate, no unit file, or no line in the release workflow — verified by
reintroducing the bug.

**`robot-config` was deliberately dropped from M1.** It was listed here, but M1's
done-test is about the health gate, and a heartbeat daemon needs CLI flags, not a shared
config store. Building it now would be carried-but-not-deliverable code. It lands when
something actually reads it — `btd`'s wifi provisioning in M6, or `robotd`'s calibration
in M4, whichever comes first.

### M2 — Dev channel  ·  no hardware needed  ·  *unblocks the team*

Install a branch or commit on a board, over the air, without cutting a release.

Most of the mechanism already exists and is deliberately shaped for this:

| Need | Status |
|---|---|
| Dev builds must never become fleet `latest` | ✅ two independent guards: GitHub's prerelease flag *and* a semver prerelease component |
| Installing an older/non-linear version | ✅ `Target::Exact` bypasses the downgrade guard by design |
| Dev builds signed with a separate key | ✅ `*.dev.pub` in the trusted set, gated by `allow_dev_keys` |
| Rollback/prune/known-bad on dev versions | ✅ they're ordinary semver, so the store needs no changes |
| Local sideload while iterating on your own board | ✅ `LocalDir` source, verification not relaxed |

What's actually new:

- **Version scheme.** Dev builds are `<next>-dev.<run>.<sha7>`, e.g.
  `0.2.0-dev.5.abc1234`. Valid semver, unique per build, sorts *below* `0.2.0` — so a
  dev build can never look like an upgrade from the release it precedes.
- **A moving tag per branch.** CI on push publishes/replaces a prerelease tagged
  `daemon-dev-<branch>`. The tag moves; the version inside is always unique.
- **`Target::Ref(String)`** and `robotctl update apply daemon --ref my-branch`, so
  nobody types a 40-character version. Needs a `manifest_at_tag()` on the GitHub source
  and an `API_VERSION` bump (additive, but the enum changes).
- **A dev key**, distinct from the release key, in every developer board's trusted set —
  and *not* in a customer robot's.

**Done when:** a teammate pushes a branch, and I install it on a board with
`robotctl update apply daemon --ref their-branch`, with rollback still working.

### M3 — `robotd` for real  ·  sim first

Port control into the new architecture. The MuJoCo setup in `microduck_brain` means
most of this needs no hardware — worth building the seam early: a `RobotBackend` trait
with `Mujoco` and `Hardware` impls, the same shape as `RobotClient` in the updater.

**Safety authority belongs here, not in M6.** `architecture.md` §6 designs it and
nothing implements it. It's the one thing that can hurt someone, and retrofitting
authority arbitration into a working control loop is far worse than designing it in.

**Done when:** it walks in sim through the new architecture, and `robotd` enforces
joint/thermal/fall limits regardless of what any client asks for.

### M4 — Hardware bring-up

M3 on the Radxa with real motors and IMU. This is where the genuinely unknown numbers
appear: control-loop jitter on a non-RT kernel, ONNX inference rate on Cortex-A55, eMMC
write timing, thermals, battery. Also the first real test of `systemctl restart` in
`on_apply`, and of the health-gate timeouts — 30s is currently a guess.

Also the first chance to settle the **log retention** question, which cannot be answered
off-board (`deploy/README.md`):

- `findmnt /var/log` — if Armbian's RAM-log has it on tmpfs, journald's `Storage=persistent`
  is a directory in memory: it survives a clean `reboot` and loses recent logs on a power
  cut, which is how a robot is actually switched off. Decide explicitly: disable the RAM log
  and accept eMMC writes, or keep it and rely on the update history (which does not go
  through `/var/log`).
- `journalctl --list-boots` after a real reboot — two or more entries, or the drop-in is not
  doing what it claims.

**Done when:** it walks on hardware, an update applied via `robotctl` restarts `robotd`
cleanly with the gate passing, and `journalctl -u robotd -b -1` returns the previous boot's
logs after a power cut.

### M5 — `mediad`, WebRTC, SDK

Camera/mic, encode, perception, the remote gateway. Privacy lands here and not later:
per-session consent and a visible streaming indicator are cheap now and expensive to
bolt on. The SDK's WebSocket + snapshot path (§5.3) is what makes "an LLM drives the
robot" easy.

**Done when:** telepresence works from outside the LAN, and a server-side script can
fetch a frame and send an intent in a few dozen lines.

### M6 — Ship readiness

`btd` + the app update path, provisioning (device identity, calibration, key
installation), recovery mode (§8.2's last link), manifest staleness reporting (§8.4.2),
and the authority arbitration finished.

**Done when:** a non-developer updates the robot from the phone, and a deliberately
bricked release recovers without a laptop.

## Organisation

**One repo, one workspace.** `robotd`, `mediad`, `btd` join as siblings. They co-version
because they all ship in the same `daemon` artifact — one version line is correct, and
models version separately already.

**Crate layout as it should end up:**

```
robot-proto/    wire types — serde/serde_json/semver only; btd/robotd/robotctl depend
                on this, never on updater
robot-config/   config store: file + flock + inotify        (not built yet)
updater/        engine + updaterd
robotctl/       CLI
robotd/         control, kinematics, gait, safety           (skeleton)
mediad/         camera, encode, perception, WebRTC gateway  (not built yet)
btd/            BLE transport adapter                       (not built yet)
xtask/          build/publish tooling — never ships
```

**Docs per concern, not per service.** `architecture.md` is the cross-cutting contract;
a service gets its own design doc only when it earns one (`updater-design.md` is the
model). Resist one giant document.

**Two channels of work for newcomers.** A teammate on `robotd` should be able to: clone,
`cargo test`, run against sim, push a branch, and install it on a board via `--ref`.
That's the whole onboarding path, and M1+M2 are exactly what make it true.

## Decisions that shape work rather than follow it

1. ~~**Signing key custody**~~ — **done.** Three encrypted release keys plus an
   unencrypted dev key in `~/.duck-keys`, all round-trip verified. Releases are signed in
   CI behind an approval gate; only `release-1` goes into secrets. See
   [`ci-setup.md`](ci-setup.md).
2. **Safety authority** (§6) — pulled into M3 for the reason above.
3. **Provisioning** — deciding nothing now is fine, but §5.7's per-device state
   (calibration, identity) needs a home before the first robot ships, and it constrains
   M6.
4. **Privacy** — consent + indicator in M5, not M6.

## Not doing, on purpose

Recorded so they stay decided: A/B image updates, OS/kernel OTA, fleet
dashboards/telemetry, delta updates, staged rollouts, hardware capability matrix,
competing model alternatives per slot (§17), peripheral firmware OTA (§11.1).
