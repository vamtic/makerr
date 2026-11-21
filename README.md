# TimelapseMaker V2

[![CI](https://github.com/vamtic/makerr/actions/workflows/ci.yml/badge.svg)](https://github.com/vamtic/makerr/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/vamtic/makerr?include_prereleases&label=release)](https://github.com/vamtic/makerr/releases)
[![Downloads](https://img.shields.io/github/downloads/vamtic/makerr/total)](https://github.com/vamtic/makerr/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows-lightgrey.svg)](#installation)

> **Turn pixel-art canvas history into timelapse videos.**
> Pick a canvas (PixelPlanet, PixelYa, PixMap, PXU…), select a region and a date
> range, and let the app download tiles and encode them to MP4, MKV, WebM, MOV
> or AVI via ffmpeg. Available as a **desktop GUI** and a **headless CLI**.

**V2 is a complete Rust rewrite of the original V1 app.** Same features, same
`settings.json` / template formats, but with a fully async engine, zero-disk
rendering and no Python runtime or bundled interpreter. V1 settings are imported
automatically on first launch.

---

## Contents

- [About](#about)
- [Features](#features)
- [Quick start](#quick-start)
- [Installation](#installation)
- [Upgrading from V1](#upgrading-from-v1)
- [Usage](#usage)
  - [Desktop GUI](#desktop-gui)
  - [Headless CLI](#headless-cli)
  - [CLI subcommands](#cli-subcommands)
  - [Template format](#template-format)
- [Configuration](#configuration)
  - [Settings reference](#settings-reference)
  - [Adaptive concurrency](#adaptive-concurrency)
  - [Tile cache](#tile-cache)
- [Architecture](#architecture)
- [Development](#development)
- [Contributing](#contributing)
- [License](#license)

---

## About

Pixel canvases store a history of snapshots. TimelapseMaker walks that history
for a selected rectangle, composes one frame per snapshot (or per sampled day),
and pipes the frames to **ffmpeg** to produce a smooth timelapse. Everything
that the GUI can do is also available from the CLI, so it fits automation and
CI pipelines.

| | |
|---|---|
| **Engine** | `tlm-core` — async, UI-independent Rust library |
| **Interfaces** | `timelapse` — one binary: no arguments opens the Slint GUI, any clap subcommand runs headless |
| **Compatibility** | Imports V1 settings and templates |
| **Website** | <https://maker.vamtic.us.to> |
| **License** | MIT application code; the GUI uses Slint under the Royalty-free 2.0 license |

---

## Features

<details open>
<summary><strong>Acquisition &amp; caching</strong></summary>

- 📦 Multi-server support with presets (PixelPlanet, PixelYa, PixMap, PXU…) and custom domains
- 🔀 Fully async tile downloads (HTTP/2, connection pooling) with multi-frame prefetching
- 💾 Packed `cache.redb` tile store; legacy loose-file caches migrate automatically
- 🧹 Configurable retention with automatic pruning and instant cache stats

</details>

<details open>
<summary><strong>Rendering &amp; Studio FX</strong></summary>

- ⚡ **Zero-disk rendering** — raw RGBA frames stream straight into ffmpeg's stdin (no thousands of intermediate PNGs)
- 🚀 AVX2-accelerated blending with bit-exact parity, and fast `zune-png` decoding (automatic fallback to the `image` decoder)
- 🎨 Burned-in timestamp clock, progress line, watermark, Ken Burns pan/zoom, looped/faded soundtrack
- 📱 Social framing presets: 9:16 Shorts (pixelated mirror bars), 1:1, 16:9
- 🖼 Live area preview with day timeline and a live rendering theater that streams composed frames into the GUI while encoding

</details>

<details open>
<summary><strong>Output &amp; automation</strong></summary>

- 🎬 Codecs: **H.264**, **H.265** (libx265), **AV1** (libsvtav1) plus hardware encoders (NVENC / AMF / QSV / VA-API) when available
- ⏱ Live encoding progress and ETA; resumable PNG runs (`--resume`)
- 🤖 Headless CLI with subcommands for scripting and CI
- 🌍 English / Hungarian UI

</details>

<details open>
<summary><strong>Compatibility</strong></summary>

- 🧩 **V1 compatible**: imports the old `settings.json`, keeps the template JSON schema, and adds a packed `cache.redb` tile store

</details>

---

## Quick start

```sh
git clone https://github.com/vamtic/makerr.git
cd makerr
cargo build --release

# GUI (no arguments)
./target/release/timelapse

# CLI — render from a template
./target/release/timelapse run --template my/region.json

# CLI — render fully from flags
./target/release/timelapse run --canvas <id> --from -128_-128 --to 128_128 \
  --start 2000-01-01 --end 2000-01-07 --out out/ \
  --fps 30 --codec h264 --format mp4
```

> **ffmpeg must be reachable on `PATH`** (or placed next to the executable) for video encoding.

---

## Installation

No Python and no separate runtime is needed. The prebuilt Windows package
bundles `ffmpeg`; on Linux, install `ffmpeg` from your distribution and make
sure it is on `PATH`. The Linux GUI needs an X11 or Wayland session and the
usual GL/font packages; Windows needs nothing extra.

### Prebuilt binaries

Grab the latest installer or portable archive from the
[Releases page](https://github.com/vamtic/makerr/releases) — every push to
`master` updates the `latest` release with Linux (`.tar.gz`, `.zip`) and Windows
(installer, portable `.zip`) artifacts built by [GitHub Actions](https://github.com/vamtic/makerr/actions/workflows/ci.yml).
The [V2 website](https://maker.vamtic.us.to) links the same files dynamically,
and every release also ships a `SHA256SUMS` file.

| Platform | Artifact |
|---|---|
| Linux x86_64 | `TimelapseMaker-linux-x86_64.tar.gz` / `.zip` |
| Windows x64 | `TimelapseMaker-Setup.exe` / `TimelapseMaker-windows-portable.zip` |

```sh
# Linux
tar -xzf TimelapseMaker-linux-x86_64.tar.gz
./install.sh    # binary, menu entry and icon into ~/.local
timelapse

# Windows
# Run TimelapseMaker-Setup.exe, or extract the portable zip and start timelapse.exe
```

Running `./timelapse` straight from the extracted folder works too, but on
Wayland (KDE Plasma, GNOME) the title bar and the taskbar take the app icon
from an installed `.desktop` entry rather than from the window itself, so
without `install.sh` they show a generic icon. `install.sh` copies the binary
to `~/.local/bin`, the entry to `~/.local/share/applications` and the icon
sizes to `~/.local/share/icons/hicolor`; X11 sessions show the icon either way.

### Build from source

Building requires a **stable Rust toolchain** (1.85 or newer) and `ffmpeg` at
runtime.

```sh
cargo build --release
```

The build produces a single binary in `target/release/`:

| Binary | Purpose |
|---|---|
| `timelapse` | The application: the desktop GUI when started without arguments, the headless CLI for every subcommand |

On Linux, `packaging/linux/install.sh target/release/timelapse` installs that
binary together with the desktop entry and the icon sizes for the current user.

Run the tests:

```sh
cargo test --workspace
```

---

## Upgrading from V1

V2 replaces the Python application with a single native binary, but it keeps
reading the old data:

- On first launch, a V1 `settings.json` next to the old script/executable is
  imported into the V2 config file. The original file is left untouched.
- The V2 config file lives in the per-user config folder
  (`%APPDATA%\TimelapseMaker\settings.json` on Windows,
  `~/.config/TimelapseMaker/settings.json` on Linux). A portable
  `maker/settings.json` in the working directory is used instead when it
  already exists, so extracted portable builds keep working.
- V1 template JSON files load as-is.
- V2 keeps downloaded tiles in the packed `cache.redb` store; there is no V1
  tile cache to migrate.
- The binary is called `timelapse` (no extension) instead of
  `Timelapse Maker V1.2.2.py` / `.exe`.
- Python, pip and the old dependency list are no longer needed or shipped.

If you want to keep using V1, keep its folder; V2 does not delete or modify it.

---

## Usage

### Desktop GUI

```sh
./target/release/timelapse
```

Starting the binary with no arguments opens the GUI. On Linux this requires an
X11 or Wayland session; without one the binary prints a hint and exits with
code 2 instead of failing inside the windowing system.

- **Left column** — canvas picker, corner coordinates, date range, output folder,
  Create / Render-only / Cancel, and a console log (clear, copy, errors-only filter).
- **Right column** — live area preview with a ◀ ▶ day timeline (scroll to zoom,
  drag to pan), pipeline stages, progress with ETA and download statistics.
- **Fullscreen** — press `F11` or use the fullscreen button in the header;
  `Esc` leaves fullscreen.

### Headless CLI

Every subcommand runs headless — the same binary, no window:

```sh
# From a template file (same schema the GUI exports)
timelapse run --template my/region.json

# Or fully from flags
timelapse run --canvas <id> --from -128_-128 --to 128_128 \
  --start 2000-01-01 --end 2000-01-07 --out out/ \
  --fps 30 --codec h264 --format mp4

# Extra flags
timelapse run ... --dry-run          # show the frame plan without downloading
timelapse run ... --frames-only      # write a PNG sequence instead of streaming
timelapse run ... --no-stream        # same, but keep the normal frame pipeline
timelapse run ... --save-frames      # keep t*.png files (the default)
timelapse run ... --no-save-frames   # stream only, discard the frames afterwards
timelapse run ... --timestamps --progress-bar --watermark "My Faction"
timelapse run ... --camera zoom_out  # none | zoom_out | zoom_in | pan_lr | pan_rl
timelapse run ... --audio soundtrack.mp3
timelapse run ... --preset vertical  # source | vertical (9:16) | square | wide
timelapse run ... --link-mode auto   # auto | hard (ln) | soft (ln -s) | copy
```

`--domain <domain>` also selects that preset's storage domain; pass
`--storage <base>` as well when using a custom mirror.

By default a run **streams** composed frames into ffmpeg and also keeps the
`t*.png` sequence on disk. Pass `--no-save-frames` to stream only and discard
the frames; `--frames-only`, `--no-stream` and `--resume` switch to a
PNG-sequence-first run (the resumable artefact). `--save-frames` is still
accepted for compatibility.

**Exit codes:** `0` success · `1` cancelled/failed · `2` invalid input.

During a run it prints progress and status to stdout:

```text
[status] Processing: 2000-01-03
[progress] 42/120
Done: out/timelapse.mp4 (350 frames, 2m 11s total)
```

### CLI subcommands

| Subcommand | Purpose |
|---|---|
| `run` | Download frames and encode a timelapse video |
| `list-canvases` | List canvases (`--domain pixmap.fun`) |
| `preview` | Render a single day to PNG (e.g. `--day 2000-01-02 -o preview.png`) |
| `relink` | Turn duplicate frames into links (`--out out/`) |
| `cache-stats` | Show tile-cache statistics |
| `clear-cache` | Remove cached tiles |
| `prune-cache` | Prune tiles older than `--days N` |

Examples:

```sh
timelapse list-canvases [--domain pixmap.fun]
timelapse preview --canvas <id> --from 0_0 --to 255_255 --day 2000-01-02 -o preview.png
timelapse relink --out out/
timelapse cache-stats | clear-cache | prune-cache --days 7
```

### Template format

```json
{
  "canvas_id": "b740af83...",
  "first_cord": "-128_-128",
  "second_cord": "128_128",
  "start_date": "2000-01-01",
  "end_date": "2000-01-07",
  "output_folder": "/path/to/output/",
  "domain": "pixelplanet.fun",
  "storage_domain": "storage.gs-os.com"
}
```

| Field | Meaning |
|---|---|
| `canvas_id` | Canvas UUID — from `timelapse list-canvases`, the GUI, or `https://<domain>/api/canvases` |
| `first_cord` / `second_cord` | Two `x_y` pixel corners (order-independent) |
| `start_date` / `end_date` | Inclusive `yyyy-MM-dd` range |
| `domain` / `storage_domain` | Optional site override |

---

## Configuration

The settings file lives in the per-user config folder
(`%APPDATA%\TimelapseMaker\settings.json` on Windows,
`~/.config/TimelapseMaker/settings.json` on Linux) and is written on first run
and every time settings are saved. A portable `maker/settings.json` in the
working directory takes precedence when it already exists, and `--settings`
overrides both. The schema stays compatible with V1, and a V1 `settings.json` is
imported automatically when the V2 file does not exist yet.

### Settings reference

| Key | Meaning |
|---|---|
| `video_codec` | `h264` / `h265` / `av1` |
| `hardware_acceleration` | `auto` / `none` / `nvenc` / `amf` / `qsv` / `vaapi` |
| `cache_ttl_days` | Tile-cache retention in days (`0` = keep forever) |
| `max_connections` | Parallel connections for history fetching |
| `max_concurrent_tiles` | Tiles downloaded per frame |
| `adaptive_concurrency` | Adaptive limits from server responses (`true`) |
| `link_mode` | `auto` / `hard` / `soft` / `copy` for unchanged frames |
| `preview_limit` | Max tiles shown in the preview |
| `stream_video` | Stream RGBA frames to ffmpeg (`true` by default) |
| `save_png_sequence` | Keep `t*.png` frames while streaming (`true` by default) |
| `show_timestamp` | Burn the animated date/time clock into frames |
| `timestamp_format` | `chrono` format string for the clock |
| `show_progress_bar` | Draw the bottom progress line |
| `watermark_text` | Custom branding text |
| `watermark_position` | `top_left` / `top_right` / `bottom_left` / `bottom_right` |
| `camera_motion` | `none` / `zoom_out` / `zoom_in` / `pan_left_to_right` / `pan_right_to_left` |
| `camera_strength` | Camera travel in percent (1–90) |
| `audio_path` | Optional soundtrack file |
| `audio_fade_ms` | Audio fade-in/out in milliseconds |
| `social_preset` | `source` / `vertical_9_16` / `square_1_1` / `wide_16_9` |

### Adaptive concurrency

Both download phases start from the limits above but tune themselves unless
`adaptive_concurrency` is turned off. The history fetcher starts at
`min(max_connections, 12)` and the tile fetcher at `max_concurrent_tiles`;
successful requests grow the pool additively (history up to 16), while HTTP
429/503 (and any `Retry-After` the server sends) halve it, with a short cooldown
so a burst that was already in flight cannot immediately undo the reduction. If
a history request still fails after all retries, the day is retried once more
after the burst has drained instead of being dropped. A fast server is not kept
at the old conservative limits, and a throttled burst no longer leaves holes in
the timeline.

While the history range is downloading, the pipeline warms the tile cache:
every day base is queued immediately, and the kept snapshot frames are added as
each day's timestamps arrive. History lives on `domain`, tiles on
`storage_domain`, so the two streams do not share a rate limit; by the time the
frame loop starts, the first days are already on disk and the history wait
hides a real part of the download time. The warmer schedules frames rather than
whole days, so one day's snapshots are not serialized behind its base and the
tile pool is filled up to `max_concurrent_tiles` even for small regions.
`max_connections` and `max_concurrent_tiles` keep working as starting points and
upper hints.

### Tile cache

Tiles are stored in a single packed redb database
(`~/.cache/TimelapseMaker/tilecache/cache.redb` on Linux), keyed by an MD5
digest of the tile URL. Existing loose-file caches are migrated in the
background on first use; until that finishes both layouts are readable. Pruning
is a B-tree scan instead of a directory walk, and cache stats are instant. If
redb cannot be opened (another process holds the file lock, read-only cache
directory), the app transparently falls back to the loose-file cache. The
`TIMELAPSE_CACHE_DIR` environment variable overrides the cache root.

---

## Architecture

```text
crates/
├── tlm-core/     engine library (no UI deps)
│   ├── coord.rs      x_y corner parsing
│   ├── site.rs       per-site quirks (PixMap chunk layout, /api/canvases, …)
│   ├── tileurl.rs    tile URL construction (both storage layouts)
│   ├── settings.rs   settings load/save/clamp (V1-compatible schema)
│   ├── i18n.rs       EN/HU strings (%1-style placeholders)
│   ├── cache.rs      MD5-keyed tile cache API, pruning, stats
│   ├── cache/        packed redb store + legacy background migrator
│   ├── simd/         AVX2 / scalar bit-exact blend kernels
│   ├── stream/       zero-disk ffmpeg stdin encoder + buffer pool
│   ├── studio/       overlays (bitmap font), camera, social presets, audio
│   ├── net.rs        HTTP client, site headers, retries
│   ├── adaptive.rs   AIMD concurrency limiter for history/tiles
│   ├── history.rs    canvas list + per-day snapshot timestamps
│   ├── compose.rs    parallel zune-png decode + frame composition
│   ├── pipeline.rs   the async pipeline (prefetch, compose, stream/save)
│   ├── video.rs      ffmpeg discovery, HW detection, encoding
│   └── template.rs   template JSON import/export
└── tlm-app/      `timelapse` binary (CLI + Slint GUI)
```

<details>
<summary><strong>What changed in V2</strong></summary>

- One shared pooled HTTP/2 client replaces the per-thread request workers and
  event-loop sliding-window machinery of the old app.
- 404s are no longer retried (a missing tile is legitimate history state);
  genuine network errors retry with backoff.
- History snapshots are **cumulative diffs against the day base**, not deltas
  between consecutive snapshots. Each frame is rendered as day base + the
  current cumulative snapshot (`compose::apply_cumulative_snapshot`): pixels
  dropped from a later snapshot are restored to the day base, so erased pixels
  really disappear instead of ghosting for the rest of the video.
- Unchanged frames are **linked** to their predecessor instead of re-encoded:
  `link_mode = auto` (default) tries a hardlink (`ln`), then a symlink
  (`ln -s`), then a plain copy — so it works on Linux/macOS, on NTFS/ReFS on
  Windows, and even on exFAT/network shares where hardlinks are unsupported.
  `--link-mode hard|soft|copy` forces one mechanism. The change signal is
  computed during the cumulative-snapshot pass and is only set when the final
  canvas really differs, so identical frames remain linkable. Unchanged frames
  stay at tile-footprint work (no whole-frame copy or byte compare) and peak
  memory stays at one frame buffer plus a compact per-tile mask; the link count
  *and* the mechanism actually used are reported in the CLI summary, the GUI
  log and `PipelineResult::{linked_frames, link_kind}`.
- Downloads reuse one process-wide pooled HTTP/2+TLS client and the caller's
  runtime, so connections (and their handshakes) survive across frames and
  previews; cache writes no longer `fsync` per tile, are batched off the async
  worker, and the cache root is resolved once per frame instead of per tile.
- Frames are written through temp files, so an interrupted run never leaves a
  half-written frame that `--resume` would adopt.
- Hardware-encoder probes run with a timeout so a hung `nvidia-smi` cannot
  stall encoding.
- `--dry-run`, `--frames-only`, `list-canvases` and single-day `preview`
  rendering are new headless capabilities.

</details>

---

## Development

```sh
cargo fmt --all -- --check          # formatting
cargo clippy --workspace --all-targets  # linting
cargo test --workspace              # tests
cargo build --release --workspace   # release build
```

CI runs formatting, clippy and the test suite on Linux, plus a Windows compile
check, for every push to `master` and for every pull request. Pushes to
`master` also build and publish the release artifacts. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the full workflow.

For GUI development with the Slint MCP server enabled, use the provided helper
scripts:

```sh
./slintmcpstart.sh         # starts the GUI with the Slint MCP server on :8080
```

The `image_parity` crate under `crates/tlm-core/vendor/` is a test-only,
byte-identical copy of the exact `image` crate version used here, with a few
private items made public so the blend-parity test can call the same src-over
blend that `imageops::overlay` uses. Keep it in sync with
`[workspace.dependencies.image]` when bumping `image`.

---

## Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) first.
Use [Issues](https://github.com/vamtic/makerr/issues) for bug reports and
feature requests, and [Discussions](https://github.com/vamtic/makerr/discussions)
for questions and ideas. Security issues should be reported privately as
described in [SECURITY.md](SECURITY.md).

---

## License

TimelapseMaker application code is released under the [MIT License](LICENSE)
© vamtic.

The desktop GUI is built with [Slint](https://slint.dev) under the
[Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0](LICENSES/Slint-Royalty-free-2.0.md).
Third-party components, including the FFmpeg build bundled with the Windows
packages, are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
