# Porting FSearch from macOS to Windows

What the original does with Darwin, and what this does with Win32. The engine,
the index layout, the query language, the ranking, the content index and the
daemon protocol are unchanged; this file is only about the platform layer and
the few places Windows forced a real design decision.

## Syscall by syscall

| macOS | Windows | Notes |
|---|---|---|
| `getattrlistbulk(2)` | `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` | Both fill one buffer with hundreds of entries carrying name, type, size and mtime, so a crawl is one call per directory and no per-file stat. `FILE_ID_BOTH_DIR_INFO.EaSize` carries the reparse tag. |
| — (unsupported filesystems) | `FindFirstFileExW` / `FindNextFileW` | Fallback where the bulk class is unavailable (FAT, some filter drivers). Chosen per directory on the first failed call. |
| `openat()` relative to the parent fd | full path per child, `\\?\`-prefixed | Windows has no `openat`. Paths are rebuilt per child; the `\\?\` prefix removes MAX_PATH and drive-relative surprises. |
| FSEvents, replayable by event id | `ReadDirectoryChangesW`, one recursive watch per volume | Real-time, no admin needed. **No replayable history** — see below. |
| `kFSEventStreamEventFlagMustScanSubDirs` | `ERROR_NOTIFY_ENUM_DIR` (buffer overflow) | Same response: rescan that subtree. |
| `lstat(2)` | `GetFileAttributesExW` | Does not follow reparse points, so it is lstat-shaped. |
| Unix domain socket | named pipe `\\.\pipe\fsearch-s<session>` | Same JSON-lines protocol. Per-session so two users, or one user over RDP, each get their own daemon. |
| `flock(LOCK_EX\|LOCK_NB)` | `File::try_lock()` | std maps this to `LockFileEx` with `LOCKFILE_FAIL_IMMEDIATELY`; the same call works on both platforms, so `libc` is gone. |
| launchd LaunchAgent plist | `HKCU\...\Run` value | No admin needed. `fsearchd.exe` (built `windows_subsystem = "windows"`) is what it launches, so no console window at sign-in. |
| `~/.local/bin/fsearch` | `%LOCALAPPDATA%\Programs\FSearch\` | |
| `~/Library/Application Support/FSearch` | `%LOCALAPPDATA%\FSearch` | |
| `$HOME` | `%USERPROFILE%` | |
| `setiopolicy_np(…DATALESS_FILES, OFF)` | attribute check in `open_regular` | There is no process-wide "don't materialize" switch. Instead files carrying `RECALL_ON_DATA_ACCESS` / `RECALL_ON_OPEN` / `OFFLINE` / `PINNED` / `UNPINNED` are indexed by name and never opened, which is the outcome the macOS policy exists to guarantee. |
| `pthread_set_qos_class_self_np` | `SetThreadPriority` | user-interactive → `HIGHEST`, user-initiated → `ABOVE_NORMAL`, utility → `BELOW_NORMAL`. |
| `malloc_zone_pressure_relief` | `HeapCompact` | |
| global allocator over `mmap`/`munmap` | global allocator over `VirtualAlloc`/`VirtualFree` | Same rule: blocks ≥ 1 MiB go straight to the OS and straight back. |
| TCC / Full Disk Access | token elevation | Windows has no consent prompt, so `SKIP` is an optimisation rather than a way to avoid blocking. `Status.full_disk_access` keeps its name for protocol compatibility and reports elevation. |
| `raise_fd_limit` | — | Windows has no comparable per-process descriptor limit. |

## The one real design change: no replayable history

The original crawls once and then stays current from FSEvents, and because
FSEvents can replay from an event id, a restart applies only what changed
while the daemon was down. `ReadDirectoryChangesW` has no such cursor.

The USN journal does have one, and is the closest analogue — but reading it
needs `FILE_READ_DATA` on a volume handle, which needs elevation. Making the
daemon require admin to restart quickly is a bad trade, so the port uses the
recovery path the original already has for "FSEvents history is gone":

- the index header keeps `synced_at`, the wall-clock second it was last known
  complete;
- on a restart the engine relists every folder whose mtime moved since
  `synced_at - 120 s`, plus the folders of indexed text files edited in place
  (an edit does not touch its folder);
- relisting is one attribute read per folder, in parallel, under a read lock,
  so searches keep answering while it runs.

Measured under Wine: a restart with one changed folder logged
`relisted 1 folders changed since … in 13.48ms`. On a full disk this is O(dirs)
attribute reads — seconds, not a recrawl.

`Index.event_id` therefore stops being a stream cursor and becomes a save
generation: it increments on each compaction, which is all a follower needs to
notice the owner wrote a newer index. The compaction trigger loses its
`stale &&` term for the same reason and fires on the 12-hour timer outright.

## Paths

Inside fsearch a path is a byte string joined with `\`, exactly as a user
writes it: `C:\Users\me\main.rs`. Two consequences:

- **A drive root is its drive token.** `C:` is the index path of `C:\`; only
  the platform layer appends the separator when it builds a Win32 name,
  because `C:` on its own would mean "the current directory on C".
- **Everything hangs off one virtual root** (entry 0) whose children are the
  volumes. That is what keeps `in:` a single contiguous range across several
  disks, and keeps `descendants()`, the dir memo fold and the whole DFS block
  layout working unchanged. Volumes are given depth 0 so `C:\Users` is depth
  1, where the ranking table expects it.

Names come off the wire as UTF-16 and are stored as UTF-8. Every valid Windows
name has a UTF-8 encoding, so this round-trips; unpaired surrogates (legal in
a name, not in UTF-8) become U+FFFD.

## Reparse points

Not following them is what stops `WinSxS` and `Users\All Users` being indexed
several times, and stops a link to an ancestor looping. But OneDrive folders
are reparse points too, and skipping those would lose a user's Documents. So
the rule is:

- a reparse-point **directory** is not descended into and carries `FLAG_MOUNT`,
  **unless** it is a cloud-files placeholder — reparse tag in the `0x9…` range,
  or any of the `RECALL_*` / `OFFLINE` / `PINNED` / `UNPINNED` attributes — in
  which case it is a real folder and is descended into. Listing a placeholder
  does not download anything; only reading file contents would.
- a reparse-point **file** is `KIND_LINK`.
- a hard depth cap (`MAX_DEPTH = 96`) bounds the cloud-placeholder case.

## Platform tables

The algorithms are the original's; the data in them is Windows'.

- `prior_adjust`: `Users` 0, `Program Files` +10, `Windows` −40,
  `WinSxS`/`WindowsApps` −40, `AppData`/`ProgramData`/`Packages` −15,
  `node_modules`/`venv`/`site-packages` −30, `target`/`build`/`dist`/`obj`/
  `bin`/`x64`/`Debug`/`Release` −12, dot-dirs −25, `$`-dirs −30, and the
  same `+15` for entering the home folder.
- `content::SKIP_DIRS` / `SKIP_UNDER_HOME` / `SKIP_SUFFIXES` / `TEXT_EXTS`:
  the original lists plus Windows equivalents (`Windows`, `WinSxS`,
  `WindowsApps`, `AppData`, `$Recycle.Bin`, `System Volume Information`, `.vs`,
  `obj`, `scoop`, `.nuget`; `ps1` `psm1` `reg` `inf` `sln` `csproj` `xaml`
  `resx` `ahk` `vb` `bas` … as text).
- `type:app` means Windows programs (`.exe` `.msi` `.msix` `.appx` `.lnk` …),
  with `type:program` as an alias. On macOS it meant `.app` bundles; the
  intent is the same, the extension list cannot be.
- The `NF_APP` ranking bonus applies to app *packages* (`.msix`/`.appx`
  families), which are directories — the same shape as `.app` bundles. `.exe`
  files are deliberately not boosted; there are too many.
- `char_bit` sends `\` to the "anything else" bit. It is the separator now, so
  it can never appear in a name; `:` (drive tokens) shares that bit.

## Deliberate deviations

- **Two binaries.** `fsearch.exe` (console) and `fsearchd.exe` (no console).
  The original needs one because launchd runs it in the background; a Windows
  sign-in entry runs a console app *with a window*, so the daemon gets its own
  build of the same `cli::run()`.
- **`connect()` verifies with a `ping`.** A pipe connection is only handed
  back once the daemon has answered one, because during daemon startup a
  client can connect to an instance that is about to be closed under it. The
  wait is bounded (`wait_readable`, 3 s), so it cannot hang.
- **Index files are swapped, not replaced.** Windows refuses to replace a file
  another process has mapped (`ERROR_USER_MAPPED_FILE`), but it will rename
  one that was opened with `FILE_SHARE_DELETE` — which is how Rust opens
  files — and the mapping follows the old name. So `os::replace` renames the
  old file aside, renames the new one in, and deletes the old. This matters
  because a follower has `index.bin` mapped while the owner compacts.
- **`FSEARCH_ROOT`** indexes one folder as the `C:` volume. It exists so the
  test suite (and the Wine runs used to validate this port) can exercise the
  real engine against a small tree. Unset, the build indexes every fixed and
  removable volume.
- **The crate is `os`-agnostic above `src/os/`.** `index`, `query`, `live`,
  `content`, `engine`, `server`, `walk` and `cli` contain no `cfg(windows)`.
  `src/os/windows.rs` is the Win32 half; `src/os/host.rs` is a host backend
  that dresses a temp folder up as `C:` so `cargo test` covers everything but
  the syscalls. Neither `fsevents.rs` nor `libc` exists any more.

## Diagnostics

A port whose platform layer nobody has run on the target OS needs to explain
itself when it fails, so:

- `src/diag.rs` installs a panic hook and a `SetUnhandledExceptionFilter`
  handler, and traces each startup stage to `%LOCALAPPDATA%\FSearch\trace.log`.
  The fault handler formats into a stack buffer and writes with raw
  `CreateFileW`/`WriteFile`, because the thing it reports on may be a corrupted
  heap. `SetErrorMode(SEM_NOGPFAULTERRORBOX)` keeps Windows' own dialog from
  being the only evidence.
- `fsearch doctor` runs every subsystem in turn under `catch_unwind`, so one
  failure does not hide the next, and reports the OS error for each.
- `Stream::failures()` carries the reason a volume could not be watched, rather
  than leaving a silently dead change stream.
- The binaries link the CRT statically (`.cargo/config.toml` sets
  `+crt-static` for the MSVC target). Without it the exe imports
  `VCRUNTIME140.dll` and will not start on a Windows without the VC++
  Redistributable — which presents as an instant crash with no message.
- `fsearch.exe` waits for a keypress before exiting when it is the only process
  on its console (`GetConsoleProcessList` <= 1), i.e. when it was double-clicked
  and the window would otherwise vanish.

## Validated

- `cargo test`: 11 tests over index build, path rendering and lookup, fuzzy
  and typo scoring, every filter, ranking, live create/delete/rename/subtree
  diffs, compaction round-trip, save/load, the trigram content index
  (`grep:`/`regex:`/`sym:`), the `Engine` lifecycle, and the daemon's JSON
  protocol.
- `cargo build --release --target x86_64-pc-windows-gnu`: clean, no warnings;
  the exe imports only `kernel32`, `advapi32`, `msvcrt`, `ntdll`,
  `bcryptprimitives` and one api-set — no MinGW runtime DLLs to ship.
- Run under Wine against a real tree: crawl, name search, typo tolerance,
  `'exact`/`^prefix`/`suffix$`/`!exclude`, `ext:`/`type:`/`kind:`/`in:`/
  `size:`/`mtime:`/`re:`/`path:`/`limit:`, `grep:`/`regex:`/`sym:`, `--json`,
  `stdio`, `status`, `bench`, live create and delete (~1 s), folder rename,
  content indexing of a new file (~3 s), restart catch-up, client-side
  auto-spawn of the daemon, and `install` / `install --login` / `uninstall`
  against the real registry.
