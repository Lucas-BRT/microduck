# CI setup

Status: draft · Date: 2026-07-28 · Owner: pierre

One-time setup for the release pipeline. See [`updater-design.md`](updater-design.md)
§5.4 for key custody and §16.3 for the staging → stable model.

## Decision: releases are signed in CI

Deliberate, not a default. The alternative — CI builds, a human signs locally — keeps the
fleet-signing key off a networked service, but adds friction to every release. We took
the automated path and compensate with an approval gate.

The consequence to be clear-eyed about: **`release-1`'s private key is reachable from
CI.** Its passphrase must also be a CI secret for signing to work non-interactively, so
against an attacker with repo-admin access, encryption adds nothing — they get both. The
encryption protects the key *at rest elsewhere* (laptop, password manager, backups).

What actually limits the damage is the tiering:

| key | in CI | role |
|---|---|---|
| `release-1` | **yes** | signs every release and promotion |
| `release-2` | no | first rotation target if CI or `release-1` is compromised |
| `release-3` | no, ideally never on a networked machine | last resort |
| `team.dev` | yes (dev workflow only) | branch builds; cannot touch a customer robot |

All **public** keys go into every robot image from the start — a robot can only verify
against the set baked into it, so this is the only chance to make rotation possible
without physically re-flashing.

## Secrets and variables

GitHub Secrets are **write-only**: once set, nobody — including you — can read them back.
They are a *deployment copy*, never storage. The password manager remains the system of
record; losing it means the key is gone and every robot trusting it can never be signed
for again.

Repository → Settings → Secrets and variables → Actions.

**Secrets** (encrypted, not readable back):

| name | value |
|---|---|
| `MINISIGN_SECRET_KEY` | full contents of `~/.duck-keys/release-1.key`, both lines |
| `MINISIGN_PASSWORD` | the passphrase for `release-1` |

**Variables** (plain, readable — a public key is not a secret):

| name | value |
|---|---|
| `MINISIGN_PUBLIC_KEY` | the key line of `~/.duck-keys/release-1.pub` |

The public key is used by `release.yml` to verify a release through the robot's own code
path before publishing it. Keeping it as a *variable* rather than a secret is
deliberate: treating a public key as secret invites confusion about which half is which.

Do **not** add `release-2` or `release-3`. Their entire value is being absent from here.

## The `release` environment

Both `release.yml` and `promote.yml` declare `environment: release`. Create it under
Settings → Environments and add **required reviewers**.

Without it, anyone who can push a `daemon-staging-v*` tag can sign for the whole fleet.
With it, reaching the signing key needs a second person's approval — which recovers most
of what local signing would have given, at the cost of one click per release.

Fork pull requests never receive secrets, so the key is unreachable from contributor PRs
regardless.

## Where the key is handled

Exactly one step per workflow writes the key to disk, and it is removed immediately:

```
umask 077
printf '%s' "$MINISIGN_SECRET_KEY" > "$RUNNER_TEMP/secret.key"
cargo run -p xtask -- sign --dir dist --key "$RUNNER_TEMP/secret.key"
shred -u "$RUNNER_TEMP/secret.key" || rm -f "$RUNNER_TEMP/secret.key"
```

Written to a file rather than passed as an argument, because a key on a command line is
visible in the process list to anything else on the runner.

`release.yml`'s verification step deliberately needs **no** key: `xtask package` emits a
second manifest with a bare-filename URL (for `LocalDir`), and `xtask sign` signs both in
one pass. Re-signing to verify would mean handling the signing key twice in one job for
no benefit.

## Cutting a release

```
git tag daemon-staging-v0.2.0 && git push --tags
```

`release.yml` then cross-builds for aarch64, packages, signs, verifies through the real
engine, and publishes a **prerelease**. Nothing reaches robots on `stable` yet.

After a canary robot has run the on-device checks, promote:

```
gh workflow run promote --field version=0.2.0
```

`promote.yml` re-signs a stable manifest pointing at the **same artifact bytes** staging
validated — no rebuild. Add `--field min_supported=0.2.0` only when remediating a bad
release (§8.1); it forces robots below that version to update without waiting for a
client.

## Rotating a key

If `release-1` or CI is compromised:

1. Replace `MINISIGN_SECRET_KEY` / `MINISIGN_PASSWORD` with `release-2`'s.
2. Publish a release signed by `release-2`. Robots already trust it — that is why both
   public keys shipped from the first image.
3. Remove `release-1.pub` from `trusted_keys_dir` in a subsequent release, so the
   compromised key stops being accepted.
4. Generate a replacement third key so a spare still exists:
   `cargo xtask keygen --kind release --name release-4 --out ~/.duck-keys`

Step 3 lags step 2 on purpose: revoking the old key before every robot has taken the
new-signed release would strand any robot that missed it.
