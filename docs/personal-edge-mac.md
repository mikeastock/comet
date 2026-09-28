# Personal edge on a Mac

Get the Mac app talking to the personal edge
(`https://comet-native-edge.buildr-internal-tools.workers.dev`).

The app has no edge-URL setting. Sign-in uses the URL and WorkOS client id
compiled into the binary, unless `ZERON_EDGE_URL` and `ZERON_WORKOS_CLIENT_ID`
are set in that process's environment. A double-clicked production `Zeron.app`
keeps using `https://edge.zeron.sh`.

This branch bakes the personal endpoint, so the build below is the path that
survives opening the app from Finder. The [env-var path](#point-a-stock-binary)
is for a binary you already have.

The WorkOS API key lives only as a wrangler secret on the worker. It is not
needed on the Mac, and it does not belong in this repo.

Worker deploy and the devbox daemon are in
[personal-edge-runbook.md](personal-edge-runbook.md). iOS sign-in is also
compiled in; there is no URL field. Build that app from this same branch
(section 4 of that runbook).

## Prerequisites

- This branch, `personal-edge-redeploy`, from `github.com:mikeastock/comet`
- Rust (`cargo` on `PATH`, or `~/.cargo/bin`)
- Xcode command line tools (`iconutil` and `sips` come from there)

On the Mac the fork is `origin` and zeronsh/comet is `upstream`. The devbox
uses the opposite names.

## 1. Check out the branch

```sh
git fetch origin personal-edge-redeploy
git checkout personal-edge-redeploy
```

Confirm the baked endpoint:

```sh
rg 'DEFAULT_EDGE_URL|WORKOS_CLIENT_ID' apps/zeron/src/main.rs
```

Expect `https://comet-native-edge.buildr-internal-tools.workers.dev` and
`client_01KYXECAJHZ0A07VVNDKMWV3X5`.

## 2. Package and install

From the repo root:

```sh
bash scripts/package-macos.sh
```

That release-builds `zeron` and writes:

| Path | Use |
| --- | --- |
| `target/package/Zeron.app` | local install |
| `target/package/zeron-<ver>-macos-<arch>.dmg` | shareable install |

Quit Zeron if it is open, then replace the installed app:

```sh
osascript -e 'quit app "Zeron"' || true
rm -rf /Applications/Zeron.app /Applications/Comet.app
cp -R target/package/Zeron.app /Applications/Zeron.app
```

The package is ad-hoc signed unless `CODESIGN_IDENTITY` is set. The first
launch may need right-click → Open under Gatekeeper.

Confirm the installed binary:

```sh
strings /Applications/Zeron.app/Contents/MacOS/zeron \
  | rg 'buildr-internal-tools|client_01KYX'
```

## 3. Drop any old session, then sign in

The app and a running daemon share `~/.zeron` and port 27654. If something is
already listening, the new app attaches to that engine and ignores the URL
baked into the app you just installed.

Stop a leftover service, using the new binary:

```sh
/Applications/Zeron.app/Contents/MacOS/zeron daemon stop || true
/Applications/Zeron.app/Contents/MacOS/zeron logout || true
```

Open `/Applications/Zeron.app` and use Sign in. That talks to the personal
edge with the client id baked into this branch.

Check from a terminal:

```sh
/Applications/Zeron.app/Contents/MacOS/zeron status
```

A signed-in account should name your email. Sync starts on the next engine
start, so quit and reopen the app after the first sign-in if the window is
still local-only.

This app looks for updates on the personal edge. That releases bucket is
empty, so the updater will not replace it with the public build.

## Point a stock binary

Skip the rebuild only when you can launch a process with both variables set.
Finder and `open` do not pass them through. Run the executable directly:

```sh
export ZERON_EDGE_URL=https://comet-native-edge.buildr-internal-tools.workers.dev
export ZERON_WORKOS_CLIENT_ID=client_01KYXECAJHZ0A07VVNDKMWV3X5

# quit any app or daemon already bound to port 27654 first
"$ZERON_BIN" logout || true
"$ZERON_BIN"
```

`$ZERON_BIN` is the `zeron` executable you want to run, for example
`/Applications/Zeron.app/Contents/MacOS/zeron`. Setting only the URL is not
enough: a production binary still presents the production WorkOS client, and
this edge will reject that login.

`zeron daemon install` records whatever `ZERON_*` variables are set into the
launchd unit. On this branch the binary already has the personal defaults, so
the variables are optional for a build from this checkout and required for a
stock binary.
