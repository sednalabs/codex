# codex-app-server-daemon

> `codex-app-server-daemon` is experimental and its lifecycle contract may
> change while the remote-management flow is still being developed.

`codex-app-server-daemon` backs the machine-readable `codex app-server`
lifecycle commands used by remote clients such as the desktop and mobile apps.
It is intended for Codex instances launched over SSH, including fresh developer
machines that should expose app-server with `remote_control` enabled.

## Platform support

The daemon supports Linux, macOS, and Windows using platform-specific process
and file-locking primitives. Windows startup requires a non-elevated terminal
whose host permits detached child processes.

Windows automatic attachment requires the canonical socket address to fit the
108-byte AF_UNIX limit (including its terminator). A short junction alias whose
resolved address exceeds that limit falls back to the embedded server. Use a
shorter `CODEX_HOME` to share the daemon; discovery does not trust a mutable alias.

Shared clients use the environment inherited when the daemon started. Opening a
new terminal or clearing variables there does not clear the running daemon's
environment; per-client environment isolation is not provided.
An invocation that sets `CODEX_EXEC_SERVER_URL` skips implicit daemon attachment
so its executor selection is preserved. If an implicitly discovered daemon cannot
initialize the connection, the TUI starts an embedded server instead. Explicit
`--remote` endpoints remain authoritative and report connection failures.

## Commands

```sh
codex app-server daemon start
codex app-server daemon restart
codex app-server daemon update
codex app-server daemon enable-remote-control
codex app-server daemon disable-remote-control
codex app-server daemon stop
codex app-server daemon version
codex app-server daemon bootstrap --remote-control
```

On success, every command writes exactly one JSON object to stdout. Consumers
should parse that JSON rather than relying on human-readable text. Lifecycle
responses report the resolved backend, socket path, local CLI version, and
running app-server version when applicable.

Eligible managed daemons check for updates after five minutes, then hourly by
default. Edit `CODEX_HOME/app-server-daemon/settings.json` to change this:

```json
{"remoteControlEnabled": false,
 "shutdownGraceSeconds": 60,
 "updater": {"autoUpdateEnabled": false, "updateIntervalMinutes": 120}}
```

Positive minute intervals have no configured cap. `daemon restart` applies the
enabled state; the next updater wait reads a new interval. The preference does
not affect an explicit `codex update` command or `daemon update`.

`daemon update` selects the latest stable release, even with automatic updates
disabled. It also returns pinned or local managed packages to production update
eligibility, preserving the automatic-update preference. Legacy installations
migrate to the dedicated root once the published installer and release support
migration. JSON reports `updated`, `noUpdate`, or `unsupported`, with installed
and running versions. A running daemon restarts, so active or queued work may be
interrupted; a stopped daemon stays stopped. Installer errors return nonzero.
The updater uses saved network settings; CLI `-c` overrides do not reach it.

For all managed app-server shutdowns, including explicit stop and restart and
updater-triggered restarts, `shutdownGraceSeconds` defaults to 60 and accepts
an integer from 0 through 300. Zero forces shutdown immediately after requesting
a graceful exit; the five-minute maximum bounds the wait even if a turn is still
running.

## Bootstrap flow

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
For a new remote machine running a stable Sedna Linux release, choose and copy
the exact release tag from the [Sedna releases page](https://github.com/sednalabs/codex/releases),
then install that fork-owned release before bootstrapping:
=======
For a new Linux or macOS machine:
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

```sh
release_tag='v0.124.0-sedna.2' # replace with the exact selected release-page tag
curl -fsSL https://raw.githubusercontent.com/sednalabs/codex/main/scripts/install_sedna_release_asset \
  | CODEX_NON_INTERACTIVE=1 bash -s -- \
      --repository sednalabs/codex \
      --release-tag "$release_tag"
$HOME/.codex/packages/standalone/current/codex app-server daemon bootstrap --remote-control
```

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
`bootstrap` requires the standalone managed install. It records the daemon
settings under `CODEX_HOME/app-server-daemon/`, starts app-server as a
pidfile-backed detached process, and launches a detached updater loop only
when the release is eligible for the automatic Sedna channel. The persisted
bootstrap marker means app-server has completed initial setup; it does not
replace updater reconciliation.

## Installation and update cases

The daemon always resolves `current/codex` to a canonical executable inside
`CODEX_HOME/packages/standalone/releases/` before it launches app-server or an
updater. Automatic updates are available only for stable Sedna releases on
Linux `x86_64` and Linux `aarch64` that were installed through the fork-owned
standalone release installer. The daemon verifies the resolved release's
`RELEASE-METADATA.json` and executable against the installer-written
`INSTALLED-SHA256SUMS.txt` manifest, then validates its repository, version,
and target. The binary that
invokes `bootstrap` does not grant update authority to a different managed
release.

| Situation | What starts | Does this daemon fetch new binaries? | Does a running app-server eventually move to a newer binary on its own? |
| --- | --- | --- | --- |
| A managed standalone release is installed, but only `start` is used | `start` resolves the canonical executable from `CODEX_HOME/packages/standalone/releases/` | No | No. The managed release is used when starting or restarting, but no updater is installed. |
| An eligible stable Sedna Linux release is installed, then `bootstrap` is used | The pidfile backend uses the canonical executable selected from `CODEX_HOME/packages/standalone/releases/` | Yes. Bootstrap launches a detached updater loop that resolves the fork release candidate hourly. The installer accepts it only when its strict Sedna version is stable and newer than the running release. | Yes, while that updater process is alive and app-server is already running. After a successful eligible update, the updater revalidates the final managed release, restarts app-server with its canonical executable, and only then replaces its own process image. |
| Prerelease, macOS, package-manager, or unsupported installation | Lifecycle commands use a canonical managed release if one is present | No automatic action. Select and install a compatible release manually. | No. The daemon does not select or activate an automatic release for these installation classes. |
=======
On Windows, use a non-elevated PowerShell terminal whose host allows breakaway:

```powershell
irm https://chatgpt.com/codex/install.ps1 | iex
$codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $HOME '.codex' }
& "$codexHome\packages\standalone\current\bin\codex.exe" app-server daemon bootstrap --remote-control
```

`bootstrap` can use any complete CLI package. If no daemon package is installed,
it copies the invoking package into `CODEX_HOME/packages/app-server-daemon` and
prints an installation message without asking for confirmation. Existing daemon
packages are reused, including legacy installations; a broken selection is not
silently replaced. A bare executable cannot supply a new installation.

It records the daemon settings under `CODEX_HOME/app-server-daemon/`, starts app-server as a
pidfile-backed detached process. It launches a detached updater loop when
automatic updates are enabled, the installer selected the stable `latest`
channel, and the managed binary supports the updater command.

## Installation and update cases

New daemons use `CODEX_HOME/packages/app-server-daemon/current/bin/codex`
(`codex.exe` on Windows). The package contains the executable and its helpers.
Daemon-only installer updates leave the user's CLI command and shell setup alone.

Previously launched legacy daemons retain `CODEX_HOME/packages/standalone/current`,
including its flat binary layout when present. Starts and scheduled updates keep
using that location. An explicit production update prepares and validates a
compatible dedicated package before stopping the legacy updater and daemon,
selecting the new package, and restarting only a previously running daemon.
The old CLI package files and selection remain unchanged.

| Situation | What starts | Does this daemon fetch new binaries? | Does a running app-server eventually move to a newer binary on its own? |
| --- | --- | --- | --- |
| Latest-channel installer has run; `start` or `bootstrap` is used with automatic updates enabled | Managed binary and detached updater when supported | When supported, the platform's installer runs on the configured cadence. | When supported, the running server restarts with the new binary before the updater replaces itself. |
| Installer selected an explicit release; `bootstrap` is used | Managed binary only | No; the selected release stays pinned. | No; an explicit restart uses the selected binary. |
| Another tool updates the managed binary | A fresh start or explicit restart uses it; a running server is reused. | Yes, when a latest-channel updater is running, on the configured cadence. | An updater that was running through the change compares binary contents on its next successful installer pass and refreshes the server first. |
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

### Managed packages

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
For eligible stable Sedna Linux installs created by the fork-owned standalone
release installer:

- lifecycle commands launch the canonical executable selected from the
  standalone releases root
- `bootstrap` is supported
- `bootstrap` starts a detached pid-backed updater loop that resolves `latest`
  and accepts only a strictly newer stable Sedna release
- after a successful eligible refresh, if app-server is running and the managed
  binary contents changed, the updater restarts app-server with that binary
  first and only then replaces its own process image
- the updater loop is not reboot-persistent; a later `codex remote-control`
  start restores a missing eligible updater and app-server without repeating
  bootstrap

Prerelease, macOS, package-manager, and unsupported installations are
manual-only. Choose the exact release tag from the Sedna releases page; use an
explicit prerelease allowance or macOS preview selector only when the selected
release requires it.
=======
For dedicated and retained legacy daemon installations:

- lifecycle commands use the selected daemon package, regardless of the invoking
  CLI version; they do not implicitly replace an existing package
- `bootstrap` is supported
- managed `start`, `restart`, and `bootstrap` ensure a single detached pid-backed
  updater loop only when automatic updates are enabled for a stable latest-channel
  release whose managed binary supports the updater command
- the installer records the latest-channel selection alongside `current`;
  selecting an explicit release clears it, even if that version is currently
  latest. The updater checks the selection again while holding the install lock
  so an in-flight update cannot override a new pin
- installs made before the installer recorded channel selections need one new
  `latest` installation to opt into automatic updates; until then the daemon
  continues to serve app-server without updating the selected release
- after a successful refresh, if app-server is running and the managed binary
  contents changed, the updater restarts app-server with that binary first and
  only then replaces its own process image
- the updater loop is not reboot-persistent; a managed start after reboot
  starts it again
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

### Out-of-band updates

This daemon does not watch arbitrary executable files for replacement. If some
other tool updates the managed binary path:

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
- without `bootstrap`, a currently running app-server remains on the old
  executable image until an explicit `restart`
- with `bootstrap` on an eligible stable Sedna Linux install, the detached
  updater loop notices the changed managed binary on its next successful
  selected-candidate pass; if app-server is running, it refreshes app-server
  first and then refreshes itself once that replacement starts successfully
=======
- an updater that was already running notices a changed managed
  binary on its next successful scheduled installer pass; if
  app-server is running, it refreshes app-server first and then refreshes itself
  once that replacement starts successfully
- if the updater was absent during a same-version binary replacement, a later
  managed start recovers it but cannot infer the running server's previous
  executable identity; use `codex app-server daemon restart` to refresh the server
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

## Lifecycle semantics

`start` is idempotent and returns after app-server is ready to answer the normal
JSON-RPC initialize handshake on the Unix control socket.

`restart` stops any managed daemon and starts it again.

`enable-remote-control` and `disable-remote-control` persist the launch setting
for future starts. If a managed app-server is already running, they restart it
so the new setting takes effect immediately.

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
Top-level `codex remote-control` bootstraps with `--remote-control` only before
initial setup. On every later start it preserves a running app-server and
reconciles the updater against the current validated release: it starts a
missing eligible updater, preserves an eligible updater only when its recorded
executable identity matches the current release, replaces a stale eligible
updater, stops an ineligible running updater, and leaves an ineligible missing
updater absent.
This also restores both services after a reboot when the current release is
eligible.
=======
Top-level `codex remote-control start` enables and persists remote control for
the managed daemon, overriding a saved disabled value. It starts or bootstraps
the daemon as needed. Plain `codex remote-control` runs a separate foreground
server and does not change daemon settings; `codex remote-control stop` stops
the managed daemon without clearing its saved remote-control preference.
`daemon start` and `daemon restart` use that saved preference. `daemon bootstrap`
sets it according to `--remote-control` (disabled when omitted).
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

`stop` sends a graceful termination request first, then force-terminates the
process after the configured grace window if it is still alive.

All mutating lifecycle commands are serialized per `CODEX_HOME`, so a concurrent
`start`, `restart`, `enable-remote-control`, `disable-remote-control`, `stop`,
or `bootstrap` does not race another in-flight lifecycle operation.

## State

The daemon stores its local state under `CODEX_HOME/app-server-daemon/`:

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
- `settings.json` for persisted launch settings and bootstrap completion state
=======
- `settings.json` for remote-control launch settings and updater preferences
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
- `app-server.pid` for the app-server process record
- `app-server-updater.pid` for the pid-backed standalone updater loop
- `daemon.lock` for daemon-wide lifecycle serialization
