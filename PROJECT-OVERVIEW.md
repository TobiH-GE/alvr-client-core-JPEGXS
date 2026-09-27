# JPEG XS low-latency streaming for ALVR — project overview

Private working copy. This file is identical in all four repositories of the project;
it exists so that someone (or some model) landing in any one of them can see the whole
picture before reading code.

## What this is

[ALVR](https://github.com/alvr-org/ALVR) streams VR from a Windows PC to a headset. This
project replaces its video codec path with **JPEG XS** (Intel's SVT-JPEG-XS) for a
**Apple Vision Pro** client, aiming at very low latency: JPEG XS is intra-only and can be
encoded, transmitted and decoded **slice by slice**, so a frame does not have to be
complete at any stage before the next stage starts on it.

Target format: **4288x1664, 90 fps, 3-6 bpp**, two image columns (left/right eye half)
encoded and decoded independently.

The slice pipeline is the point of the whole exercise. A slice is handed to the next
stage the moment its packets are complete. Anything that batches slices back together
for convenience defeats the purpose and must be justified by measurement.

## The four repositories

| Repository | What it holds | Branch |
|---|---|---|
| `ALVR-JPEGXS-private` | The ALVR fork: streamer (server, Windows), encoder integration, slice packetization, send path | `jpegxs-column-split-gpu-offload` |
| `SVT-JPEG-XS-private` | Fork of Intel's SVT-JPEG-XS: CUDA encode path, CPU/GPU hybrid, rate control | `main` |
| `alvr-visionos-private` | The visionOS client app (Swift + Metal) and the vendored JPEG XS decoder (`JXSDecoderSrc/`) | `jpegxs-column-split-gpu-offload` |
| `alvr-client-core-private` | Submodule of the client: ALVR's Rust `client_core` and `sockets` (receive path, discovery) | `jpegxs-column-split-gpu-offload` |

`alvr-visionos-private` references `alvr-client-core-private` as its `ALVR` submodule, so
clone it with `--recurse-submodules`. Both ALVR forks also carry Valve's public `openvr`
as a submodule; that one points at the upstream repository and needs nothing special.

## Building

### Server (Windows)

Needs Visual Studio 2022, CMake >= 3.16, YASM on PATH, and a CUDA toolkit for the GPU
encode path.

1. Build SVT-JPEG-XS from `SVT-JPEG-XS-private`:
   ```
   cd SVT-JPEG-XS\Build\windows
   build.bat 2022
   ```
2. Copy its output into the ALVR tree (see `deps/windows/svt-jpegxs/README.md` in the
   server repo for the exact layout — `include/`, `lib/`, `bin/`). `build.rs` detects that
   folder and only then defines `ALVR_JPEGXS`.
3. Build the streamer:
   ```
   cargo xtask build-streamer --release --keep-config
   ```
   **`--keep-config` is not optional in practice.** Without it the build deletes
   `session.json`, which holds both the settings and the headset pairing.

### Client (macOS, for Apple Vision Pro)

Needs Xcode with the visionOS SDK and a provisioning profile. From the repository root:

```
bash deploy_test.sh
```

This rebuilds `JXSDecoder.xcframework` from `JXSDecoderSrc/`, builds the app for the
device, installs and launches it. It must run in a real GUI Terminal window — code
signing needs the unlocked login keychain, which a plain SSH session cannot reach.

The app writes its own log to `Documents/jxstest_debug.log` inside its container; pull it
with `xcrun devicectl device copy from --domain-type appDataContainer`. Every diagnostic
mentioned below lands there, once per second or once per five seconds.

## Where the work is

**Server** (`ALVR-JPEGXS-private`)
- `alvr/server_core/src/connection.rs` — send thread pinned to performance cores, the
  per-second diagnostic line (µs per shard, queue depth, CPU/GPU load via NVML).
- `alvr/server_core/src/lib.rs` — whole-frame admission: a frame is accepted or dropped as
  a unit, so a refused frame never contributes a half image.
- `alvr/sockets/src/` — `send_segmented()` and Windows UDP segmentation offload
  (`UDP_SEND_MSG_SIZE`), roughly 46 shards per syscall.
- Slice packetization: 105 units per column (1 header + 104 slices), 210 packets per
  frame, `packet_index = column * 105 + unit_index`.

**Client** (`alvr-visionos-private`)
- `ALVRClient/JXSTestPlayer.swift` — the decode driver, GPU dispatch threads, frame
  assembly from slice packets, and the stage-split diagnostics.
- `JXSDecoderSrc/jxs_native_bridge.c` — the bridge between Swift and the decoder: the
  incremental GCLI describe, the GPU output store, the staging ring, all counters.
- `JXSDecoderSrc/Lib/Decoder/Codec/Packing.c` — the decoder's own unpack path, with the
  GPU substitution site where GPU-produced GCLI bytes replace CPU decoding.

**Client receive path** (`alvr-client-core-private`)
- `alvr/sockets/src/stream_socket.rs` — one syscall per datagram instead of per shard.
- `alvr/client_core/src/connection.rs` — receive thread at `QOS_CLASS_USER_INTERACTIVE`.
- `alvr/client_core/src/sockets.rs` — discovery announces on wired interfaces first.

## Measured state (2026-09-24)

With a game running (so the server actually produces 85-100 fps), 4288x1664:

| | GCLI on CPU | GCLI on GPU |
|---|---|---|
| `gcli_decode`, worker CPU per frame | 21.9 ms | 6.3 ms |
| GPU done after deadline | 245 of 450 | 7-11 of 450 |
| slack to deadline | -1.11 ms | +4.7 ms |

Data correctness is verified continuously rather than assumed: the driver's descriptors
are compared against the decoder's reference describe (0 differing over 5.4 million
cases) and the frame numbering on both sides is cross-checked (0 mismatched over 42
million checks).

## Things that will mislead you

- **Idle SteamVR sends ~25 fps, not 90.** With no game running the server produces about
  5250 packets/s and the client's frame assembly starts giving up on incomplete frames.
  This looks exactly like a server or network fault and is neither. Always measure
  throughput with a game running.
- **Worker stage timers measure wall time, not work.** If every stage inflates by roughly
  the same factor at once, the threads are not running — look for CPU starvation (too
  many GPU dispatches, lock contention), not for a slow function.
- **Successive frames have identical geometry.** A descriptor set from the wrong frame
  passes every plausibility check and produces wrong pixels, not an error. Anything keyed
  by frame must be looked up by frame, never by "whichever is current".
- The bottom 16 rows and right 18 columns of each eye are ALVR foveation padding. They
  are outside the region the client un-foveates and are not a decoder bug.

## Open items

- Total worker CPU per frame rose when GCLI moved to the GPU (`entropy_total` 32 -> 42 ms)
  even though wall-clock improved. That friction is unclaimed headroom.
- The server has shown a bistable "slow send state" (17-30 µs per shard instead of
  1.0-1.5) in earlier sessions. Not reproduced under observation yet; two `xperf` traces
  both caught fast sessions.
- The in-encoder P4 copy produces chroma errors in live mode; live therefore uses the
  ordered copy path. Root cause open.
