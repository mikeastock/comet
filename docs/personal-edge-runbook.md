# Personal edge runbook

Redeploy the **buildr-dev** personal stack: Cloudflare Worker + Durable Objects,
devbox headless daemon, Mac app, and iOS app — all pointed at our edge and WorkOS
credentials (not production `edge.zeron.sh`).

## What’s “personal” vs production

| | Personal (this stack) | Upstream production |
| --- | --- | --- |
| Cloudflare account | buildr-dev (`9df6ecdbc507977b8256c6462ffff9b3`) | zeron |
| Public edge URL | `https://comet-native-edge.buildr-9df.workers.dev` | `https://edge.zeron.sh` |
| Host | `workers.dev` only (no custom domain) | custom domain + install routes on zeron.sh |
| WorkOS client id | `client_01KYXECAJHZ0A07VVNDKMWV3X5` | `client_01KWD0EAKZKD50YCQJNYSRE4BY` |
| Config commit | `chore(deploy): point personal stack at buildr edge` | mainline defaults |

Baked-in defaults live in:

- `edge/wrangler.jsonc` — CF account, `WORKOS_CLIENT_ID`, no zeron routes
- `apps/zeron/src/main.rs` — `DEFAULT_EDGE_URL` / `DEFAULT_WORKOS_CLIENT_ID`
- `apps/ios/Zeron/App/AppModel.swift` + `Views/SignInView.swift` — same endpoints

`WORKOS_API_KEY` is a **wrangler secret** (never committed). Local copy for
`wrangler dev`: `edge/.dev.vars` (gitignored).

## Prerequisites

- Mac: `wrangler` logged in as `mike@buildr.com` on **buildr-dev**
  (`npx wrangler whoami` → account `9df6ec…`)
- Node 22+ for `edge/`
- Rust toolchain (repo `rust-toolchain.toml`)
- SSH host `devbox` (`devbox-mike.tail5a0ea0.ts.net`)
- Xcode 26+ for iOS; iPhone paired for device install
- Remotes (this machine):

  ```text
  origin    → github.com:mikeastock/comet
  upstream  → github.com:zeronsh/comet
  ```

  Devbox flips those names: `origin` = zeronsh, `fork` = mikeastock.

## 0. Refresh main + re-apply personal wiring

The personal stack is **one commit** on top of upstream main (not a long-lived
fork of product code).

```sh
git fetch upstream main
git checkout -B personal-edge-redeploy upstream/main

# If the personal commit is already on a branch tip:
git cherry-pick <personal-deploy-sha>
# Historical sha (may need conflict fix on hot paths):
#   75c521d  chore(deploy): point personal stack at buildr edge
```

Confirm defaults after cherry-pick:

```sh
rg 'DEFAULT_EDGE_URL|account_id|WORKOS_CLIENT_ID|buildr-9df' \
  apps/zeron/src/main.rs edge/wrangler.jsonc apps/ios/Zeron
```

Expect buildr workers.dev URL + `client_01KYXECAJHZ0A07VVNDKMWV3X5` +
account `9df6ecdbc507977b8256c6462ffff9b3`.

Push the branch so the devbox can fetch it:

```sh
git push -u origin personal-edge-redeploy
```

---

## 1. Redeploy the Cloudflare Worker (Durable Objects)

From a machine authenticated to **buildr-dev**:

```sh
cd edge
npm ci
# Secret must exist once (no-op if already set):
#   printf '%s' "$WORKOS_API_KEY" | npx wrangler secret put WORKOS_API_KEY
npx wrangler secret list   # should list WORKOS_API_KEY
npx wrangler deploy
```

Success looks like:

```text
Uploaded comet-native-edge
Deployed comet-native-edge triggers
  https://comet-native-edge.buildr-9df.workers.dev
```

Bindings to expect: `SESSION_ROOMS`, `DEVICE_ROOMS`, `REGISTRY_ROOMS`, R2
`BLOBS` / `RELEASES`, vars `WORKOS_CLIENT_ID` + `AUTH_MODE=workos`.

### Smoke

```sh
# Unauthed root → 401 JSON (worker is live)
curl -sS -o /dev/null -w '%{http_code}\n' \
  https://comet-native-edge.buildr-9df.workers.dev/

# Auth routes configured (not 501 workos-not-configured)
curl -sS -X POST \
  https://comet-native-edge.buildr-9df.workers.dev/auth/exchange \
  -H 'content-type: application/json' -d '{}'
# → {"error":"missing code"}
```

### Do **not** re-add production routes

This stack intentionally omits:

```jsonc
// production only — do not deploy from personal wrangler.jsonc
"routes": [
  { "pattern": "edge.zeron.sh", "custom_domain": true },
  { "pattern": "zeron.sh/install.sh", "zone_name": "zeron.sh" },
  { "pattern": "zeron.sh/releases/*", "zone_name": "zeron.sh" }
]
```

Those belong to the zeron account/zone.

---

## 2. Rebuild + redeploy the zeron daemon on devbox

SSH host alias: `devbox`. Service: `zeron.service` (systemd user).
Binary path used by the unit:

`/data/workspace/code/oss/comet/target/release/zeron`

```sh
ssh devbox
```

On the box:

```sh
cd /data/workspace/code/oss/comet   # same tree as ~/code/oss/comet

git fetch fork personal-edge-redeploy
git fetch origin main

# Stash any local WIP so checkout is clean
git stash push -u -m "auto-stash before personal-edge-redeploy $(date -u +%Y%m%dT%H%M%SZ)" || true

git checkout -B personal-edge-redeploy fork/personal-edge-redeploy

export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p zeron

export ZERON_EDGE_URL=https://comet-native-edge.buildr-9df.workers.dev
export ZERON_WORKOS_CLIENT_ID=client_01KYXECAJHZ0A07VVNDKMWV3X5
export ZERON_DEVICE_NAME=devbox

# Captures ZERON_* into the unit + enables/starts the service
./target/release/zeron daemon install

systemctl --user daemon-reload
systemctl --user restart zeron.service
systemctl --user status zeron.service --no-pager
```

Healthy logs (journal or `~/.zeron/logs/`):

```text
engine core assembled
IPC server listening
device-room: host connected
registry room joined org=org_…
```

Confirm the unit still embeds credentials:

```sh
grep ZERON_ ~/.config/systemd/user/zeron.service
# ZERON_EDGE_URL=https://comet-native-edge.buildr-9df.workers.dev
# ZERON_WORKOS_CLIENT_ID=client_01KYXECAJHZ0A07VVNDKMWV3X5
# ZERON_DEVICE_NAME=devbox
```

Optional: confirm the binary itself bakes the same defaults:

```sh
strings target/release/zeron | rg 'buildr-9df|client_01KYX'
```

---

## 3. Rebuild the Mac app

On the Mac, on the personal-edge branch:

```sh
# from repo root
cargo build --release -p zeron
bash scripts/package-macos.sh
```

Artifacts:

| Path | Use |
| --- | --- |
| `target/package/Zeron.app` | local install |
| `target/package/zeron-<ver>-macos-arm64.dmg` | shareable install |
| `target/package/zeron-<ver>-macos-arm64-app.tar.gz` | auto-updater payload |

Install into Applications:

```sh
rm -rf /Applications/Zeron.app /Applications/Comet.app
cp -R target/package/Zeron.app /Applications/Zeron.app
```

Ad-hoc signed by default (`CODESIGN_IDENTITY` for Developer ID). First launch may
need right-click → Open under Gatekeeper.

Sanity-check baked endpoints:

```sh
strings /Applications/Zeron.app/Contents/MacOS/zeron \
  | rg 'buildr-9df|client_01KYX'
```

---

## 4. Rebuild + install the iOS app (device)

Team id used for automatic signing: `SA5NQ48YZF`. Device in this setup:
**Mike’s iPhone** (UDID `00008150-001231243C80401C`).

```sh
cd apps/ios

# Unlock the phone; keep it trusted / developer mode on.
xcrun devicectl list devices   # expect Mike’s iPhone available (paired)

xcodebuild -project Zeron.xcodeproj -scheme Zeron \
  -configuration Release \
  -destination 'id=00008150-001231243C80401C' \
  -derivedDataPath build/DerivedData \
  DEVELOPMENT_TEAM=SA5NQ48YZF \
  CODE_SIGN_STYLE=Automatic \
  build

APP=build/DerivedData/Build/Products/Release-iphoneos/Zeron.app
DEVICE=5803C649-7866-50F0-A56E-26398A145354   # devicectl identifier

xcrun devicectl device install app --device "$DEVICE" "$APP"
xcrun devicectl device process launch --device "$DEVICE" sh.zeron.Zeron
```

Do **not** use the iOS Simulator destination for day-to-day personal stack work.

---

## Quick checklist (full redeploy)

1. [ ] `git fetch upstream main` + cherry-pick personal deploy commit
2. [ ] `cd edge && npm ci && npx wrangler deploy` (buildr-dev)
3. [ ] Smoke `/auth/exchange` → `missing code` (not 501)
4. [ ] Devbox: pull branch, `cargo build --release -p zeron`, `daemon install`, restart
5. [ ] Mac: `package-macos.sh` → copy `Zeron.app` to `/Applications`
6. [ ] iOS: device Release build → `devicectl install` + launch

## Common failures

| Symptom | Fix |
| --- | --- |
| `wrangler deploy` wrong account | `npx wrangler whoami` — must be buildr-dev, not zeron |
| Auth 501 `workos not configured` | `npx wrangler secret put WORKOS_API_KEY` then redeploy |
| Devbox still on old binary | Rebuild release + restart unit; check `ExecStart=` path |
| Daemon points at zeron edge | Re-run `daemon install` with `ZERON_EDGE_URL` / `ZERON_WORKOS_CLIENT_ID` set |
| iOS install can’t reach phone | Unlock phone, trust computer, ensure `devicectl list` shows available |
| Journal `unknown variant grok-build` | Harmless legacy journal lines; upstream harness id is `grok` |

## Credential map (non-secret)

| Name | Value |
| --- | --- |
| Edge URL | `https://comet-native-edge.buildr-9df.workers.dev` |
| WorkOS client id | `client_01KYXECAJHZ0A07VVNDKMWV3X5` |
| CF account id | `9df6ecdbc507977b8256c6462ffff9b3` |
| Worker name | `comet-native-edge` |
| Devbox device name | `devbox` |
| iOS bundle id | `sh.zeron.Zeron` |

Secrets (API key) stay in wrangler secrets / `edge/.dev.vars` only.
