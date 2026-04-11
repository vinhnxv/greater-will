# launchd Troubleshooting — `com.greater-will.daemon`

Operator runbook for diagnosing the Greater-Will daemon when running as a
macOS launchd LaunchAgent. Registered via `gw daemon install`. The service
label is **`com.greater-will.daemon`** and the plist lives at
`~/Library/LaunchAgents/com.greater-will.daemon.plist`.

## 1. Check loaded

Verify launchd knows about the service:

```bash
launchctl list | grep greater-will
```

Output columns are `PID  STATUS  LABEL`:

- **PID integer, STATUS `0`** — daemon is running, last exit was clean.
- **PID `-`, STATUS `0`** — loaded but not currently running (KeepAlive will
  respawn on demand).
- **PID `-`, STATUS non-zero** — last exit code. Non-zero means the daemon
  crashed; combine with section 3 to find out why.

## 2. Common failures

- **Missing `$HOME` in launchd context** — LaunchAgents inherit a minimal env.
  If the daemon can't resolve `~/.gw`, check with
  `launchctl getenv HOME`. Fix by running `launchctl setenv HOME /Users/<you>`
  or setting `EnvironmentVariables` in the plist.
- **`tmux` / `claude` not in `PATH`** — LaunchAgents start with
  `/usr/bin:/bin:/usr/sbin:/sbin`. If tmux is installed via Homebrew
  (`/opt/homebrew/bin`), add it to the plist `PATH` env or symlink it.
- **Permissions on `~/.gw`** — daemon writes sockets, logs, and state here.
  Ensure it is owned by your user and writable (`ls -ld ~/.gw`).

## 3. Read launchd log

The plist routes stdout/stderr to `$GW_HOME/logs/` (default `~/.gw/logs/`):

```bash
tail -n 200 ~/.gw/logs/daemon.stderr.log
tail -n 200 ~/.gw/logs/daemon.stdout.log
```

For launchd's own view of service start/stop events:

```bash
log show --predicate 'sender == "launchd" AND eventMessage CONTAINS "com.greater-will"' --last 1h
```

## 4. Force restart

Bounce the service in place without uninstalling:

```bash
launchctl kickstart -k gui/$(id -u)/com.greater-will.daemon
```

The `-k` flag sends SIGTERM to any current instance before relaunching.
Alternatively, the native wrapper does the same and cleans up sockets:

```bash
gw daemon restart
```

## 5. Clean uninstall

Remove the LaunchAgent entirely:

```bash
launchctl unload ~/Library/LaunchAgents/com.greater-will.daemon.plist
rm ~/Library/LaunchAgents/com.greater-will.daemon.plist
```

Or use the supported command, which performs both steps:

```bash
gw daemon uninstall
```

## 6. Crash loop recovery

When the daemon crashes 5 times within 30 seconds, the circuit breaker writes
`$GW_HOME/crashloop.flag` and refuses to start. The detector is implemented in
`src/daemon/mod.rs::start_daemon` via `CrashLoopDetector::new(5, 30, 300)`
(5 crashes / 30-second window / 300-second stability reset). These thresholds
are currently hardcoded — env-var configurability is tracked for a follow-up plan.
`gw daemon status` prints a recovery banner showing the actual flag path.

To recover:

```bash
cat ~/.gw/crashloop.flag              # inspect reason + crash count
tail -n 200 ~/.gw/logs/daemon.stderr.log  # find root cause first
rm ~/.gw/crashloop.flag               # clear the sentinel
gw daemon start                       # or: launchctl kickstart -k gui/$(id -u)/com.greater-will.daemon
```

See the "Recovering from a crash-loop halt" subsection in the main
[README](../../README.md#recovering-from-a-crash-loop-halt) for a shorter
quick-reference version.
