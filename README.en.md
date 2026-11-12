<div align="right"><a href="README.md">简体中文</a> | <a href="README.en.md">English</a></div>

# kp — keep processes running

`kp` is a transparent command wrapper: like `nohup`, it hands your command to the background, but keeps the full terminal experience — re-attach any time, watch from multiple windows at once, and duplicate commands are deduplicated automatically. Inspired by screen/tmux, but **requires no change to your habits**: type your command as usual, just prefix two letters.

## What problem does it solve

Developers constantly switch between terminals and IDEs (VSCode, Trae, Cursor…). Long-running commands (dev servers, watchers, long builds) are tied to the terminal that started them:

- Close the terminal / restart the IDE → the service dies;
- Type the same command in another window → a second instance starts, port conflict.

With `kp`:

```console
# In VSCode
$ kp make run server 127.0.0.1
─ kp ─────────────────────────────
 session  kp-myproj-3f8a12c0b41d55e6 (started)
 ...

# Same command again in Trae → no new instance; attaches to the existing session (read-only)
$ kp make run server 127.0.0.1
─ kp ─────────────────────────────
 session  kp-myproj-3f8a12c0b41d55e6 (attached · read-only)
 ...
```

Close any window and the session keeps running; Ctrl-C in any window ends the session, all windows exit in sync and echo the exit code — **Ctrl-C passes through even in read-only windows** (no other keyboard input is forwarded). Want to leave the screen without stopping the service? **`Ctrl-b d` detaches** (identical for native and tmux engines; `Ctrl-b Ctrl-b` sends a literal Ctrl-b to the program; any other prefix combo passes both keys through untouched).

## Install

```console
$ cargo install kp-cli      # installs kp (command wrapper) and kpm (session manager)
```

**macOS (Homebrew, recommended)**:

```console
$ brew tap x007007007/kp && brew install kp    # zero external deps; tmux is an optional engine
```

**Direct download**: grab the artifact for your platform from [Releases](https://github.com/x007007007/kp/releases) —

- macOS: `kp-installer-vX.dmg` (double-click, run `install.sh`) or `kp-cli-vX-<target>.tar.gz`;
- Linux: `kp-cli-vX-<target>.tar.gz`; unzip and put `kp`, `kpm` on your PATH.

**Engine**: since v0.2 kp defaults to its **built-in PTY engine** (one lightweight holder process per session, **zero external dependencies**). For advanced terminal reuse such as multi-window co-editing with full-screen TUI programs, switch back to the tmux engine in config:

```toml
[run]
engine = "tmux"   # default native; tmux requires >= 3.0 (brew install tmux)
```

The tmux engine uses a dedicated socket (`-L kp`), fully isolated from your own tmux sessions. screen is not supported as an engine (no child exit code, no read-only attach — native already covers its capabilities).

> macOS notarization note: directly downloaded binaries may be blocked by Gatekeeper on first run; `xattr -d com.apple.quarantine kp kpm` (or install via brew) fixes it.

## Usage

`kp` itself does **one thing: wrap commands**. No subcommands — everything after `kp` is the command to run, so `kp ls`, `kp help`, `kp restart` all wrap same-named system commands. Session management lives in the second command, **`kpm`**:

```console
$ kp <command...>              # start / attach by policy (core usage, any command)
$ kp -n NAME <command...>      # explicit session name (default kp-<project>-<NAME>)
$ kp -p restart <command...>   # restart the old instance on repeat (after code changes)
```

```console
$ kpm ls                    # all sessions: status / policy / PID / age / clients / branch
$ kpm status <name>         # single session details
$ kpm attach <name>         # attach to an existing session
$ kpm logs <name> [-f]      # plain-text log (a second pair of eyes beyond the attach stream)
$ kpm restart <name> [--fresh-env]
$ kpm stop <name> | -a      # kill the whole process group (TERM → 10s → KILL)
$ kpm history               # exit codes and durations of finished sessions
$ kpm clean                 # clean up exited sessions and stale logs
$ kpm config                # print the merged config (for troubleshooting)
$ kpm doctor                # diagnostics: binary versions, daemon identity, sessions, log tail
```

### Troubleshooting (debug)

- **`kpm doctor`**: one-stop checkup — local kp/kpm versions and modification times, daemon version/pid/exe (catches "my change didn't take effect"), session overview, tail of daemon.log.
- **daemon.log** (`~/.local/state/kp/daemon.log`, path follows `$XDG_STATE_HOME`): a timeline of all daemon/holder lifecycle events — create/finalize/kill/cleanup/reconcile, exit-code report success, attach authorization sequence. Please attach `kpm doctor` output when filing an issue.
- **`KP_ATTACH_TRACE=1`**: per-frame tracing of the native attach relay (for debugging keyboard forwarding / exit codes).

### Language / Language

UI text is chosen automatically per POSIX convention: `KP_LANG` > `LC_ALL` > `LC_MESSAGES` > `LANG`; `zh_*` shows Chinese, everything else (unset/C included) shows English:

```console
$ KP_LANG=en kp echo hi     # force English
$ KP_LANG=zh kpm doctor     # force Chinese
```

Built on [rust-i18n](https://github.com/rust-i18n/rust-i18n): translations live in `locales/app.yml` (zh/en side by side, embedded into the binary at compile time), call sites use a single `t!("key")`. **Adding a language**: append a locale section (e.g. `ja:`) for the existing keys in `locales/app.yml` and add the language code in `src/i18n.rs`'s `parse()`; missing keys fall back to English.

`--help` texts and daemon.log are not localized yet (planned).

### Repeat policies (`-p`)

| Policy | Behavior | Typical case |
|---|---|---|
| `attach` (default) | Attach to the existing session | dev server |
| `restart` | Gracefully stop the old instance → start a new one → attach | code changed |
| `wait` | Wait for the existing session to exit first | queued builds / tests |
| `new` | Start a parallel instance (name gets a suffix) | you really need a second one |
| `reject` | Fail with an error (exit 3) | db-migrate and friends that must never run twice |

**Policies are sticky**: set `-p` once for a service and later commands for the same service (without `-p`) reuse it; another `-p` switches it. Priority: CLI `-p` > sticky value > `.kp.toml` > default.

**Restarts don't shake you off**: on `restart` (from any window), other windows see "service restarted; re-attached to the new instance" and automatically follow to the new instance — no manual reconnect.

### Session identity

Without an explicit name, identity = `hash(command argv + project root + env vars − ignore list)`. IDE-injected noise variables (`VSCODE_*`, `TRAE_*`, `TERM_PROGRAM`, …) are ignored out of the box; extend the list in `.kp.toml`.

## Configuration: `.kp.toml`

Put it at the project root (looked up upward, like git); it can be committed for team sharing. Global config lives at `~/.config/kp/config.toml`. Merge chain: built-in defaults < global < project < CLI.

```toml
[identity]
ignore_env = ["MY_NOISE_*"]     # appended to the built-in ignore list

[run]
default_policy = "attach"
term_timeout   = 10             # seconds between TERM and KILL when killing the group
log_max_mb     = 50
history_keep   = 100

[[services]]
name   = "server"
match  = "make run server*"     # glob over argv; first match from the top wins
policy = "attach"

[[services]]
name   = "migrate"
match  = "make db-migrate*"
policy = "reject"               # nobody runs this twice
```

> Security: a `.kp.toml` in a repo is treated as untrusted input — it can only declare matches and policies, it **cannot** run hooks or arbitrary scripts.

## Known limitations

- Compound commands (`&&`, pipes) are parsed by your shell first — kp only receives the first command. Wrap explicitly for compound semantics: `kp sh -c 'a && b'`;
- A child that `setsid`s itself out of the process group escapes whole-group kills (shared with similar tools);
- Single-user by design: sessions are not shared across Unix accounts.

## Development

```console
$ git config core.hooksPath hooks   # enable pre-commit secret scanning
$ make dev-env                      # dev environment: kp = in-development build, state sandboxed in .dev/ (auto-cleaned on exit)
$ cargo test
$ scripts/check-secrets.sh --all    # manual full scan
```

Design docs live in [`docs/`](docs/) (requirements / scope / WBS / config / architecture).

## License

[MIT](LICENSE)
