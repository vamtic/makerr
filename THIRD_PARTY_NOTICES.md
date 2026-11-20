# Third-party notices

TimelapseMaker V2 application code is licensed under the MIT License. It uses
the following third-party components.

## Slint

- Copyright: SixtyFPS GmbH
- Project: https://slint.dev
- License: Slint Royalty-free Desktop, Mobile, and Web Applications License,
  version 2.0
- License text: [LICENSES/Slint-Royalty-free-2.0.md](LICENSES/Slint-Royalty-free-2.0.md)

Slint is used as the user-interface toolkit for the desktop application.
TimelapseMaker displays the required Slint attribution in its About dialog.

## FFmpeg

- Project: https://ffmpeg.org
- Source code: https://ffmpeg.org/download.html
- License: GNU General Public License version 3 (the Windows packages use a
  GPL build from https://www.gyan.dev/ffmpeg/builds/)
- Build information: https://www.gyan.dev/ffmpeg/builds/

The Windows installer and Windows portable package bundle `ffmpeg.exe` so that
video encoding works without a separate installation. Linux packages do not
bundle FFmpeg; install it from your distribution.

## Rust dependencies

TimelapseMaker is built with Rust and a number of open-source crates. Their
license metadata is available from the lockfile and from
https://crates.io. A copy of the dependency license information can be produced
with `cargo metadata` and `cargo license` when building from source.

## Questions

If you believe a notice is missing or incorrect, please open an issue at
https://github.com/vamtic/makerr/issues.
