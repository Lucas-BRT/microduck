# CI setup

Status: draft · Date: 2026-07-28 · Owner: pierre

One-time setup for the release pipeline. See [`updater-design.md`](updater-design.md)
§5.4 for key custody and §16.3 for the staging → stable model.

## ⚠ Blocked: the approval gate this decision depended on is not available

**Status: unresolved. `release-1` is deliberately NOT in CI, and no release can be cut
until this is decided.**

The plan was: CI signs, and reaching the key needs a second person's approval via the
`release` environment's required-reviewers rule. Attempting to create that rule fails:

```
HTTP 422: Failed to create the environment protection rule.
Please ensure the billing plan supports the required reviewers protection rule.
```

Environment **protection rules** — required reviewers, wait timers, deployment branch
policies — need GitHub Team or Enterprise on a *private* repository. `pollen-robotics` is
on the free plan. The `release` environment exists, with zero protection rules.

Environment-scoping the secrets does not substitute for it. A secret scoped to an
environment is only readable by a job that declares `environment: release` — but with no
deployment branch policy, *any* branch may deploy to that environment, so any collaborator
who can push can add a workflow that declares it and prints the key. Scoping prevents
accidental exposure by an unrelated workflow; it does not prevent a person.

That matters more now than it would have last week: the point of the tiering below was to
bound what a CI compromise costs, and "a teammate can extract the fleet signing key with a
three-line workflow" is a larger blast radius than the design accepted. So the original
trade — friction versus a networked key — has to be made again, with the gate off the table:

| option | cost | leaves the fleet key off CI |
|---|---|---|
| **Sign locally, don't wire CI signing** | a few minutes per release, by hand | yes |
| Split: CI signs staging with `team.dev`, a human signs promotion with `release-1` | needs `promote` to re-sign the **artifact**, not only the manifest (see below) | yes |
| Upgrade the org to GitHub Team | money, org-level decision | no, but the gate works as designed |
| Put `release-1` in CI ungated | none | **no** — any collaborator can take it |

Signing locally is the cheapest correct answer *right now*: zero releases have been cut, the
cadence is a release every 2–3 weeks and later every 2–3 months (§3), and `xtask sign` is
already the same code path CI would run. It stops being cheap when releases get frequent or
when someone other than the key holder needs to cut one.

**The split option needs a code change first.** `xtask promote` re-signs the *manifest* and
points `sig_url` at the staging artifact's existing `.minisig`, so the artifact signature is
whatever staging used. If staging were signed with `team.dev`, a customer robot
(`allow_dev_keys = false`) would refuse the artifact even after promotion. Promotion would
have to re-sign the artifact bytes with `release-1` and upload that signature alongside the
stable manifest — which preserves the "identical bytes" property (§16.3), since only the
signature file changes.

## The tiering (unchanged, and still the thing that bounds damage)

Whatever is decided above, what limits the cost of a compromise is which key is reachable
from where:

| key | in CI | role |
|---|---|---|
| `release-1` | **not currently** — see above | signs every release and promotion |
| `release-2` | no | first rotation target if CI or `release-1` is compromised |
| `release-3` | no, ideally never on a networked machine | last resort |
| `team.dev` | intended, dev workflow only | branch builds; cannot touch a customer robot, because `allow_dev_keys` is false there |

All **public** keys go into every robot image from the start — a robot can only verify
against the set baked into it, so this is the only chance to make rotation possible
without physically re-flashing.

## Secrets and variables

GitHub Secrets are **write-only**: once set, nobody — including you — can read them back.
They are a *deployment copy*, never storage. The password manager remains the system of
record; losing it means the key is gone and every robot trusting it can never be signed
for again.

**Scope them to the `release` environment, not to the repository.** A repository secret is
readable by every workflow job in the repo; an environment secret is readable only by a job
declaring that environment. On this plan that difference stops an unrelated workflow from
seeing the key, and nothing more (see above) — but it is strictly better and costs nothing:

```bash
gh secret set MINISIGN_SECRET_KEY --env release < ~/.duck-keys/release-1.key
```

```bash
gh secret set MINISIGN_PASSWORD --env release
```

The second prompts, so the passphrase never lands in shell history or a transcript.

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
