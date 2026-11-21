# NexMirror

Low-latency screen mirroring between Windows PCs on the same local network.
Run a game on your desktop PC, keep your controller and headset connected to it, and watch
the screen (with sound) on your laptop anywhere in the house.

* **One portable file.** `NexMirror.exe` (≈4 MB). No installer, no runtimes, no drivers.
* **Hardware end to end.** GPU capture (DXGI Desktop Duplication), GPU colour conversion,
  hardware H.264 encoding (NVIDIA NVENC, AMD AMF, Intel Quick Sync) and hardware decoding.
* **Built for latency.** Low-latency encoder mode, no B-frames, keyframes only when needed,
  one frame in flight, UDP transport, flip-model presentation with tearing allowed in full screen.
* **Survives Wi-Fi packet loss.** Reed-Solomon forward error correction rebuilds lost packets
  without waiting for a resend.
* **Secure by default.** First connection is approved with a 6-digit code (SPAKE2
  password-authenticated key exchange). All video, audio and control traffic is encrypted
  with AES-256-GCM. Pairing secrets are protected with Windows DPAPI.

## Using it

1. On the PC with the game, open NexMirror → **Share this PC** → turn sharing on.
   When Windows Firewall asks, choose **Allow** (needed so other devices can reach it).
2. On the laptop, open NexMirror → **Connect**. The PC appears under *Available on your network*
   (or type the address shown on the PC's Share page). Click **Connect**.
3. The first time, the PC shows a 6-digit code. Type it on the laptop. Next time no code is needed.

| Shortcut | Action |
| --- | --- |
| `F11` or double-click | Toggle full screen |
| `Esc` | Leave full screen |

## Getting the lowest latency

* Connect the shared PC by **Ethernet** and the laptop to **5 GHz (or 6 GHz) Wi-Fi**.
  2.4 GHz Wi-Fi adds more delay than everything else combined.
* Use **full screen** on the laptop: it bypasses desktop composition.
* If the PC's monitor runs above 60 Hz, set **Frame rate** to 120 or 144 on the Share page.
* *Resolution: Match viewer screen* (default) streams at the laptop's resolution, which keeps
  bandwidth and encode time down.

The performance overlay (chart icon while viewing) shows live latency from capture on the PC to
decoded frame on the laptop, plus frame rate, bitrate, network round-trip and repaired packets.

## Requirements

* Windows 10 or 11 (64-bit) on both PCs.
* A GPU with hardware H.264 encoding on the shared PC (any NVIDIA GTX 600+, AMD GCN+ or
  Intel HD 4000+). Any laptop GPU from the last decade can decode.
* Both devices on the same local network. NexMirror uses TCP and UDP port 47800.

Settings and logs live in `%APPDATA%\NexMirror` (logs: Settings → Open logs).

## Building

Requires [Rust](https://rustup.rs) with the `x86_64-pc-windows-gnu` toolchain (no Visual Studio needed).

```powershell
.\build.ps1
```

The script runs the unit tests and writes `dist\NexMirror.exe`.
