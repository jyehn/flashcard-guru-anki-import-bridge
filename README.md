# flashcard-guru-anki-import-bridge

A static-library FFI shim that exposes a narrow C ABI on top of
[ankitects/anki](https://github.com/ankitects/anki)'s `rslib` crate, so the
[Flashcard Guru iOS app](https://flashcard-guru.flashify.app) can parse
`.apkg` / `.colpkg` packages, render note templates, and convert SM-2
scheduling state into FSRS — all using the canonical Anki implementation
rather than a hand-rolled parser.

> **License:** AGPL-3.0-or-later (because rslib is AGPL).
> See [`LICENSE`](LICENSE) for the full text.
>
> The Flashcard Guru iOS app links the compiled xcframework as a binary
> dependency. The AGPL obligations apply to the compiled artifact and
> this source; the rest of the iOS app (which only sees a narrow Swift
> interface above this crate) is not affected.
>
> If you're forking this for a closed-source app and want to avoid AGPL,
> replace this whole crate with a pure-Swift `.apkg` parser — the Swift
> architecture above the bridge is parser-agnostic.

## Repository layout

This crate was extracted from the
[`flashifyIOS`](https://flashcard-guru.flashify.app) repository on
2026-05-09 to satisfy AGPL-3.0's source-availability obligation cleanly.
It is consumed back into `flashifyIOS` as a git submodule at
`Vendor/RustImportCore/`. The Flashcard Guru iOS app's
[`Core/Services/Import/`](https://github.com/jyehn/flashcard-guru-anki-import-bridge)
bridge calls into the resulting `xcframework`.

For standalone development (without flashifyIOS), see
[Standalone build](#standalone-build) below.

## What it is

A single Cargo crate (`rust_import_core`) that:

1. depends on `anki = path = "../anki-rs/rslib"` (real rslib)
2. defines `<20` `pub extern "C"` functions wrapping the bits of rslib we
   actually need
3. compiles to two static libraries (`aarch64-apple-ios` device and
   `aarch64-apple-ios-sim` simulator)
4. wraps them in an `XCFramework` that the Xcode project links

The Swift side (`FlashReader/Core/Services/Import/RustImportBridge.swift`)
is the only file in the app that imports the resulting `RustImportCore`
module. Everything else uses the bridge's pure-Swift `DeckSummary` /
`NoteSummary` / `CardSchedulerState` / `RenderedTemplate` types.

## Layout

```
Vendor/
├── anki-rs/                    # git submodule, pinned (see "Updating rslib")
├── RustImportCore/             # this crate
│   ├── Cargo.toml
│   ├── Cargo.lock
│   ├── cbindgen.toml           # C header generator config
│   ├── build.rs                # runs cbindgen on every cargo build
│   ├── src/
│   │   └── lib.rs              # all FFI definitions
│   ├── target/                 # gitignored
│   └── README.md               # this file
└── RustImportCore.xcframework/ # gitignored, produced by scripts/build-rust.sh
```

The `target/` and `.xcframework/` are deliberately not checked in: the
xcframework is ~43 MB and rebuilding from source via `scripts/build-rust.sh`
is fast on a warm cargo cache (~2s).

## Prerequisites

| Tool | Required version | How |
|---|---|---|
| `rustup`/`cargo` | any recent | https://rustup.rs/ |
| Rust toolchain | **1.92.0** | auto-installed by rustup when you `cd Vendor/anki-rs` (driven by `rust-toolchain.toml` inside the anki repo) |
| iOS targets | `aarch64-apple-ios`, `aarch64-apple-ios-sim` | `rustup target add aarch64-apple-ios aarch64-apple-ios-sim` |
| `protoc` | any modern | `brew install protobuf` (rslib's own bundled protoc lives in `anki/out/extracted/protoc/` and we don't run anki's bootstrap) |
| Xcode | 16+ | needed for `xcodebuild -create-xcframework` and the iOS SDK |

The build script (`scripts/build-rust.sh`) auto-exports `PROTOC` from
`/opt/homebrew/bin/protoc` if the env var isn't already set.

### One-time submodule init

After cloning the repo (or pulling a commit that bumps the rslib pin):

```
git submodule update --init --recursive Vendor/anki-rs
```

The `--recursive` matters — anki itself uses two nested submodules
(`ftl/core-repo` and `ftl/qt-repo`) for translation files, and rslib's
`anki_i18n` build script reads from those directories. Without them you
get a confusing `Os { code: 2, kind: NotFound }` panic deep in
`gather.rs:62`.

## Building

```
scripts/build-rust.sh release        # produces Vendor/RustImportCore.xcframework
scripts/build-rust.sh debug          # debug profile, ~2x faster cold build
```

The script:

1. ensures the iOS targets are installed via `rustup target add`
2. exports `PROTOC` if needed
3. runs `cargo build --release --target aarch64-apple-ios` and
   `--target aarch64-apple-ios-sim` (sequentially; rslib has a single
   `target/` per build, parallel target builds wedge the lockfile)
4. locates the cbindgen-generated `rust_import_core.h` (cargo doesn't
   surface a stable path, so we pick the newest `out/include/` by mtime)
5. stages each platform's `.a` + `Headers/rust_import_core.h` +
   `Headers/module.modulemap` under `Vendor/.rust-stage/`
6. invokes `xcodebuild -create-xcframework` to produce the final
   `Vendor/RustImportCore.xcframework`
7. cleans `Vendor/.rust-stage/`

Cold build times on M-series Mac: ~1m37s release, ~45s debug.
Subsequent builds with no source changes finish in <2s.

## Xcode integration (one-time)

The xcframework needs to be linked into the FlashReader Xcode target
once. The reference is already present in `FlashReader.xcodeproj/
project.pbxproj`, so a fresh clone shouldn't need the linkage step —
just run the build script and re-open Xcode. If the reference is
missing (e.g. the pbxproj was reverted):

1. Run `scripts/build-rust.sh release` so the xcframework exists on disk.
2. In Xcode, drag `Vendor/RustImportCore.xcframework` into the project
   navigator. In the dialog:
   - **Action:** *Reference files in place* (do **not** copy)
   - **Add to targets:** check `FlashReader`
3. Project → FlashReader target → **General** → **Frameworks,
   Libraries, and Embedded Content**: confirm
   `RustImportCore.xcframework` is listed and **Embed:** `Do Not
   Embed` (it's a static library).
4. ⌘B to build. The Swift bridge file at
   `FlashReader/Core/Services/Import/RustImportBridge.swift` will
   resolve `import RustImportCore`.

### Auto-rebuild on every Xcode build (recommended)

To avoid having to remember `scripts/build-rust.sh` after editing Rust
sources, set up a **separate aggregate target** that runs the script,
then make `FlashReader` depend on it. Why aggregate-target rather than
a build phase on `FlashReader` itself: putting the script inside
`FlashReader` creates a dependency cycle — `ProcessXCFramework`
consumes `Vendor/RustImportCore.xcframework` as input to the same
target whose script phase produces it, and Xcode refuses with
`error: Cycle inside FlashReader; building could produce unreliable
results`. Splitting into two targets breaks the cycle by giving Xcode
an explicit "build this first" edge.

Once-off setup:

1. **Create the aggregate target.**
   File → **New** → **Target…** → search "Aggregate" (under
   Cross-platform / Other) → **Next**.
   Product Name: `RustImportCore`. Project: `FlashReader`. Click
   **Finish**.

2. **Add the run-script phase to it.**
   Project navigator → `FlashReader` project → select the new
   `RustImportCore` aggregate target → **Build Phases** → **+** →
   **New Run Script Phase**. Name it `Build RustImportCore`.

3. **Script body:**
   ```sh
   if [ "$ACTION" = "indexbuild" ]; then exit 0; fi
   "${SRCROOT}/scripts/build-rust.sh" "${CONFIGURATION:-Debug}"
   ```
   The first line skips the script during Xcode's background
   indexing builds, which would otherwise spawn cargo for every
   index update.

4. **Input Files** (one per line):
   ```
   $(SRCROOT)/Vendor/RustImportCore/Cargo.toml
   $(SRCROOT)/Vendor/RustImportCore/Cargo.lock
   $(SRCROOT)/Vendor/RustImportCore/build.rs
   $(SRCROOT)/Vendor/RustImportCore/cbindgen.toml
   $(SRCROOT)/Vendor/RustImportCore/src/lib.rs
   ```

5. **Output Files:**
   ```
   $(SRCROOT)/Vendor/RustImportCore.xcframework/Info.plist
   ```
   Output Files are safe here — they live on a different target than
   the one consuming the xcframework, so no cycle.

6. **Make FlashReader depend on it.**
   Select the `FlashReader` target → **Build Phases** → **Target
   Dependencies** → **+** → pick `RustImportCore` → **Add**.

7. **Update the scheme.**
   Product → **Scheme** → **Edit Scheme…** → **Build** →
   confirm both `RustImportCore` and `FlashReader` are listed; if
   only `FlashReader` is there, click **+** and add
   `RustImportCore` above it. (Xcode usually adds it automatically
   when you wire the dependency, but sometimes doesn't.)

Behaviour after this is wired:

- **No Rust changes** → Xcode skips the aggregate target's script
  phase entirely (Input Files unchanged vs. Output Files). Zero
  overhead.
- **Touched but unchanged Rust source** → script's internal
  fast-skip exits in ~12 ms (compares Info.plist mtime against
  newest source mtime).
- **Real Rust change** → cargo recompiles. Debug profile rebuild
  is ~10–30 s warm; release is slower because of LTO (~2–3 min)
  and only matters for archives.

If you don't want this, just remove the aggregate target — manual
`scripts/build-rust.sh` from the terminal still works exactly as
before.

### Common mistake: putting the script on `FlashReader` directly

If you (or Xcode AI assistance) put the Run Script phase on the
`FlashReader` target with the same Output Files, every build fails
with:

```
error: Cycle inside FlashReader; building could produce unreliable results.
Cycle details:
→ Command: ProcessXCFramework /…/Vendor/RustImportCore.xcframework …
○ That command depends on command in Target 'FlashReader': script phase "Build RustImportCore"
```

Move the phase to the aggregate target as described above.

## FFI surface

### ABI v5: imported history and queue

`flashify_card_import_history_json(pkg, card_id)` returns one owned UTF-8 JSON
snapshot containing card/note/ordinal identity, type/queue/due/interval/reps/lapses,
collection creation and import timestamps, and all nine revlog columns per row.
Release it with `flashify_free_import_history_json`, never libc `free`.
NULL means a read error; `reviews: []` means a successful read with no history.
The collection's existing mutex/SQLite connection is used: rslib holds an
exclusive lock, so external SQLite readers cannot safely inspect its database.
The payload is one card at a time, with no duplicated note fields or templates.
Clients must treat these rows as imported history, not new rating mutations to
replay over an already-imported scheduling baseline.

All functions live in [`src/lib.rs`](src/lib.rs). The cbindgen-generated
header is a build artifact; refer to the Rust source for the
authoritative definitions.

### Versioning

| Function | Returns | Use |
|---|---|---|
| `flashify_abi_version()` | `u32` | Currently `5`. Swift bridge checks this at `openPackage` and rejects a mismatch. **Bump when the FFI contract changes.** |
| `flashify_anki_version()` | `*const c_char` | rslib's own version string (e.g. `"25.10b1"`). Diagnostic only. |
| `flashify_self_test()` | `c_int` | Returns 0. Smoke probe. |

### Lifecycle

| Function | Returns | Use |
|---|---|---|
| `flashify_open_package(package_path, workspace_path)` | `*mut OpenedPackage` | Opens an `.apkg` / `.colpkg`, runs `Collection::import_apkg` into a fresh collection inside `workspace_path/rust-import-<pid>-<ns>/`. NULL on failure. |
| `flashify_free_package(pkg)` | `void` | Frees the package and recursively deletes its work dir. |
| `flashify_media_dir(pkg)` | `*const c_char` | Absolute path to the media folder rslib populated. Pointer is owned by the package. |
| `flashify_last_error_message()` | `*const c_char` | Last error string set anywhere in this crate. NULL if never set. |

### Decks

| Function | Returns | Use |
|---|---|---|
| `flashify_enumerate_decks(pkg)` | `*mut DeckList` | Snapshot of every deck (default deck filtered out). |
| `flashify_free_decks(list)` | `void` | |
| `flashify_decks_count(list)` | `i64` | -1 on null. |
| `flashify_decks_id_at(list, idx)` | `u64` | 0 on out-of-range. |
| `flashify_decks_name_at(list, idx)` | `*const c_char` | Hierarchical name (`Parent::Child`). NULL on out-of-range. |

### Notes

| Function | Returns | Use |
|---|---|---|
| `flashify_enumerate_notes(pkg, deck_id)` | `*mut NoteList` | All notes in the deck. `deck_id = 0` for every note in the package. |
| `flashify_free_notes(list)` | `void` | |
| `flashify_notes_count(list)` | `i64` | |
| `flashify_notes_id_at(list, idx)` | `u64` | |
| `flashify_notes_notetype_id_at(list, idx)` | `u64` | |
| `flashify_notes_field_count_at(list, idx)` | `i64` | |
| `flashify_notes_field_at(list, idx, field_idx)` | `*const c_char` | |
| `flashify_notes_tag_count_at(list, idx)` | `i64` | |
| `flashify_notes_tag_at(list, idx, tag_idx)` | `*const c_char` | |

### Cards

| Function | Returns | Use |
|---|---|---|
| `flashify_enumerate_cards_for_note(pkg, note_id)` | `*mut CardOrdList` | Every card the notetype generated, sorted by ord. Covers Basic+Reverse and Cloze. |
| `flashify_free_card_ords(list)` | `void` | |
| `flashify_card_ords_count(list)` | `i64` | |
| `flashify_card_ords_id_at(list, idx)` | `u64` | |
| `flashify_card_ords_ord_at(list, idx)` | `u32` | |
| `flashify_card_id_for(pkg, note_id, ord)` | `u64` | Single-card lookup; returns 0 if no card at that ordinal. |
| `flashify_card_state_as_fsrs(pkg, card_id)` | `FsrsState` (by value) | Three priority levels: FSRS-native → SM-2-converted → empty (`-1, -1`). |
| `flashify_render_template(pkg, note_id, ord)` | `*mut RenderedCard` | Calls `Collection::render_existing_card` with `partial_render=false`. |
| `flashify_free_rendered(card)` | `void` | |
| `flashify_rendered_question(card)` | `*const c_char` | |
| `flashify_rendered_answer(card)` | `*const c_char` | |

## Memory ownership rules

These rules are **frozen** as part of the ABI contract. Breaking any
of them is undefined behaviour from Swift's side.

1. **Heap-allocated handles** (`*mut OpenedPackage`, `*mut DeckList`,
   `*mut NoteList`, `*mut CardOrdList`, `*mut RenderedCard`) are owned
   by Rust. The caller MUST release each one with the matching
   `flashify_free_*` function. Never `free()` from Swift.

2. **Borrowed strings** (`*const c_char` from any `*_at` accessor or
   from `flashify_rendered_question` / `flashify_rendered_answer` /
   `flashify_anki_version` / `flashify_last_error_message` /
   `flashify_media_dir`) are valid only until the parent handle is
   freed. Swift must `String(cString:)` (which copies) before the
   parent goes out of scope.

3. **`FsrsState`** is returned by value via the C calling convention.
   No allocation; a stack copy is fine.

4. **Errors** are signalled by NULL returns or non-zero status codes.
   Call `flashify_last_error_message()` immediately afterward to
   retrieve the message; subsequent FFI calls may overwrite it.

## Common gotchas

These bit us during development. Reading them now saves you the same
debugging session.

### A TLS feature is required

rslib needs one TLS backend to type-check even though we never touch its
sync layer. Without one, `cargo check -p anki` fails on type inference
inside `async-compression` because `reqwest` defaults to a TLS-less
configuration and the trait bounds for `tokio::bufread` don't resolve.
The crate's default feature `native-tls` keeps the iOS build exactly as
before; the Android build selects `rustls` instead (see "Android").

### Rust 1.92.0 pinned

`Vendor/anki-rs/rust-toolchain.toml` pins to `1.92.0`. Newer versions
fail clippy lints inside rslib that anki maintains as errors. Rustup
auto-honors the file when invoked from inside `Vendor/anki-rs/`. Our
crate (one level up) doesn't have a pin and works on 1.92+ — but rslib
itself doesn't.

### Nested submodules

`anki/ftl/core-repo` and `anki/ftl/qt-repo` are submodules of the anki
repo. A plain `git submodule add ankitects/anki` pulls in only the
top-level repo; rslib's `anki_i18n` then panics looking for translation
files. Always use `--recursive`.

### `descriptors.bin` and the build system

rslib's `build.rs` reads `<workspace>/out/rslib/proto/descriptors.bin`
which is normally produced by anki's Ninja-based build system. We
**don't** run that bootstrap — instead, `anki_proto`'s own `build.rs`
generates the descriptors via `prost-build` as a Cargo dependency, and
rslib's `build.rs` finds them at `OUT_DIR/../../anki_descriptors.bin`.
This works because of a fallback path in
`anki_proto_gen::descriptors_path()`. If that fallback ever gets
removed, we'll need to set `DESCRIPTORS_BIN` ourselves.

### cbindgen header lookup is racy

Cargo creates a hashed build dir per build script invocation
(`target/.../build/rust_import_core-<hash>/`). Old hashes accumulate
across builds. The build script picks the newest `rust_import_core.h`
by mtime; if you ever see a stale FFI surface in the xcframework, blow
away `Vendor/RustImportCore/target/` and rebuild.

### `rusqlite` features in our crate

We depend on `rusqlite` directly with `features = ["bundled"]` so the
SQLite library inside our static library matches what rslib uses. If
you ever see "table cards has no column named X" type errors, ensure
both crates are using the same SQLite feature set.

### Static library size

The release xcframework is ~43 MB (8.5 MB → 21 MB → 43 MB across the
PR-2/3a/3b series as more rslib code becomes linker-reachable).
Linker DCE in the final app binary drops most of it; the .ipa impact is
typically 8-12 MB after stripping.

## Updating the rslib pin

```
cd Vendor/anki-rs
git fetch origin
git checkout <new-commit-or-tag>
git submodule update --init --recursive
cd ../..
git add Vendor/anki-rs
scripts/build-rust.sh release   # confirm clean build
git commit -m "build(import): bump rslib to <commit>"
```

Things that may break on a bump:

- **rslib API drift.** Anki maintainers refactor freely; methods we
  call (`Collection::import_apkg`, `render_existing_card`,
  `get_all_deck_names`, `search_notes_unordered`) are stable in
  practice but not contractually so. Compile errors will be obvious.
- **Schema migrations.** rslib runs migrations during `import_apkg`;
  newer rslib may produce different `cards.data` layouts. Our SQL in
  `flashify_card_state_as_fsrs` reads the `data` JSON via
  `CardDataBlob` (`s`/`d`/`lrt`); new fields are tolerated by serde
  defaults.
- **rust-toolchain.toml.** Anki sometimes bumps Rust. Rustup will
  auto-install the new pinned version on the next build.
- **`native-tls` may stop being enough.** If a future rslib needs
  `rustls` instead, change the feature flag in our `Cargo.toml`.

## CI hints

A minimal CI job that builds the xcframework:

```yaml
# Example for GitHub Actions on macos-14 (Apple Silicon)
- run: brew install protobuf
- run: rustup target add aarch64-apple-ios aarch64-apple-ios-sim
- run: git submodule update --init --recursive Vendor/anki-rs
- run: scripts/build-rust.sh release
- run: xcodebuild -workspace FlashReader.xcworkspace
                 -scheme FlashReader -configuration Debug
                 -destination 'platform=iOS Simulator,name=iPhone 17 Pro'
                 build
```

Cold cache: ~5-8 minutes (most spent compiling rslib's dep tree).
Caching `~/.cargo/registry`, `~/.cargo/git`, and
`Vendor/RustImportCore/target/` between runs cuts subsequent builds to
~30s. The `Vendor/anki-rs/.git/modules` directory and the resolved
submodule contents are also worth caching.

## Android

The same crate builds as an Android JNI library, `librust_import_core.so`.
Nothing about the iOS build changes: the default features, `crate-type`,
the C ABI and the generated header (byte-identical) stay as they are.

```bash
# Once: rustup target add aarch64-linux-android x86_64-linux-android \
#         armv7-linux-androideabi --toolchain 1.92.0
# Needs NDK 27.1.12297006 (or ANDROID_NDK_HOME) and protoc.
scripts/build-android.sh [OUTPUT_JNILIBS_DIR]   # default android/jniLibs/<abi>/
ANDROID_ABIS=arm64-v8a scripts/build-android.sh  # one ABI only
```

Output: `<abi>/librust_import_core.so` for `arm64-v8a`, `x86_64` and
`armeabi-v7a`, plus `SHA256SUMS` and `BUILD-INFO` (commit, rustc, NDK, API).
Tagged releases attach the same tree as `RustImportCore-android-jniLibs.zip`
(job `android` in `.github/workflows/release.yml`).

- `--no-default-features --features android`: rslib with `rustls` (no
  OpenSSL; `native-tls` cannot cross-compile without one) plus the JNI
  layer in [`src/android_jni.rs`](src/android_jni.rs), with the `jni`
  crate as an optional dependency.
- `cargo rustc --crate-type cdylib`: the shared library is requested on
  the command line, so `Cargo.toml` keeps `crate-type = ["staticlib"]`.
- 16 KB ELF segment alignment on the 64-bit ABIs (`-z max-page-size=16384`,
  `common-page-size=16384`), which Play requires on 64-bit devices for
  apps targeting Android 15+ and NDK r27 does not do by default.
  `armeabi-v7a` keeps 4 KB, as NDK r28+ does: 16 KB pages exist only for
  64-bit ABIs.
- `DT_SONAME` is set, and `--remap-path-prefix` replaces the build
  machine's paths in panic locations (`/cargo/...`, `/build/...`). The
  bridge checkout's location, the target dir and `CARGO_HOME` then no
  longer change the bytes; the absolute path of `../anki-rs` still does
  (Cargo hashes the path of a path dependency outside the workspace into
  crate metadata), so a CI build and a local build differ unless anki-rs
  sits at the same absolute path.
- After each build the script checks the ELF and stops on a failure:
  LOAD alignment, SONAME, no text relocations, `NEEDED` only from libc,
  libm, libdl, liblog and libandroid, every strong undefined symbol
  defined by those NDK stubs at API 26, no `setenv`, `Java_` exports.
- `systemstat` is locked at 0.2.5 (the version anki-rs 7d8dc017 itself
  locks): 0.2.6, pulled by fsrs → burn → burn-train, does not compile for
  `target_os = "android"`. Its Apple sources are identical in both
  versions.

The JNI layer exposes coarse calls (open a package, deck list, paged
notes, render a batch of notes with scheduler state and history,
notetype schema, render_fields, close) that return UTF-8 JSON as
`byte[]`. It repeats the C functions' bodies as `Result`-returning Rust,
because a panic cannot be caught once it reaches an `extern "C"` frame;
every JNI entry point catches panics and throws a Java exception instead.
`nativeCompareWithCAbi` reads one package through both paths so drift is
testable on a device. The Java binding class is
`com.guruowl.android.core.ankibridge.NativeBridge`; the module comment of
`src/android_jni.rs` documents handles, threading, strings and temp
files (rslib's apkg import writes a temp file through tempfile, whose
default directory is not writable for Android apps; `nativeInit` points
tempfile's process-wide override at the app's cache, without `setenv`).

## See also

- [`Vendor/RustImportCore/src/lib.rs`](src/lib.rs) — the FFI source.
  Every public function has a doc comment explaining what it wraps and
  what its memory contract is.
- [`FlashReader/Core/Services/Import/RustImportBridge.swift`](../../FlashReader/Core/Services/Import/RustImportBridge.swift)
  — the Swift side of the bridge.
- [`FlashReader/Core/Services/Import/ImportPersister.swift`](../../FlashReader/Core/Services/Import/ImportPersister.swift)
  — the consumer that walks bridge output and writes Realm objects.
- [`FlashReader/Core/Services/Import/ImportCoordinator.swift`](../../FlashReader/Core/Services/Import/ImportCoordinator.swift)
  — the state machine that ties everything together.
- [`scripts/build-rust.sh`](../../scripts/build-rust.sh) — the build
  pipeline.

## Standalone build

This crate currently depends on Anki's `rslib` via a relative path:

```toml
anki = { path = "../anki-rs/rslib" }   # TLS backend chosen by this crate's features
```

So a standalone clone needs Anki at `../anki-rs/`:

```bash
git clone https://github.com/jyehn/flashcard-guru-anki-import-bridge
git clone https://github.com/ankitects/anki ../anki-rs
cd ../anki-rs && git checkout 7d8dc01722b4b6dfbea83df6cc38f0fbaf0fdbf1
cd ../flashcard-guru-anki-import-bridge && cargo check
```

Inside `flashifyIOS` this layout is provided automatically by
`Vendor/anki-rs/` (an existing git submodule of the same upstream commit
that is sibling to `Vendor/RustImportCore/`).

Future work will switch this dependency to a Cargo `git` dep or vendor
anki-rs as a submodule of *this* repo, so the crate is buildable with
no external setup. Tracked in flashifyIOS's release plan.
