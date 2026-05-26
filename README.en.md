# IronPlayer

**English** | [Português (BR)](README.md)

**MPEG-TS Multicast Player & Stream Analyzer** — Rust · Windows 10/11 x86-64

> Professional MPEG-TS player and analyzer for video and streaming workflows. Free, open source, no per-seat licensing, no call-home, no feature wall.

---

## Overview

Professional MPEG-TS analysis tools (TSReader, Bitrate Viewer, Wireshark+DVB) are often expensive, closed-source, or outdated. **IronPlayer** is being built to fill that gap: a production-grade open source alternative that brings together in a single window:

- Live playback of UDP/RTP multicast streams
- Real-time visualization of Transport Stream structure
- Full PSI/SI and DVB table analysis (PAT, PMT, NIT, SDT, EIT, TDT, BAT)
- Per-PID bitrate metrics with history graph
- Detection of Continuity Counter errors, PCR jitter, and null packets

## Status

**Spec/Design phase — pre-implementation.** See the [roadmap](.specs/project/ROADMAP.md) for the delivery plan.

## Stack

| Layer               | Technology                      |
| ------------------- | ------------------------------- |
| Language            | Rust stable (MSRV 1.78)         |
| UI                  | egui 0.29 / eframe 0.29         |
| GPU Rendering       | wgpu (D3D11 — Windows)          |
| A/V Decoding        | FFmpeg 7.x via `ffmpeg-next`    |
| Audio               | cpal 0.15 (WASAPI)              |
| Inter-thread Queues | crossbeam-channel 0.5 (bounded) |

## Architecture

Cargo workspace with 4 crates:

```text
crates/net/   — UDP/RTP multicast reception
crates/ts/    — MPEG-TS demuxer + parser (pure Rust, no FFI)
crates/av/    — FFmpeg bridge (A/V decode only)
crates/ui/    — egui application
src/main.rs   — entry point, wires the channels together
```

Dependency rule: `ui → ts, av, net` · `av → ts` · `ts` and `net` are standalone.

## Prerequisites

- Rust 1.78+ (stable)
- Windows 10/11 x86-64
- FFmpeg 7.x DLLs in the root `ffmpeg/` directory (see the [technical spec](docs/ironstream-spec.md#workspace--crates))

## Build, Run & Test

```bash
# Development build (debug)
cargo build

# Optimized build (release)
cargo build --release

# Run in debug mode
cargo run --bin ironplayer

# Run in release mode
cargo run --release --bin ironplayer

# Test an individual crate
cargo test -p ts
cargo test -p net

# Lint (CI rejects warnings)
cargo clippy -p ts -- -D warnings
```

## Documentation

| Document                                                   | Content                              |
| ---------------------------------------------------------- | ------------------------------------ |
| [docs/ironstream-spec.md](docs/ironstream-spec.md)         | Complete technical specification     |
| [docs/ironstream-prd-v0.1.md](docs/ironstream-prd-v0.1.md) | Product Requirements Document        |
| [.specs/README.md](.specs/README.md)                       | Specs index and implementation order |
| [.specs/project/ROADMAP.md](.specs/project/ROADMAP.md)     | v0.1 → v1.0 phases                   |
| [.specs/project/STATE.md](.specs/project/STATE.md)         | Architectural decisions and risks    |

## License

Distributed under the MIT license. See [LICENSE](LICENSE) for details.

> **FFmpeg note:** FFmpeg DLLs are distributed separately under LGPL 2.1+. IronPlayer links to them dynamically and does not embed them into the main binary.