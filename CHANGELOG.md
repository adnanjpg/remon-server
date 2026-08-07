# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.20.2] - 2026-08-07

### Added

- **Windowed aggregates, so `for_duration_secs` means what it looks like it means.** The evaluator compares the *instantaneous* value at each tick, which quietly makes a rule's sensitivity a function of its poll rate rather than its threshold: a spike shorter than `eval_interval_secs` is either missed outright or seen exactly once, and a rule needing two consecutive violating ticks can then never fire on it however severe it is. Measured on a live host — `cpu.usage_percent > 80`, `for 10s`, `eval 10s`, samples every 2s — 24 hours held 43,991 samples, exactly two above the threshold, neither consecutive; a day with fourteen threshold crossings peaked at a 26.8% five-minute average. Seventeen days produced twelve incident captures and no alert events, and nothing in the state row said the rule had stopped being able to fire. `max(…)`, `min(…)` and `avg(…)` over a window now read every raw sample in the span instead of the one that landed under the poll: `max(cpu.usage_percent, 30s) > 80`. Windows cap at an hour and are limited to the namespaces that keep sample history, which the rule editor learns from `GET /alerts/schema`; a window over one that doesn't is a 400 at write time rather than a warning on every tick.

- **An outbound dead-man's switch, for the one outage this daemon cannot report itself.** Every alert rule is evaluated by a task inside the process, so when the host dies the evaluator dies with it: the outage produces no notification at all and surfaces only afterwards, from the boot event written once it is already over. Observed on a live host — a nine-minute reboot window passed with a correct `clean_shutdown` verdict, a matching metric gap and a `host_rebooted` marker, and not one thing sent while it was happening. `[liveness]` inverts the direction: remon pings a push endpoint on a schedule and *absence* becomes the signal, so healthchecks.io, Uptime Kuma or Better Stack raises the alarm on remon's behalf. Off by default; a non-empty `url` that is unparseable, non-HTTP(S), or paired with a timeout at or above the interval fails boot rather than leaving the operator believing they are covered. A ping carries no health verdict on purpose — gating it on host health would fold a sick host and a dead one into the same silence. This is not the existing `heartbeat_checks`, which is the inbound mirror and stops when we do.

## [0.19.0] - 2026-07-28

### Fixed

- **The kernel-journal sweep could not see the thing it exists to find.** `journalctl -k` is documented to imply `-b`, restricting every read to the current boot — while the startup lookback exists to catch the OOM kill or panic behind an unclean restart, which is by definition in the boot *before* this one. Measured on a live host: over the same 60-day window `-k` returns 39,751 kernel lines and an explicit `_TRANSPORT=kernel` match returns 236,406, and the two real OOM kills in that journal are invisible to the first and found by the second. Three more blindnesses went with it. A transient failure on the first tick disabled the sweep for the life of the process, and the first tick lands during boot with the log daemon still replaying — so the most likely moment to fail was also the most expensive; only a missing tool ends the sweep now, everything else gets the next tick. A non-empty stderr was fatal even when entries came back on stdout, which is exactly what journald does when it skips a truncated file after a power cut, so stderr now only fails a scan that returned nothing. And the cursor moved with the wall clock rather than with what was actually recorded: events past the per-tick cap were dropped outright, and anything the journal flushed while the scan was running was stepped over. The cursor is a high-water mark of what was handled now, and an empty tick stays a grace period behind `now`.
- **`GET /events` returned an empty page when a filter was busy.** `kinds` and `sources` were applied to the merged result, but each store had already capped its read at `limit` — so a window in which the unwanted kind filled the limit on its own answered `200` with `count: 0` while matching events sat just outside it. Both filters push into SQL now. The existing tests could not have caught it: every one of them stays under the limit.
- **A rule disabled while pending came back with its `for` window already spent.** Rehydration restored `state_since` verbatim, so a rule that sat pending while switched off — or through a long outage — went straight to firing on its first sample, with a notification claiming the condition had been sustained. Time nobody was evaluating cannot count toward a window that means "held continuously while watched", so the anchor now moves forward by the unobserved gap: a two-second restart keeps the evidence it had, a three-day gap keeps none of it. Firing rows are untouched — there `state_since` answers "since when", which is a fact about the incident rather than a countdown.

### Changed

- **Notifications are delivered by one owned task instead of inline.** The evaluator awaited the fan-out for every transition, and a wedged Web Push relay can take its full 30-second budget; twenty label sets crossing at once meant ten minutes in which no rule was evaluated at all. Producers now hand the notification to a bounded queue and return. A queue rather than a task per notification because ordering is not optional here — an operator must never be told a rule recovered before being told it fired — and because a queue can be drained at shutdown, which it now is. **`alert_events.notified` changes meaning slightly:** the row is written claiming nothing and the flag is raised only once a channel has accepted the notification. Every failure — a dropped queue entry, a dead channel, the process dying mid-flight — therefore leaves it false. That is the safe direction for a field an operator reads while asking "was anyone told about this?".
- **The host-event ledger is written by one owned task too.** Spawning per row raced process exit, and the row most likely to be lost was the restart/shutdown audit itself — written moments before the process ended. The writer drains on shutdown and keeps accepting *during* the drain, so `/system/shutdown` reporting the outcome of stopping its own unit still lands. Rows are stamped when they are queued, not when the writer reaches them, so a backlog cannot file events under the wrong moment. Attribution moved into the writer as well: the request path no longer does a device lookup.
- **Background loops stop when the server is stopping.** Only the SSE streams watched the shutdown signal. Every collector, the rollup and retention passes, session cleanup, the event sweep and the alert evaluator kept starting fresh work until the runtime dropped them mid-flight — the evaluator firing rules on a half-stopped host among them. They observe it now. The two tasks with queued work (ledger, notifications) are awaited by `main`; the periodic loops have nothing to flush, so stopping promptly is the whole requirement.
- **The evaluator's throttles run on a monotonic clock.** Reload and per-rule intervals were compared against wall time, so a backwards step — an NTP correction, a VM resuming from a snapshot — made `now - last_eval` negative and held every rule below its interval until real time caught up: alerting stopped for the length of the jump and nothing said so. Persisted deadlines (notification cooldown, operator silence) stay on the wall clock, which is what they mean.

### Added

- **WebSocket exec sessions are audited.** Opening a shell in a container is the most privileged thing this server does and was the only one leaving no ledger row, while alert silencing and probe reloads both wrote one. Each accepted session now records a `container_exec` row naming the device, the container and the argv. The endpoint still has no per-token scope check — that gap is unchanged, and the ledger is what stands in for it until scoped tokens exist.

## [0.18.2] - 2026-07-27

### Security

- **The Windows data directory was readable by every local user, and reachable before the installer ever ran.** `%ProgramData%\remon` was created with `New-Item` and left on ProgramData's inherited ACL, which on a stock Windows 11 grants `BUILTIN\Users:(OI)(CI)(RX)` — read and execute on everything in it, files included. That directory holds the database, so the JWT signing secret and every token hash were readable by any account on the box, as was the log the pairing code is printed to. The Linux installer has always made its data directory `0700`; Windows had no equivalent. The same inherited ACL also carries `BUILTIN\Users:(CI)(WD,AD,WEA,WA)` — any user may create entries under ProgramData — and `CREATOR OWNER:(OI)(CI)(IO)(F)` gives whoever creates a directory there full control of its contents. So a user who created `%ProgramData%\remon` before an administrator ever ran the installer owned the wrapper script that the startup task then executed as SYSTEM. The directory now gets an explicit ACL — inheritance broken, SYSTEM and Administrators only, applied before anything is written into it — and an existing directory owned by neither those nor the installing account is refused rather than adopted, since a `config.toml` planted in it would otherwise be honoured (`smart.smartctl_path` alone is enough to run a chosen binary as SYSTEM). Well-known SIDs are used rather than account names, which are localised.
- **A release that published no checksums installed anyway.** Both installers warned and continued when `SHA256SUMS` could not be fetched — and the Linux one did the same when `sha256sum` was not present — which left nothing tying the downloaded bytes to the tag that was asked for, on a path that then runs as root or SYSTEM. Someone installing through `curl … | sudo sh` never sees the warning. Being unable to verify is now a stop; `REMON_ALLOW_UNVERIFIED=1` (or `-AllowUnverified`) is the deliberate way through, needed only for releases that predate the current pipeline. Verifying and *failing* was already fatal and is unchanged.

### Fixed

- **A failed upgrade left the service stopped and the binary already replaced.** The order was stop, replace, then validate the configuration — so a config the new build rejected exited with "configuration did not validate; nothing was enabled" after the old binary was gone and the service was down, and nothing restarted it. `was_running` was tracked but only ever used to pick a closing message. Validation now runs against the new binary, from the scratch directory, before anything is stopped or overwritten: a rejected config leaves the existing install running and untouched, and says so.
- **`REMON_NO_SERVICE=1` switched off a running agent and reported success.** The stop happened before the flag was read, and the flag's branch exits without starting anything — so an operator asking to refresh only the binary got a green "Installed." and a host that was no longer being monitored. Skipping service *setup* now leaves a service that was running running, on the new binary. `-NoService` on Windows had the same shape and the same fix.
- **`uninstall.sh --purge` deleted the wrong directories.** It honoured `REMON_PREFIX` but hardcoded `/etc/remon` and `/var/lib/remon`, while the installer accepts `REMON_CONFIG_DIR` and `REMON_DATA_DIR` — so on a host installed with either of those, `--purge` removed paths that install had never used, left the real database and configuration in place, and reported that it had removed "configuration and metrics history". Both overrides are read now, and the OpenRC log, which purge never touched, is removed with them.

## [0.18.1] - 2026-07-27

### Security

- **`DELETE /processes/{pid}` could kill every process on the host.** The path segment is parsed as `u32`, but Unix `kill(2)` takes a signed `pid_t`, so any value above `i32::MAX` narrowed to a *negative* pid — and a negative pid is a broadcast, not a target. `DELETE /processes/4294967295` became `kill(-1, …)`: every process the caller may signal, which for a unit that deliberately runs as root is the entire machine. The three guards added in 0.18.0 all waved it through — it is not pid 0, not pid 1, and not the server's own pid — and the request read as an ordinary kill of a process that had already exited, so a client-side integer bug reached it as easily as a deliberate call. Rejected in `kill_process` rather than the handler, so every caller inherits the check; Linux caps `pid_max` at 2^22 and no Unix allocates pids near `i32::MAX`, so no reachable process is lost to the range.
- **Notification deliveries followed redirects, walking straight past the outbound URL policy.** `check_url` vets the URL a channel is *configured* with — rejecting loopback, link-local, and private ranges unless the operator allows them — but the shared `reqwest` client was built without a redirect policy, so it followed the default ten hops and none of them were re-checked. An allowed host answering `302 Location: http://169.254.169.254/…` reached exactly the range the policy exists to block. The response body of a non-2xx reply is logged at `warn!`, which the default `log_insertion_level` persists into the `logs` table, so what came back was readable afterwards through the API. The client now refuses redirects outright; a channel that needs one should be configured with the final URL.

### Fixed

- **The assistant's answer stream could still block shutdown for minutes.** 0.17.2 made every infinite SSE stream shutdown-aware and explicitly exempted this one, on the grounds that it terminates on its own — which is true, but "on its own" means the whole tool-use loop draining, up to the step cap times the per-step provider timeout. That outlives systemd's stop timeout, so a restart with one question in flight got SIGKILLed, `mark_clean_shutdown` never ran, and the next boot was misfiled as an unclean exit — the precise failure 0.17.2 set out to close, left open in the one stream it excluded. The router's `TimeoutLayer` was no help: tower-http races the response future, not the body. Now wrapped in `until_shutdown` like the other seven.
- **A rule disabled while firing kept its badge forever.** `list_active_state` joined `alert_rules` without filtering on `enabled`, and the `/summary` count did not join at all. Disabling a rule stops the evaluator from touching its `alert_state` row but does not clear it, so the stale row stayed active in both — leaving a red badge on every dashboard that nothing short of deleting the rule could clear. Both now filter on `enabled`; the row is still kept, so re-enabling the rule resumes where it left off.

## [0.18.0] - 2026-07-26

### Fixed

- **The server could switch off the monitoring it was providing, and stay off.** `DELETE /processes/{pid}` defaults to SIGTERM, the server handles SIGTERM as a graceful shutdown and returns 0, and the systemd unit ran `Restart=on-failure` — which does not restart a success. So killing the agent from the process list *it renders*, where it sits near the top having just done the work to build that list, turned monitoring off until a human noticed. The gentler signal was the more destructive one: SIGKILL counts as a failure and did come back. Stopping or disabling its own unit through `/services/{name}` was worse still, surviving reboots. Fixed at both levels: every supervisor now restarts the agent however it went down (`Restart=always` on systemd, `supervise-daemon` on OpenRC — which previously had no supervision at all, since `command_background` hands off to `start-stop-daemon` and forgets the process), and the generic endpoints return **409** when a request names this server, pointing at the deliberate alternative. The systemd start rate limit is dropped with it: five starts in ten seconds then a permanent failed state turns a transient problem — a full disk, a volume not mounted yet — into monitoring that is off and silent about it.
- **Container/image prunes and image deletes left no audit trail.** All three ran with `_claims` — they deleted an unbounded set and recorded nothing about who did it or what had been there, while every other mutating endpoint wrote to the ledger. Now they do too.

### Added

- **A one-line installer, a systemd unit, and the CLI an installed server needs.** `curl -fsSL .../packaging/install.sh | sudo sh` resolves the build for the machine, verifies it against the published `SHA256SUMS`, installs to `/usr/local/bin`, writes `/etc/remon/config.toml` only when there is not one already, validates it, and starts an enabled service — then waits on `/health` so a crash loop is reported instead of discovered later. Re-running it is the upgrade path; configuration and the database are never touched. `uninstall.sh` keeps both unless given `--purge`. The unit runs as root deliberately (restarting services, killing processes and reading SMART do not work otherwise, which also rules out `User=`/`NoNewPrivileges`) and carries the hardening that does not conflict with that job, plus a stop timeout long enough for the SSE drain to record a clean shutdown. The binary itself gained `--version`, `--help`, `--config-dir`, `--data-dir`, `config check` (validate and exit — what the installer gates on) and `doctor` (paths, writability, bind address and port availability, privileges, init system, smartctl, Docker socket; opens nothing and mutates nothing, so it is safe against a live install). Hand-rolled argument parsing — no new dependency.
- **A fresh install now says what to do next.** Until a device has paired, every boot prints the reachable addresses and how pairing starts. A wildcard bind says nothing about how to reach the host, so the address the routing table would use to leave the machine is resolved and offered alongside loopback; no packet is sent, a connected UDP socket only records a peer and performs the route lookup. This lives in the server rather than the installer because a host set up from a package, a container image or an unpacked tarball never runs one.
- **Windows release artifacts, and a way to start them at boot.** The SCM service backend and the PowerShell event collectors have been in the tree for several releases with nothing published to run them on; `remon-server-windows-amd64.zip` ships now, with `install-windows.ps1` mirroring the Linux installer (resolve, verify, place under `Program Files` with state in `ProgramData`, validate, start, wait on `/health`). It registers a startup task rather than a service: a Windows service has to speak the Service Control Manager protocol from inside the process, and remon-server is a plain console program, so `sc.exe create` would register something that starts and immediately dies with error 1053. A task running as SYSTEM at boot with restart-on-failure and no execution time limit delivers what the service was wanted for; implementing the protocol properly remains open. The task runs through a generated `.cmd` wrapper because a scheduled task cannot redirect output and stdout is where the pairing code goes — the wrapper rotates that log past 10 MB.
- **OpenRC installs as a service too.** The server has driven OpenRC through its service endpoints since before this release, and Alpine is a natural target for a static musl binary, but the installer only knew systemd and left Alpine hosts with a bare binary. It now detects either init and installs the matching service, reporting the right log location for each (journal under systemd, `/var/log/remon-server.log` under OpenRC). `REMON_CONFIG_DIR` and `REMON_DATA_DIR` join `REMON_PREFIX` as installer overrides, and the shipped unit and init script are retargeted to whatever those resolve to.
- **`POST /system/restart` and `POST /system/shutdown`.** The deliberate counterparts to what the generic control endpoints now refuse. Restart is how boot-time configuration (listen port, log format, CORS origins) gets applied without shell access to the host; shutdown is for an agent misbehaving on a machine you are not sitting at. Both answer `202` before acting — a handler that ended the process inline would never get its response out — then run the ordinary drain, so in-flight requests finish, SSE streams close, and the clean-shutdown marker is written. Restart exits **75**, not 0: a Windows scheduled task only restarts an action that failed. Shutdown cannot merely exit for the same reason, so it asks the supervisor to stop the unit, and where nothing supervises the process it exits — the reply says which of the two happened, since the client cannot see an exit code. New `[control] own_service` config lets an operator name the unit where auto-detection (`RC_SVCNAME`, cgroup path) cannot see it.
- **An installer test suite** (`packaging/tests/install-test.sh`). Runs `install.sh` against stubbed system commands and checks both init branches, the checksum gate, an upgrade re-run leaving an edited config alone, and installing from a release that predates the bundled unit files. The installer is the artifact hardest to test on the machine it is written on, and this covers the parts that are pure script.

### Fixed

- **The published release artifact could not start.** The tarball contained a lone executable, but `config/default` was a *required* config source — extracting it anywhere without a repo checkout beside it died at boot with `configuration file "config/default" not found`, so the only working path was cloning the repository. The defaults are now compiled into the binary via `include_str!` and the on-disk copy is one more optional override layer, leaving the checkout workflow byte-for-byte unchanged. An empty CORS allow-list is no longer fatal either: it is the tightest possible policy (no browser origin may call the API, native clients unaffected) and the shipped default, so it boots and logs a warning instead of refusing to start — a fresh install has no frontend origin to name yet.
- **Linux builds would not run on Debian 12, Ubuntu 22.04 or RHEL 9.** The artifacts were glibc builds produced on `ubuntu-24.04`, so they inherited that image's glibc 2.39 requirement; glibc is backward compatible, not forward, and those distros ship 2.34–2.36. Linux now builds against musl and CI asserts the result is statically linked, so the binary has no libc floor and no runtime dependency to install. The release pipeline also gained a smoke test that runs `--version` and `config check` from a directory holding nothing but the binary — the failure this pipeline exists to catch.

### Changed

- **Config, database and probes are no longer resolved relative to the working directory.** A daemon has no meaningful working directory, so an installed server had nowhere sensible to put its files. The layout resolves once at startup: `--config-dir`/`--data-dir` or `REMON_CONFIG_DIR`/`REMON_DATA_DIR` win; a working directory containing `config/default.toml` keeps the existing relative paths, so `cargo run` in a checkout is unchanged; otherwise `/etc/remon` and `/var/lib/remon` when privileged, XDG directories when not, `%ProgramData%\remon` on Windows. Configured paths that are relative resolve against the data directory and absolute ones are honoured as given, so the database can sit on its own volume. Adds `config.toml` to the layer order — the file an installed server edits, as distinct from `default.toml`, which mirrors the compiled-in defaults. No schema change.

## [0.17.2] - 2026-07-22

### Security

- **Notification-channel credentials could leak into the server log on a delivery failure.** Every channel (`telegram`, `ntfy`, `webhook`, `fcm`, `webpush`) builds its outbound request against a URL that embeds a secret — the Telegram bot token is a path segment, the ntfy topic is effectively a bearer-equivalent, an operator's webhook URL may carry a token in its query string, and a Web Push endpoint is itself an unguessable per-subscriber credential. `reqwest::Error`'s `Display` includes the request URL verbatim, and every channel's error path did `e.to_string()` straight into a `warn!()` on send failure — so a single Telegram hiccup (bad chat ID, a transient API blip) would print the bot token to the server log at WARN level. Fixed by calling `.without_url()` on every reqwest error before it's stringified, across all five channels. No config or behavior change — only what lands in the log on failure.

### Fixed

- **`systemctl restart`/`stop` could hang for 90s and get SIGKILLed while an SSE stream was open.** `main.rs` hands axum's graceful shutdown a signal future, which stops accepting new connections but then waits for every in-flight response to finish. The live-stats, container-log, and service-log-follow SSE streams are infinite by design (they run until the client disconnects) and never observed the shutdown signal — so any open dashboard tab or log-follow blocked shutdown indefinitely, until systemd's default `TimeoutStopSec` elapsed and SIGKILLed the process. The SIGKILL had a second, quieter effect: `mark_clean_shutdown` only runs after `serve()` returns normally, so a killed process never set the `clean_shutdown` marker — corrupting the next boot's classification (a plain restart with a live dashboard open would misreport as a crash-restart, and the following real reboot as an unclean shutdown). Fixed with a `watch::Sender<bool>` on `AppState`, flipped inside the shutdown future *before* axum's drain phase begins; every infinite SSE stream (`routes::sse::until_shutdown`) now races its next item against that signal and ends within one poll. `systemctl restart` with open SSE clients now completes in well under a second instead of 90s. The assistant's answer-streaming SSE is unaffected — it already terminates on its own (a `done`/`error` frame closes the channel), so it isn't in the affected set. Binary-only, no schema change.

## [0.17.1] - 2026-07-21

### Fixed

- **The kernel-journal sweep (OOM kills, now also crashes/disk errors) has been silently disabling itself on every single restart since 0.15.0.** `journalctl -g` exits 1 when nothing matches the pattern, but still prints a `-- No entries --` banner to stdout — the sweep's availability check read "non-zero exit + non-empty stdout" as "journalctl is broken" and permanently disabled itself on the very first tick, which is *every* boot unless an OOM kill happened to fall inside the 900s startup lookback. Production has recorded **zero `oom_kill` events since the feature shipped**. Fixed by trusting stderr instead of exit status: a real failure (bad regex, no journal, permission denied) writes to stderr, "nothing matched this window" doesn't. Applied the same fix to the new Windows path pre-emptively. Binary-only, no schema change.

## [0.17.0] - 2026-07-21

### Added

- **Process crashes and disk errors are now detected as host events, cross-platform.** The kernel-journal sweep that caught OOM kills is generalised into a single system-event sweep covering two more families: `app_crash` (a process segfault / general-protection fault) and `disk_error` (I/O or filesystem errors — EXT4-fs, `blk_update_request`, buffer, critical medium/target). It runs on **Windows** too, reading the System + Application event logs via `Get-WinEvent` (disk/NTFS/storage providers → `disk_error`; Application Error → `app_crash`), so the same three kinds land on both platforms; other targets (macOS) are a no-op. `disk_error` is notification-worthy — it pages at **crit** alongside `oom_kill`/`smart_health`/unclean `boot`; `app_crash` is recorded to the ledger and shown on the timeline but stays quiet by default, as process crashes can be routine and noisy. One shared cursor drives the whole sweep (5-minute cadence, 15-minute first-run lookback). Binary-only, no schema change. *(The Windows path is compile-verified but not yet runtime-tested — there's no Windows host in the deploy path; the Linux path is verified on the production VPS.)*

## [0.16.0] - 2026-07-21

### Added

- **Critical host events now notify, no alert rule required.** The `host_events` producers were write-only — an OOM kill, an unclean reboot, or a SMART health failure landed in the ledger and showed on the `/events` timeline, but paged no one unless you'd separately built an alert rule for it. A curated set of system-source kinds now fans out through the existing notification channels the moment it's recorded: `oom_kill` and `smart_health` failures at **crit**, an unclean `boot` at **warn**. Routine lifecycle (`server_started`, a clean reboot, SMART *recovery*) and the operator audit trail stay quiet. Each channel's `min_severity` still applies, so a crit-only channel gets just the OOM/SMART ones. Notifications carry a new `NotificationEvent::HostEvent` type (webhook/web-push payloads emit `"event": "host_event"`); the alert fired/resolved path is unchanged. This is the "set and forget" half of the host-event feature. Binary-only, no schema change.

## [0.15.5] - 2026-07-20

### Fixed

- **A rule disabled and re-enabled within the same server run could lose its Firing/Pending state, silently resuming from Ok.** The 0.15.4 event-driven evaluator drops a rule's in-memory lifecycle when it's disabled (correctly — the rule stops evaluating); re-enabling it later without a restart re-created that lifecycle from scratch instead of restoring it from the `alert_state` row the DB still had (non-Ok rows persist regardless of enabled state). A rule flipped off and back on while firing would go quiet without ever emitting the resolve it's still owed. Startup hydration only ever covered a process restart, not this same-process cycle; `rehydrate_missing` now runs after every reload for any rule that just (re)appeared without a tracked lifecycle. Binary-only, no schema change.

## [0.15.4] - 2026-07-19

### Performance

- **Alert evaluation is now a single event-driven task, not one timer task per rule.** It wakes on the stats collector's signal (a `watch` bump after each `stats_latest` write), falling back to a 2s timer for liveness, and holds all rule lifecycle in memory. `alert_state` is written only on transitions and for active (pending/firing) rows — a healthy host with everything Ok touches the table **zero times per tick** (previously every rule wrote its state every tick). State hydrates from `alert_state` at startup so firing/pending survives a restart without re-firing. Per-rule `eval_interval` is honoured as a throttle; reload picks up rule changes within 30s. Completes the alerting DB-offload (Phase 2, after 0.15.2's indexes and 0.15.3's in-memory resolution). Binary-only, no schema change; the state machine, notifications, incident capture, and `/alerts/*` API are unchanged.

## [0.15.3] - 2026-07-19

### Performance

- **Alert evaluator resolves host metrics from the live in-memory snapshot instead of the DB.** cpu/memory/disk/network/pressure rules on always-present fields (`usage_percent`, `used_bytes`, `used_percent`, network rates, PSI) now evaluate against the `stats_latest` snapshot the collector already maintains — zero DB round-trips per eval tick for the common case. Optional/enriched fields (`steal_percent`, `iowait_percent`, `inode_used_percent`, page faults, …) and the boot window (before the first snapshot) fall through to the DB query, preserving the resolver's latest-non-null fallback exactly. Phase 1 of moving alerting off the DB hot path; the evaluator, state machine, notifications and `/alerts/*` API are unchanged. Binary-only change — no schema change.

## [0.15.2] - 2026-07-19

### Performance

- **Alert evaluator stops full-scanning the raw metric tables every tick.** Added `(resolution, <key>, timestamp)` indexes to the keyed metrics tables (disk, network, docker, components, pressure, smart) so the resolver's "latest value per key" query (`WHERE resolution=? … GROUP BY <key>`) resolves the per-key `MAX(timestamp)` through an index seek instead of scanning the whole raw partition into a GROUP BY temp-b-tree sorter. A profiling pass (perf + heaptrack) pinned that sorter (`sqlite3VdbeSorterWrite`) as the #1 allocation site — ~4M short-lived allocations per minute under a couple of disk rules evaluating every 10s — and the SQLite worker thread as ~76% of process CPU. The index removes the sorter from the query plan; correctness is unchanged (same rows, same latest-non-null fallback). Schema-only change (no runtime behaviour change), folded into the initial migration per the pre-1.0 policy.

## [0.15.1] - 2026-07-19

### Added

- **`server_started` host event** — the daemon now records its own startup on every run, so a gap in the metric series from a plain restart (deploy, manual bounce) is explained on the timeline, not just host reboots. It is `warn` only for a genuine crash-restart (same host boot as the previous run, which never wrote its clean-shutdown marker) and `info` otherwise — including the first run and any start right after a host reboot (where the unclean-ness belongs to the reboot, already carried by the `boot` event). This folds in the former `agent_restart` event: the crash-restart signal is now `server_started` at `warn`.

## [0.15.0] - 2026-07-19

### Breaking

- Two new tables (`host_events`, `runtime_state`) folded into the initial migration (pre-1.0 policy — no migration chaining). Existing databases fail the migration checksum at boot: delete the database folder and re-pair devices after upgrading.

### Added

- **Host-event ledger (`host_events`)** — the discrete-happenings counterpart to the metric series: things that *happened*, recorded once, queryable forever (well, 90 days). Three sources: `system` (detected by the daemon), `operator` (an authenticated device did something through the API, with actor attribution), `agent` (reserved). Ages out via the standard retention engine.
- **`GET /events`** — one normalized timeline unioning the ledger with `alert_events` (projected as `alert_fired`/`alert_resolved`, severity mapped from the rule) and `incident_snapshots` (projected as `incident_captured`, `ref` pointing at the bundle id). Same range contract as `/metrics/*` (`start`/`end`/`limit`, default last 24 h), plus `kinds=` / `sources=` CSV filters — built to be drawn straight onto charts as annotation markers/bands and to back a timeline feed.
- **Boot / powercycle detection** — on startup the daemon compares the host's boot time against the persisted previous value: a change records a `boot` event *stamped with the actual boot moment* (so the annotation lines up with the gap in the charts), flagged `warn` when the previous run never wrote its clean-shutdown marker — power loss, crash, or hard reset. Same-boot restarts after an unclean exit record `agent_restart`.
- **OOM-kill sweep (Linux)** — a 5-minute kernel-journal scan turns "Out of memory: Killed process" lines (global and cgroup) into `oom_kill` events with pid/name, stamped at kill time and cursor-deduped across restarts. The only real answer to "why did my process vanish at 03:12".
- **SMART health transition events** — the SMART collector now diffs each device's verdict against the last known one (seeded from the DB, so failures that happen while the daemon is down are still caught): `passed→failed` records an error-severity `smart_health` event, recovery records `info`.
- **Operator audit trail** — mutating endpoints record who did what: service/timer actions, container lifecycle, process kills (with the process name when the snapshot has it), alert silence/unsilence, probe reloads, runtime-config/retention/resolution changes, device pairing and revocation. Attribution comes from the JWT device identity; the ledger write is fire-and-forget and never fails the action it records.

## [0.14.0] - 2026-07-18

### Added

- **`POST /assistant/stream`** — the assistant ask, streamed as SSE. Progress frames as the loop works: `step` (a model turn or tool call starting — the client can show *which* tool is running), `delta` (answer text as the provider generates it — native Anthropic hosts only; OpenAI-compat providers get steps and the answer arrives whole), then exactly one terminal `done` (full answer + proposals + optional trace, authoritative) or `error`. Pre-loop failures (bad request, disabled assistant, missing key) stay plain HTTP errors so clients can tell "can't start" from "died mid-answer". A disconnected client aborts the loop on its next frame — no tokens burn for a listener that left. The buffered `POST /assistant` is unchanged; older clients keep working.

## [0.13.0] - 2026-07-18

### Breaking

- `server_config` gained `collector_smart_interval_ms`, folded into the initial migration (pre-1.0 policy — no migration chaining). Existing databases fail the migration checksum at boot: delete the database folder and re-pair devices after upgrading.
- `[smart] interval_secs` left the TOML — the SMART poll interval is runtime config now (`PATCH /config { collector_smart_interval_ms }`, default 30 min, floor 60 s) and applies without a restart. A leftover `interval_secs` key in an existing TOML is ignored.

### Added

- **Notifications carry the server's name** — alert fired/resolved titles and the channel test notification are prefixed `[server_name]`, so several remon instances reporting into one Telegram chat / ntfy topic are tellable apart. The previously write-only `server_name` finally earns its keep.
- **`GET/PATCH /config/retention`** — read and batch-tune the per-(resource, resolution) keep windows behind the metrics pruner. The batch validates as a whole (nothing half-applies), floor 1 h, cap 10 y; the retention task picks changes up on its next tick.
- **`GET /config/resolutions`, `PATCH /config/resolutions/{name}`** — inspect and enable/disable rollup buckets. Chain guards keep the ladder contiguous: `raw` can't be disabled, a parent feeding an enabled child can't be disabled, a child under a disabled parent can't be enabled.
- **`server_name` in `GET /system/info`** — clients read the canonical name from the response they already cache; remon-web now prefers it over the locally-typed alias (sidebar, server cards) and syncs it on pairing.
- **`updated_at` in `GET/PATCH /config`** — audit timestamp of the last runtime-config write.

## [0.12.0] - 2026-07-14

### Added

- **Incident flight recorder** — the moment an alert first crosses its threshold (ok→pending), the daemon freezes a bounded context bundle: host vitals cross-section, top processes by cpu/memory with their recent in-memory history (spike vs steady state), the daemon's recent errors, system-level error events (journald; OOM kills, segfaults), co-active alerts and failed units — plus a T+60s follow-up sample. Captures are spawned and cooldown-deduped (15 min per rule+label), so rule evaluation latency is untouched and flapping can't spam. The core is trigger-agnostic: `POST /incidents/capture {reason, category}` lets an operator or an external detector (fail2ban action, IDS hook) anchor a snapshot on demand. Snapshots age out with the standard retention engine (30 days).
- **Assistant: incident tools** — `list_incidents` + `incident_detail` answer "what caused that alert at 03:12" from the frozen bundle instead of reconstructing; `capture_incident` freezes the current moment when the assistant notices something anomalous no alert covers (observability-only write — it never touches the host).
- **Assistant: `read_system_events` tool** — OS-level error/warning events: journald on Linux, the System+Application event logs on Windows (PowerShell `Get-WinEvent`). Bounded lines, look-back window, 10s hard timeout.
- **Persistent process series (`metrics_process`)** — once a minute the processes collector folds its per-pid pass into name groups and stores the top-K by cpu and by memory (union; `[assistant] process_series_top_k`, default 20, 0 disables). Bounded by K, never by the host's process table — the same trade prometheus' process-exporter makes. Rolled up raw→1m→5m→1h and aged out like every other metric.
- **`process` metric namespace** — `process.cpu_percent{name="clickhouse-server"} > 90` now works as an alert expression; `metric_history` answers "which process was eating cpu at 3am"; `query_metric` reads the latest per-group values. Quiet processes may have no samples (top-K caveat, documented in the tool description).

## [0.11.0] - 2026-07-13

### Added

- **Assistant: native Anthropic provider** — when `[assistant] base_url` points at `api.anthropic.com`, the assistant speaks the native Messages API instead of OpenAI-compat: same tool loop, plus prompt caching (`cache_control` breakpoints on the system+tools prefix and the growing conversation, so tool-loop round trips re-read context at ~0.1x input price) and per-turn usage logging (`in/out/cache_write/cache_read`) at debug level.
- **Assistant: `list_processes` grew real diagnostic depth** (driven by the assistant's own tool-gap review, gathered via dev mode): per-process `cmdline`, `parent_pid`, `uptime_seconds`, `threads`; `min_cpu_percent` / `min_memory_percent` / `name_contains` filters; docker container short-id from the pid's cgroup (Linux); and — when `[assistant] process_history = true` (default) — a `history` block per process (avg/max cpu, avg memory, disk read/write B/s over up to 15 minutes) so spike-vs-sustained is answerable before proposing a kill/restart. The previously dormant processes collector now runs continuously behind that flag, feeding an in-memory rolling window (and keeping GET /processes warm as a side effect).
- **Assistant: `list_probes` tool + probe metrics** — the assistant can now see custom probes (name, description, schedule, last-run status/message, latest emitted metrics) via `list_probes`, and read probe metric values/trends through `query_metric` / `metric_history` under the `probe` namespace (label `probe_name`). Previously it had no visibility into the probe engine at all.
- **Assistant: longer request timeout** — the `/assistant` endpoint now has its own 150s request timeout (the rest of the REST API keeps 30s). A multi-step tool-use loop — especially at higher effort or with dev `max_steps` raised — legitimately runs past 30s; it was being cut off with a 408. Web client timeout raised to match.
- **Assistant: `read_service_logs` tool** — one-shot `journalctl -u <unit>` tail (Linux), the read-only sibling of the SSE follow stream: bounded lines (10-200), optional `since_minutes`, 10s hard timeout, newest-lines-first size clamp, and the same unit-name charset validation as the SSE handler. The assistant can now diagnose any systemd service from its journal, not just docker containers.
- **Assistant dev mode: per-ask `model` override** — try a different model on the same provider (e.g. claude-haiku-4-5 vs claude-sonnet-5 on one question) without touching config; the trace's model turns name the model that ran.
- **Assistant: conversation memory** — `POST /assistant` accepts a client-replayed `history` of prior question/answer turns (the daemon stays stateless; capped at 12 turns, answers clipped), so follow-ups like "do all of those" resolve against the previous answer.
- **Assistant: dev mode** — with `[assistant] dev = true`, an ask may carry per-request overrides for developing the assistant itself: `system` (prompt iteration without a rebuild), `max_steps`/`max_tokens` (bounded ceilings), `no_tools` (bare-model chat), and `trace` (per-turn model usage/latency + per-tool args/result-preview/latency returned with the answer). Overrides are rejected with 403 while the flag is off; device auth and the read-only/propose-only tool contract apply regardless.
- **Assistant: transient-failure retry** — provider round trips retry on 429/5xx/network errors with bounded backoff (1s → 3s, `Retry-After` honored with a 10s cap) before surfacing an error. Applies to every provider, not just Anthropic.

## [0.10.0] - 2026-07-05

### Breaking

- `heartbeat_checks` / `heartbeat_pings` tables were folded into the initial migration (pre-1.0 policy — no migration chaining). Existing databases fail the migration checksum at boot: delete the database folder and re-pair devices after upgrading.

### Added

- **Heartbeat checks** — push-model dead-man's switches, the inverse of a probe: an external job (cron, backup, anything that can `curl`) proves liveness by pinging an anonymous capability URL; missing the deadline (`period + grace`) flips the check to `down`. No scheduler and no watchdog task — state is derived from timestamps at read/eval time, so the alert rule's own tick is the deadline check and nothing new can leak.
  - `GET|POST /ping/{slug}` (+ `/fail`, `/{exit_code}`) — the slug is the credential: 128-bit random, stored blake3-hashed, shown once at create/rotate, scrubbed from request logs like `access_token`. Fail bodies (≤4 KiB) land on the ping log for 3am debugging. Own per-IP rate-limit bucket sized for NAT'd cron fleets.
  - **Pause windows** ("this silence is expected") — operator pause via `POST /heartbeats/{id}/pause` (indefinite / until / duration, 30d cap), or announced by the service itself via `POST /ping/{slug}/pause?duration=3h` (24h cap; bare form means "quiet until my next ping"). Operator always outranks service. A pause expiring re-anchors the deadline — one fresh `period + grace` instead of an instant page at window end.
  - **`heartbeat` alert namespace** — `heartbeat.up < 1` (one unfiltered rule covers every check, current and future, via label_sets) and `heartbeat.late == 1` for a warn tier that holds through `down`. Paused/disabled checks read `up` so declared maintenance resolves an open alert instead of stranding it. Rules validate before the first check exists.
  - CRUD under `/heartbeats` incl. slug rotation and a 30-day ping log (`/heartbeats/{id}/pings`).

### Fixed

- Alert evaluator: a label_set that vanished from resolver output while `firing` stranded forever — never resolved, never re-notified. Vanished `firing` rows now emit a synthetic resolve ("target removed") and are pruned; `pending` rows are pruned silently. Deleting or renaming an alert target (mount, probe stream, heartbeat check) is an ordinary operation now, not a state leak.
- Alert events sharing the same second are returned in insertion order (`id` tiebreak) instead of arbitrary order.

## [0.9.2] - 2026-06-23

### Breaking

- A `server_secrets` table was folded into the initial migration (pre-1.0 policy — no migration chaining). Existing databases fail the migration checksum at boot: delete the database folder and re-pair devices after upgrading.

### Changed

- **JWT secret is now auto-managed.** A weak or unset `auth.jwt_secret` no longer aborts startup; the server generates a strong per-install secret on first boot and persists it in `server_secrets`. An explicit strong secret in config/env still takes precedence, so sharing or rotating a secret across instances is unchanged.

### Fixed

- Graceful shutdown is no longer triggered spuriously when the Ctrl+C / SIGTERM handler fails to install — the signal future stays pending on error instead of resolving immediately.
- Probe scheduler aborts in-flight tasks for probes removed, disabled, or failed on reload.
- SSE: the `journalctl` follower is killed when the client disconnects.
- Services: enable/disable on a timer targets the `.timer` unit, not the `.service`.
- Auth: refresh tokens are consumed single-use, closing the rotation race.

## [0.9.1] - 2026-06-10

### Added

- **`GET /logs`** — read API over the daemon's own log table (written since 0.7.4 but previously unreadable without opening the SQLite file). Filters: `start`/`end`, `level` (minimum severity), `limit` (default 500, max 5000); newest-first.
- **`GET /ready`** — readiness as opposed to `/health` liveness: performs a DB round-trip and returns 503 with `{"failed_check":"db"}` when the pool can't serve queries. Public, like `/health`. (Named `/ready` rather than k8s-style `/readyz` to stay symmetric with the established `/health`.)

### Removed

- The toy `GET /hello` and `GET /teapot` endpoints; use `GET /health` for connectivity checks. No known client called either.
- The Bruno API collection (`bruno/`). It duplicated the route table by hand and drifted; the authoritative reference is `src/routes/{rest,sse,ws}/mod.rs` plus the DTO modules, which is also what coding agents read.

### Fixed

- Docker WS exec now connects through the same socket-path-aware helper as every REST route, so `docker.socket_path` (Podman, custom sockets) applies to `/docker/containers/{id}/exec` too — previously it silently used the platform default socket.

### Changed

- `GET /health` returns JSON (`{"status":"ok"}`) instead of a plain-text greeting. Status-code-based checks (curl `--fail`, the peer-health example probe) are unaffected.
- DB log writer drains the channel in batches (up to 64 entries per transaction) instead of one commit per line, and stores the emission timestamp captured at the call site rather than the insert time.

## [0.9.0] - 2026-06-10

### Breaking

- `metrics_smart` was folded into the initial migration (pre-1.0 policy — no migration chaining). Existing databases fail the migration checksum at boot: delete the database folder and re-pair devices after upgrading.

### Added

- **SMART disk health** — built-in collector wrapping `smartctl --json` (the same approach Scrutiny/Netdata take; there is no viable pure-Rust SMART stack covering ATA + NVMe + USB bridges). Auto-detects the binary at boot and degrades gracefully when absent. Polls every 30 min by default (`[smart]` config: `enabled`, `smartctl_path`, `interval_secs`), uses `-n standby` so sleeping HDDs are never spun up. Readings (health verdict, temperature, power-on hours, ATA attrs 5/197/198/199, NVMe wear/spare/media-errors) land in the new `metrics_smart` table (raw-only, 1-year retention seed).
- **`GET /system/smart`** — latest reading per device plus an `available` flag distinguishing "no smartmontools" from "no readings yet".
- **`smart` alert namespace** — `smart.health_passed{device="/dev/sda"} < 1`, `smart.temperature_c > 60`, `smart.reallocated_sectors > 0` etc.; device label sourced from `/system/smart` in the alerts schema.

- **`GET /summary`** — one-call host overview aimed at multi-server clients: server name, hostname, OS, version, uptime, latest CPU/memory gauges, fullest-mount disk percentage, and pending/firing alert counts. A fleet view polls this once per daemon instead of fanning out to `/system/info` + `/metrics/*` + `/alerts/state`. Live-gauge fields are `null` until the first collector tick.
- `probes/examples/peer-health.sh` — sibling-daemon reachability probe. A daemon cannot report its own death; in a multi-server setup each daemon watches a peer's `/health` so a dead host still produces an alert (`probe`/`peer-health`/`up < 1`).

## [0.8.3] - 2026-06-05

### Added

- Alerting: `disk.used_percent` and `disk.total_bytes` fields, so a per-mount "% full" rule (e.g. `disk.used_percent{mount_point="/data"} > 90`) is now directly expressible. `used_percent` is computed as `used_bytes/total_bytes*100`.

### Changed

- Probe reload endpoint renamed `POST /admin/probes/reload` → `POST /probes/reload`, aligning it with the other `/probes/*` routes (the `/admin/` prefix implied a privilege tier that does not exist).

## [0.8.2] - 2026-06-02

### Security

- Web push: validate the subscription `endpoint` against the SSRF policy at subscribe and send time, blocking loopback / link-local / RFC1918 targets.
- ntfy: validate the `server` URL against the SSRF policy at create, boot, and send time, matching the webhook channel.
- Probes: clear supplementary groups and set the target gid before `setuid` when dropping privileges as root.

### Fixed

- Notifications: retry FCM / web-push per device instead of per channel, so a slow fan-out no longer re-notifies already-reached devices.
- Alerting: keep a keyed target whose latest sample is NULL by applying the `IS NOT NULL` filter inside the `MAX(timestamp)` subquery.
- Probes: drain stdout/stderr concurrently with `wait()` so a probe that writes past the pipe buffer no longer deadlocks into a timeout.
- Probes: truncate output on a UTF-8 char boundary to avoid a panic on multi-byte tails.

## [0.8.1] - 2026-05-26

### Removed

- Adaptive stats sampling. Collector now ticks at `collector_stats_interval_ms` unconditionally; the prior `base × 4` slowdown on idle caused chart-granularity drift on the overview without meaningful CPU/IO savings. `collector_stats_base_interval_ms` is gone from the `/config` response — use `collector_stats_interval_ms`.

## [0.8.0] - 2026-05-26

### Fixed

- Alerting: hot-reload rules on `PUT` without server restart; remove duplicate fire-body expression; prune stale `Ok` state rows on rule delete.
- Probes: restore `pid` variable used in unix process-group kill path.

### Changed

- Removed dead code, abandoned TOTP path, and unused accessors/methods.
- Clippy clean across all crates; trailing-newline / formatting pass.
- CI: release workflow now grants `contents: write` and attaches built binaries to the GitHub release.

## [0.7.8] - 2026-05-15

### Added

- **`GET /metrics/batch`** — fetch many time-series resources in a single request. `?resources=cpu,memory,disk,network,components,cpu_cores` (whitelist, max 8, no duplicates). Window via `?span=1h|30m|24h|7d|<seconds>` or the existing `?start=&end=` (mutually exclusive). Shared `?resolution=` and `?limit=` apply to every series; `cpu_cores` ignores resolution (no rollup). Per-resource reads run in parallel on the same SQLite pool. Aimed at mobile clients — collapses N dashboard requests into one round trip, one JWT header, one envelope. Pressure / probe metrics stay out of batch (sub-key shape doesn't fit a comma list).
- Response shape: `{ start, end, resolution, series: [{ resource, points }, ...] }`. `series` order mirrors the request. New `BatchSeries` enum is `#[serde(tag = "resource")]` so future resources stay additive.

## [0.7.7] - 2026-05-15

### Security

- **Webhook channel: default-deny private / loopback ranges.** Webhook URLs that resolve to `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16` (incl. cloud metadata `169.254.169.254`), `0.0.0.0`, `255.255.255.255`, multicast/documentation v4 ranges, `::1`, `::`, `fe80::/10`, `fc00::/7`, multicast v6, or any `::ffff:<private-v4>` mapped address are now rejected at create-time (400 BAD_REQUEST) and at send-time (re-resolved each invocation as a DNS-rebinding defense). Closes the SSRF gap noted under v0.7.4 "Loopback / RFC1918 blocking is not enabled by default."
- Two override knobs under `[notifications.webhook]`: `allow_private_targets: bool = false` (master switch for dev / homelab), `allowed_private_hosts: Vec<String> = []` (production allow-list, case-insensitive exact hostname match).
- **Breaking default for upgraders**: an existing webhook channel that targets a private address will be skipped at boot with a single warn line (`Skipping webhook channel '<name>': … — see CONFIG.md`) and will refuse to send. To preserve the prior behavior set `allow_private_targets = true` in config, or migrate the trusted host to `allowed_private_hosts`.

### Changed

- `POST /notifications/channels` and `PUT /notifications/channels/{id}` now validate the webhook URL against the SSRF policy before persisting. Previously bad URLs were inserted silently and only skipped at reload — fixed.

## [0.7.6] - 2026-05-15

### Added

- **Alert silence** — temporarily suppress `Fired` notifications without disabling the rule. `POST /alerts/{id}/silence { "duration_secs": N }` mutes for N seconds; `DELETE /alerts/{id}/silence` lifts immediately. `PATCH /alerts/{id}` also accepts a `silenced_until` field (absolute timestamp; `null` to clear). State transitions and `alert_events` history continue while silenced — the suppressed fires are recorded with `notified: false`. `Resolved` notifications are never silenced. Channel-agnostic: the gate sits before fanout, so FCM and WebPush silence uniformly.
- `silenced_until: Option<i64>` field on the `AlertRuleDto` response (unix epoch seconds; `null` when not silenced).

### Changed

- `alert_rules` schema gained a nullable `silenced_until INTEGER` column (migration `0002_alert_silence.sql`).

## [0.7.4] - 2026-05-11

### Added

- **`[server] trusted_proxy` config** — when set, auth-endpoint rate limit and `devices.last_ip` key on `X-Forwarded-For` instead of the TCP peer

### Added

- **Per-route body size limits** — 8 KiB on the rate-limited anonymous auth subrouter (`/auth/pair/*`, `/auth/login`, `/auth/refresh`), 64 KiB globally
- **30s request timeout** on REST routes (SSE and WS exempted — long-lived streams)
- **`?offset=` on long-tail history endpoints** — `/alerts/events`, `/alerts/{id}/events`, `/probes/{name}/history` now support client-side pagination back through older rows (cap 100 000)
- **`?signal=` on `DELETE /processes/{pid}`** — `9` (SIGKILL) or `15` (SIGTERM, default); rejected otherwise. Ignored on Windows

### Changed

- **Single-facade logging** — `env_logger` removed; the project's `log::*!` and `tracing::*!` call sites now share one `tracing-subscriber` Registry. The `tracing-log` bridge keeps every existing `log::*!` site working unchanged. Stdout and the in-DB `logs` viewer publish the same events through a composed fmt layer + custom `DbLayer`.
- **Per-target log filter defaults** — third-party crates (`sqlx`, `hyper`, `h2`, `rustls`) are clamped to `warn` so enabling `debug` on the app code no longer floods stdout with raw SQL bodies. Operators override the whole filter via `RUST_LOG` env var.
- **ANSI escapes auto-disabled** for non-tty stdout — files, journald, and Loki ingest now receive plain text.
- **Unified timestamp format** — every log line uses RFC 3339 UTC with microsecond precision. Was previously a mix of local-time seconds (app logs) and UTC microseconds (request spans).
- **`logs` table no longer stores third-party noise** — `DbLayer` filters on the `remon_server::*` target prefix, including events bridged from `log::*!` (target is recovered via the `log.target` field). Stdout still receives everything that passes the global filter.
- `tick_timer` percentile flush demoted from `info!` to `debug!`; production logs no longer get a per-collector latency line every minute.
- Removed the duplicate `Probe loader: ...` boot line emitted from `main.rs` — the scheduler already logs it.
- Pairing code no longer logged; terminal display simplified
- FCM error responses sanitized before surfacing; raw body kept at `debug` only
- Stats / processes collector intervals share a single tick timestamp so every metric in one frame writes the same key
- Linux-only metric enrichment extracted into a dedicated `enrich_linux` async fn
- Collector loops now drive off `tokio::time::interval` with `MissedTickBehavior::Skip` instead of `sleep`-after-work; cadence no longer drifts when one tick's work overruns the period. Rollup, retention, and alert evaluator loops use the same pattern
- Notification fanout retries each channel once on transient failure (5 s timeout per attempt, 500 ms backoff). Drops fewer alerts on TCP resets, DNS blips, and upstream 503s
- `WebPushChannel` now reuses the `NotificationManager`'s shared HTTP client (connection pool + 15 s timeout) instead of building its own bare `Client::new()`
- `CompressionLayer` excludes `text/event-stream` — gzip was buffering SSE frames until a compression window filled, breaking live updates
- `Authorization: Bearer …` parsing is now case-insensitive on the scheme (RFC 7235)
- `PATCH /config` rejects `server_name` longer than 128 characters

### Fixed

- Auth-endpoint rate limit collapsed all callers into one bucket when behind a reverse proxy
- `PATCH /config` and the collector floor now reject sub-second sampling intervals — second-resolution timestamps would otherwise collide on the `metrics_*` PK and silently drop earlier rows
- `read_inode_usage` (statvfs per mount) runs inside `spawn_blocking` with a 2s timeout so a hung network/fuse mount can't stall the collector
- Metric inserts switched from `INSERT OR REPLACE` to `INSERT ... ON CONFLICT DO NOTHING` and emit a warn-level log on collision; a same-timestamp row written twice is now visible instead of silently overwritten
- Rollup loop no longer advances the cursor past a failed bucket — on error it stops at the last contiguous-successful bucket and retries the failure on the next tick. Previously a transient DB error on bucket N would leave N permanently un-rolled
- Retention task warns when a `retention_policy` row references an unknown resource instead of silently no-op'ing
- Alert evaluator now persists the state row BEFORE emitting notify/event side effects. A transient DB error on the state upsert previously caused the next tick to re-see Pending state and re-fire, producing a notification storm for as long as the DB hiccup lasted
- Webhook channel rejects URLs that don't start with `http://` or `https://` — cheap SSRF defense against `file://` / `gopher://` etc. Loopback / RFC1918 blocking is not enabled by default

### Removed

- Empty `collectors/docker` stub — container stats remain pull-only via `/docker/containers/*` and `/sse/docker/*` (the `collector_docker_interval_ms` runtime field is kept for a future implementation)

### Build

- `tokio` features narrowed from `"full"` to the explicit set in use
- `chrono` switched to `default-features = false`
- Removed unused deps: `fast_qr`, `base32`, `local-ip-address`, `maplit`, `strum`, `strum_macros`, `hostname`, `env_logger`
- Added `tracing-log` feature to `tracing-subscriber` to bridge `log::*!` macro call sites into the new single subscriber

## [0.7.3] - 2026-05-11

### Added

- **Web Push notifications** — VAPID + RFC 8291 payload encryption, in-house implementation on `ring` (no OpenSSL)
- **Network totals** — `total_rx_bytes` / `total_tx_bytes` per interface
- **`X-Forwarded-For` on `POST /auth/login`** — first entry stored as `devices.last_ip`
- **`parent_pid` on `ProcessInfo`**
- **Example probes** — `ssl-cert-expiry.sh`, `disk-free-root.sh`

### Changed

- `sysinfo` bumped 0.30 → 0.39.1

## [0.5.0] - 2026-05

### Added

- **Platform abstraction** — `ServiceManager` async trait with backends for systemd, OpenRC, Windows SCM, and an unsupported fallback
- **Service endpoints** — `GET/POST /services`, `GET /services/{name}`, start/stop/restart/reload, enable/disable
- **Systemd timers** — `GET /timers`, enable/disable; returns 501 on non-systemd platforms
- **Cron listing** — `GET /cron`; reads system and user crontab files, expands shorthands
- **Service log streaming** — `GET /sse/services/{name}/logs` (Linux only, journalctl-backed)
- **Service watcher** — background poller that sends FCM notifications on unit failure
- **`GET /system/info`** — hostname, OS, kernel, uptime, CPU model, memory, disks, network interfaces
- **Alert engine v2** — expression-based rules with `pending/firing/ok` lifecycle, configurable for-duration and cooldown
- **Alert state & events** — `GET /alerts/state`, `GET /alerts/events`, per-rule event history
- **Notification channels** — CRUD + test for FCM, Telegram, ntfy, webhook (`/notify/channels`)
- **DB indexes** — `alert_state(state)`, `devices(is_active, fcm_token)`
- **Per-collector telemetry** — p50/p95/p99/max timing logged once per minute

### Changed

- Stats collector uses targeted refreshes instead of `refresh_all()`; processes served from a cached snapshot
- DB writes use batch INSERTs for multi-row tables
- Process kill goes through `libc::kill` / `OpenProcess+TerminateProcess` directly instead of sysinfo
- `SystemdManager::get` uses targeted `systemctl show` instead of list-and-find

### Removed

- gRPC (tonic, prost, tonic-prost-build)

### Security

- Service name validated against `[A-Za-z0-9._@:-]` at handler level
- `DELETE /processes/{pid}` rejects `pid == 0`

### Build

- rand 0.10, reqwest 0.13, axum 0.8.9, ring 0.17, windows-sys 0.61

## [0.4.0] - 2026-04-28

### Added

- **Time-series rollup** — configurable resolutions (raw/1m/5m/1h), background worker, resume cursor
- **Retention policies** — per `(resource, resolution)` keep window, hourly cleanup
- **Adaptive sampling** — collector slows down when no SSE/WS subscribers are active
- **Metrics endpoints** — `/metrics/cpu`, `/metrics/memory`, `/metrics/disk`, `/metrics/network` with time range and resolution params
- **Alert rules** — threshold-based CRUD (`GET/POST/PUT/DELETE /alerts`), FCM fan-out, cooldown gate
- **`PATCH /me/fcm-token`** — register or clear FCM push token
- **`POST /auth/logout`** — revoke calling session
- **Runtime config** — `GET/PATCH /config`; changes apply without restart
- **CORS layer** — `allow_any_origin` for dev, explicit allow-list for prod
- **Hosts table** — multi-host ready; local machine seeded at boot
- **Auth rate limiting** — burst 5 / ~1 per 12s on pairing and login endpoints

### Changed

- Migrations moved to versioned `migrations/*.sql` files with `sqlx::migrate!`
- SQLite opened with WAL mode, NORMAL sync, 5s busy timeout, foreign keys on
- Auth middleware checks `jti` against `sessions` table on every request
- Refresh rotation invalidates all device sessions on use
- Pairing code 6 → 8 digits, constant-time comparison, 3-attempt cap

### Removed

- OTP authentication (TOTP QR, `/auth/login/otp`)

### Security

- JTI revocation for access and refresh tokens
- Strict JWT validation (HS256 only, required spec claims)
- Container inspect response whitelisted — env vars, mounts, host config no longer exposed

## [0.3.0] - 2026-01-22

### Added

- WebSocket Docker exec — `WS /ws/docker/containers/{id}/exec`

### Changed

- Routes reorganized into `routes/rest/`, `routes/sse/`, `routes/ws/`
- Endpoints renamed to RESTful resource-based conventions
- SSE endpoints moved to `/sse/` namespace

## [0.2.3] - 2026-01-11

### Added

- Memory cached/buffers fields in `/get-system-info` (Linux: `/proc/meminfo`, Windows: 0)

## [0.2.2] - 2026-01-07

### Added

- Docker log time filtering (`start_time`, `end_time`)
- SSE log streaming — `GET /docker/containers/{id}/logs/stream`
- Container stats endpoint — CPU, memory, network I/O, block I/O
- Podman support via socket path config
- `GET /get-system-info` — uptime, load average, memory, swap, network, process counts
- `GET /get-network-status` — per-interface rx/tx history

### Changed

- Container inspect merged into `GET /docker/containers/{id}`

## [0.2.1] - 2026-01-05

### Added

- Docker container monitoring and log retrieval

## [0.2.0] - 2026-01-04

### Added

- Structured HTTP request logging via `tracing`
- Hierarchical config with `config-rs` (`config/default.toml`, environment overrides)
- JWT secret strength validation at startup
- Bruno API collection

### Changed

- Migrated from Hyper 0.14 to Axum 0.8
- Database connection pooling with `tokio::sync::OnceCell`

### Removed

- `dotenv`, `lazy_static`, `async_once`

### Security

- Production startup fails on weak JWT secret

## [0.1.2]

### Added

- `DELETE /processes/{pid}` — kill process by PID

## [0.1.1]

### Added

- gRPC support with proto file and build.rs
- Versioning and CHANGELOG

## [0.1.0]

### Added

- Initial release
