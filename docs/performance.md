# Performance: what was measured, fixed, and ruled out

This records a CPU audit of the container and host daemons carried out on
**2026-09-20**, so the conclusions are not re-derived from scratch later. The
**Ruled out** section is the important half: several plausible-looking
optimisations were measured and found to be worth nothing.

All figures come from one live devcontainer — 8 cores, 31 processes, 662 open
file descriptors, 4 forwarded ports — unless stated otherwise. Re-measure before
trusting any number here on different hardware or a busier container.

## The finding

The container daemon burned **4.2% of one core, continuously, forever**, doing
work whose result was discarded.

`scan_listening_ports` ran once a second (`DEFAULT_SCAN_INTERVAL_MS = 1000`) and
resolved a process name for **every** listening port on **every** scan. Each
resolution restarted a full walk of `/proc/{pid}/fd`, so a scan cost
O(ports × pids × fds), not the O(pids × fds) its doc comment claimed.

Two things made it pure waste:

- The caller sends a process name only when it **first** forwards a port
  (`container/mod.rs`, guarded by `if !forwarded.contains_key(port)`). Every
  resolution for an already-forwarded port was computed and thrown away. In
  steady state that is 100% of the work.
- Two of the four ports were root-owned sockets while the daemon runs as uid
  1000. `/proc/{pid}/fd` of another user's process is unreadable, so those two
  walked every pid and every descriptor, found nothing, and returned `None` —
  every second, permanently.

### Measurement

`strace -f -c` on the daemon for ~15s:

| syscall | calls | rate |
| --- | --- | --- |
| `futex` | 141,730 | ~9,450/s |
| `readlinkat` | 33,691 | ~2,250/s |
| `sched_yield` | 9,628 | ~640/s |
| `getdents64` | 3,333 | ~220/s |

CPU sampled from `/proc/<pid>/stat` fields 14/15 over 20s: 84 ticks at 100 Hz =
0.84s, i.e. **4.2% of one core**, 75% of it system time.

The `futex`-to-`readlinkat` ratio of **4.2×** is the key number. The scanner used
`tokio::fs`, where every `read_dir`/`read_link` is a `spawn_blocking` round trip
— a queue push, a futex wake of a blocking thread, and a waker unpark. The
daemon was running **64 threads** (8 tokio workers plus ~55 blocking-pool). The
syscalls themselves were never the cost; the cross-thread handoffs were.

This matters for the shape of the fix: merely restructuring the walk while
staying on `tokio::fs` would have kept most of the cost.

### The fix

`scan_listening_ports` now takes the set of ports whose names the caller already
has, and skips two classes of work before walking anything:

- ports already forwarded — the caller has the name and will not use a new one;
- sockets owned by another uid when not running as root — the walk can only fail.

Whatever survives is resolved in **one** `/proc` pass using `std::fs` inside a
single `spawn_blocking`, with an early exit once every wanted inode is found.

Steady state is now **zero walks and zero handoffs**; a newly detected port costs
one walk with one handoff. Measured floor for a scan that resolves nothing is
~290µs (two `/proc/net/tcp*` reads), about **0.03% of a core** — roughly a
**140x** reduction.

Covered by `resolution_is_skipped_for_known_ports_and_foreign_uids` in
`src/container/scanner.rs`, which asserts all four cases: a new same-uid port
resolves, an already-forwarded port does not, a foreign-uid socket is skipped
when unprivileged, and the uid pre-check does not fire when running as root.

## Also fixed

These are correctness or hygiene, **not** measured performance wins. Do not
expect CPU from them.

- **Supervisor respawned permanent failures.** The devcontainer feature's
  entrypoint restarted the daemon on any non-zero exit, but a rejected auth
  token deliberately exits because "retrying will not help". The result was an
  endless 5s respawn loop that also spammed the host with rejected
  registrations. The daemon now exits with `EXIT_PERMANENT_FAILURE` (2) and the
  supervisor treats it as final. Covered in `tests/entrypoint.sh`.
- **Browser mutex held across the browser process.** `OpenUrl` handling held the
  browser lock across the subprocess wait, and forward, unforward and container
  cleanup all take that same lock — one slow `xdg-open` blocked port forwarding
  for every container. Validation, rate limiting and URL rewriting now happen in
  a synchronous `prepare()` under the lock; `launch()` runs without it, bounded
  by a 10s timeout. `prepare()` being non-`async` is what makes the lock
  impossible to hold across the await — that is enforced by the compiler rather
  than by a test. **The 10s timeout itself has no automated test**: exercising
  it needs a command that hangs, and doing that under a paused clock leaks the
  process.
- **`MissedTickBehavior::Delay`** on the port scanner, the heartbeat and the
  socket scanner. Tokio defaults to `Burst`, which fires catch-up ticks back to
  back after a slow cycle. Measured as **not currently occurring** (see below);
  this is insurance, not a fix.
- **One write per control message, plus `TCP_NODELAY`.** `write_message` sent
  the JSON and its newline as two writes. See the Nagle entry below — the
  expected benefit did not materialise. Kept because the data path already
  frames this way and consistency is worth something.
- **Socket scanner moved to `spawn_blocking`.** It ran blocking `readdir`/`lstat`
  on a runtime worker thread. Correct to fix; costs essentially nothing today.

## Ruled out — do not re-attempt without new evidence

Each of these looked plausible and was measured or read to a conclusion.

| Idea | Verdict | Evidence |
| --- | --- | --- |
| Nagle/delayed-ACK stalls control messages, adding ~40ms to every proxied connection setup | **Refuted** | First-byte latency through a forwarded port over 25 connections: min 8.5ms, median 20.5ms, max 96ms, only 2/25 above 35ms and **no mode near 40ms**. The 20.5ms median is the honest cost of a round trip across the Docker VM boundary. |
| `MissedTickBehavior::Burst` causes catch-up tick storms | **Not occurring** | At ~2,250 `readlinkat`/s and ~662 per walk, the scanner completed ~1.1 scans/s *while slowed by strace*. It never overran its 1s interval. |
| Host socket scanner's glob rescanning is expensive | **Non-issue** | 48µs per scan for `/tmp/claude-mcp-browser-bridge-*/*.sock` at one scan per 5s = **0.00095% of a core**. Only worth revisiting for a `**` pattern over a large tree. |
| Cache inode→process positively and negatively across scans | **Unnecessary** | The caller's `forwarded` map already is that cache. Adding another reintroduces inode-reuse staleness for zero gain. |
| Mis-attributed process names could misroute ports via `--exclude-process` | **False premise** | That flag does not exist. `container/mod.rs` passes `None` for it, and `filter.rs` only acts on `Some`. `process_name` is display-only — it reaches `dbr status` and nothing else. `docs/architecture.md` and `CLAUDE.md` both described it as a live flag, which is what made the premise look sound; both corrected. |
| Replace `/proc` polling with netlink `sock_diag` or inotify | **Not worth it** | The post-fix floor is ~290µs/s. A new dependency and a second code path buy nothing. |
| Raise the default scan interval above 1s | **Wrong lever** | It hides the bug rather than fixing it, and costs responsiveness precisely when it matters — an OAuth flow binds a random port and expects it forwarded immediately. After the fix, 1s costs ~0.03%. |
| Micro-optimisations: per-line `Vec` in `parse_proc_net_tcp`, O(containers) `find_container_for_port`, `ctrl_c()` future rebuilt per loop iteration, IPv4→IPv6 fallback memoisation | **Below the noise** | All per-event or sub-microsecond. |

## Examined and found clean

Recorded so this ground is not re-searched:

- Host daemon is effectively idle: one 30s heartbeat per container, nothing else
  periodic unless socket forwarding is enabled.
- `HostState` is never held across an await in request paths (shutdown
  excepted), and there is no lock-order inversion.
- `proxy.rs`: pending map bounded at 1024 with pruning; 10s bridge timeout; the
  10ms grace poll is bounded and off the hot path.
- `listener.rs`: biased select, no busy loop, backpressure via a bounded mpsc.
- `control.rs` read path is bounded and `fill_buf`-driven, one allocation per
  message.
- Clipboard capture is on demand only, with bounded reads, a single-permit
  semaphore, and `kill_on_drop`.
- No unbounded collections anywhere in the crate.

## Open, not investigated

- Whether the ~55 blocking-pool threads shrink after the scanner fix. Check with
  `ls /proc/<pid>/task | wc -l` on a container daemon that has been up a while.

## How to re-measure

```sh
# CPU of the container daemon (fields 14/15 are utime/stime in CLK_TCK ticks)
pid=$(pgrep -f "dbr container-daemon")
awk '{print $14, $15}' /proc/$pid/stat; sleep 20; awk '{print $14, $15}' /proc/$pid/stat

# Syscall profile — a high futex:readlinkat ratio means thread-handoff overhead
strace -f -c -p "$pid"

# Socket ownership, to see which ports can never resolve (field 8 is uid)
awk 'NR>1 && $4=="0A" {print $2, $8, $10}' /proc/net/tcp
```

Written by Claude (claude-opus-5).
