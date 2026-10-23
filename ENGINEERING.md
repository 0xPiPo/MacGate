# MacGate: engineering notes

This file explains why MacGate is built the way it is, and the bugs that shaped it. Each
section gives the symptom, the evidence that led to the cause, and the fix. The bugs below
were found by testing against a real iPhone, a real monitor and a real Mac. Unit tests did
not catch them.

---

## 1. Architecture

```
iPhone / PC (Microsoft Windows App, mstsc, FreeRDP)
        │  RDP over TLS (NLA / CredSSP)
        ▼
┌───────────────────────────── MacGate.app (SwiftUI, menu bar) ─────────────────────────────┐
│  UI, onboarding, settings, curtain shield windows                                         │
│        │  JSON Lines over stdin/stdout                                                    │
│        ▼                                                                                  │
│  macrdp-server control  (Rust helper, same bundle)                                        │
│   ├─ ironrdp-acceptor-patched : X.224 / TLS / CredSSP (NTLM) / connect sequence             │
│   ├─ ironrdp-server-gfx       : session loop, GFX pipeline (AVC420, RemoteFX Progressive)   │
│   ├─ macrdp-core              : auth, sessions, display pipeline thread, config, lifecycle  │
│   ├─ macrdp-capture           : ScreenCaptureKit, CGVirtualDisplay, DDC monitor power       │
│   ├─ macrdp-encode            : VideoToolbox H.264, colour conversion                       │
│   ├─ macrdp-input             : CGEvent injection, local-input blocker                      │
│   ├─ macrdp-audio / clipboard : RDPSND, CLIPRDR (text, HTML, files)                         │
└───────────────────────────────────────────────────────────────────────────────────────────┘
```

**Why the server runs as a helper process, not an FFI library.** The app starts the helper
and the two talk JSON Lines over stdin and stdout.
- No unsafe Swift↔Rust bindings are needed.
- A crash in the server cannot take the UI down, and the helper exits as soon as stdin closes.
- macOS charges Screen Recording and Accessibility (TCC) to the app, because the app is the
  responsible process.

**Why Rust plus IronRDP.** IronRDP gives a memory-safe implementation of the RDP protocol.
MacGate patches two of its crates:
- the acceptor, for authentication;
- the server, for the GFX pipeline, flow control and Display Control.

---

## 2. Security model

**Threat model.** The owner reaches the Mac over the LAN or WireGuard, never from the open
internet. The attacker is anyone else on that network.

| Decision | Why |
|---|---|
| **NLA only.** The server offers `HYBRID \| HYBRID_EX` and refuses TLS-only or standard RDP security with `HYBRID_REQUIRED_BY_SERVER`. | The upstream code offered TLS-only and accepted an *empty password* in ClientInfo. Anyone on the LAN got the screen, keyboard and clipboard. |
| **Login with the Mac account.** NTLM inside CredSSP runs against the NT hash, `MD4(UTF-16LE(password))`. Only the hash is stored, in the Keychain. | The password is verified once against OpenDirectory when you enroll. NTLM needs only the hash, so the plaintext is never stored. |
| **The delegated password is re-checked against OpenDirectory** after CredSSP. | A stale hash would otherwise keep working after the Mac password changes. A Restricted Admin / Remote Credential Guard client sends no password and is refused. |
| **Handshake hardening.** Each connection gets its own task. Timeouts per stage: X.224 3 s, TLS 3 s, CredSSP 30 s, connect sequence 10 s. At most 2 handshakes per IP and 8 in total, and the oldest unauthenticated handshake is evicted when full. Backoff per IP, plus a global failure budget. | The upstream accept loop handled one client at a time with no timeouts, so a single idle TCP connection locked the owner out. |
| **No secrets in logs.** The password-bytes log is gone, `SSLKEYLOGFILE` is removed, log lines escape control characters, and log files are created 0600 with `O_NOFOLLOW`. | The upstream code logged both passwords on every failed login. |

**Known tradeoffs.**
- The Keychain holds an unsalted MD4 of the login password. That is faster to crack offline
  than macOS's own salted hash.
- Curtain privacy depends on the monitor obeying DDC standby (see §6).

---

## 3. Why an iPhone gets RemoteFX Progressive, not H.264

**Symptom.** The iPhone stayed on "Configuring remote PC…" for ever. Mouse input still worked.

**Evidence and fixes, in order:**

1. **Capabilities were dropped.** `ironrdp-pdu` 0.7 rejects the *whole* GFX
   `CapabilitiesAdvertise` if one capability set has a version it doesn't know. The iOS client
   sends `0xB0101` and `0xB0300`. The handler dropped the decode error at debug level, so the
   server never sent a CapsConfirm.
   - **Fix:** skip the unknown sets and decode again. MS-RDPEGFX says the server ignores them.
2. **No H.264.** Once the capabilities decoded, the log showed `AVC_DISABLED` on every set. The
   iOS client has no H.264 decoder for RDP; the xrdp wiki confirms this.
   - **First try, uncompressed GFX:** it worked, but a full repaint is about 5 MB, so scrolling
     ran at a few frames per second.
3. **RemoteFX CAVIDEO was rejected.** The client disconnected about 40 ms after the first
   frame with protocol error `0x200d`.
   - FreeRDP's `rfx_process_message` decoded the exact same bytes offline, so the payload was
     valid. The Microsoft client simply doesn't accept CAVIDEO over GFX.
4. **RemoteFX Progressive works.** This is what Windows hosts and xrdp send to this client:
   `WireToSurface2`, codec `0x0009`, TILE_SIMPLE blocks, RLGR1. It worked first time.
   - Before it ran live, FreeRDP's progressive decoder had already accepted it, at PSNR 32 dB.
5. **A library bug found on the way.** `ironrdp-graphics` 0.7's RLGR1 encoder uses `UP_GR`
   where the spec says `UQ_GR`, so it produces invalid streams. MacGate has its own small RLGR1
   coder, covered by a round-trip test.

**Cost.**
- Only dirty 64×64 tiles are encoded, and a static screen costs nothing.
- A full 816×1544 frame takes 3–8 ms on 4 threads and is about 0.5–1.1 MB, against 5 MB
  uncompressed.
- With quality set to **Auto**, the quant table follows congestion: backlog, coalesced frames
  and RTT.

---

## 4. Latency pipeline

Problems in the upstream pipeline:
- It sent each frame one capture late.
- It had no flow control, so TCP queues grew and the stream stalled.
- Capture, encoding and input all ran on one thread.
- It defaulted to the software encoder at about 99 Mbps.

**Fixes:**
- **Ack-gated send-on-encode.** A frame is sent the moment it is encoded. While more than
  `baseline_in_flight + 2` frames are unacknowledged, the newest captured frame is held and
  coalesced, never dropped after encode, because dropping would break the H.264 reference
  chain. The window grows with `min RTT × fps` on slow links.
- **A dedicated pipeline thread** handles capture and encoding. The session task only does I/O.
  `TCP_NODELAY` is set.
- **VideoToolbox hardware H.264** by default, at about `pixels × fps × 0.12` bits per second
  (about 15 Mbps at 1080p60). The bitrate reacts to congestion, not to encode time.

---

## 5. Display: virtual displays, and the bugs they caused

Each session gets a `CGVirtualDisplay` (a private API, also used by DeskPad and BetterDisplay)
sized to the client, so the Mac renders at the phone's real resolution. Three display modes:

- **Mirror:** the physical monitors mirror the session display.
- **Extend:** the session display is an extra screen next to them.
- **Curtain:** private; see §6.

| Bug | Evidence | Fix |
|---|---|---|
| **The cursor stuck to the left edge of the main screen** in extend and curtain modes. | macOS placed the virtual display at `x = -836`. `MouseCoordMapper::map` returned `u16`, so every negative x became 0. | Signed coordinates end to end, with a test for a display left of main. |
| **Extend looked mirrored.** | The new display reported bounds `(0,0)` and no mode, so macOS had never applied the arrangement. | One app-only configuration that un-mirrors every display and places the new one explicitly, then waits for it to settle. |
| **Wrong resolution on about every second connect.** | `display arrangement configuration failed: 1001` appeared within microseconds of creating the display. When it succeeded, it took about 1.4 s. | Retry the arrangement for up to 2 s while the new display comes online. Before this, the fallback letterboxed the 2560×1440 physical display. |
| **"Connecting…" for ever when the Mac's display was asleep.** | The arrangement failed with `1001`, capture fell back to the sleeping physical display, and no frames came until ScreenCaptureKit gave up about 20 s later. | `IOPMAssertionDeclareUserActivity` wakes the displays before the session display is set up. A power assertion only *prevents* sleep; it doesn't wake a display that is already off. |
| **Rotation reconnected instead of resizing.** | No `MONITOR_LAYOUT` PDU ever arrived; the iOS client reconnects at the new size. | Reconnects were made fast: the dead 2 s wait for the display mode was removed. Live resize over MS-RDPEDISP is implemented, debounced and ordered, for clients that send it. |

---

## 6. Curtain mode: privacy, the Windows way

**Goal.** While you're remote, nobody at the desk can see or use the Mac.

**What failed:**
1. `CGSConfigureDisplayEnabled(false)` (private) turned the HDMI monitor off. Neither the end of
   the session nor quitting macrdp turned it back on: **only a reboot did**. Never power a
   display off through WindowServer.
2. `CGDisplayCapture` black shields from the helper failed with `rc=1001`, because a CLI process
   has no WindowServer app connection.
3. Black windows drawn by the app worked. But with the curtain making the session display main,
   the windows on the physical screen were missing from the session.

**What works now:**
- **DDC/CI monitor standby.** VCP `0xD6 = 5` sleeps the monitor and `0xD6 = 1` wakes it. It goes
  through the private `IOAVService` I2C calls, as MonitorControl and m1ddc do. macOS still sees
  the display, so nothing can get stuck.
  - Live finding: `0xD6 = 4` was ignored and `0xD6 = 5` worked. A write that returns success
    does not prove the monitor obeyed, so the app has a **"Test monitor standby"** button and
    stores the answer.
- **When standby works:** mirror (the phone sees everything), monitors in standby, local input
  blocked by a `CGEventTap`. MacGate marks its own injected events with
  `kCGEventSourceUserData`, so they pass the blocker.
  - Optional keep-alive: re-send standby every 5 s.
  - A marker file wakes the monitors on the next start after a crash.
- **When it doesn't:** the session display becomes main, the app covers the physical displays
  with black windows, and the windows move to the session display through the Accessibility
  API.

---

## 7. Locked Mac and remote unlock

Through MacGate's helper, which holds Accessibility and Screen Recording, the client sees the
macOS lock screen and can type into it. That was verified live, so no root helper is needed.

- **Optional `auto_unlock`:** the password from NLA is held in a `Zeroizing` buffer for at most
  10 s. It is typed into the lock screen only if the screen is locked, then wiped. It is never
  logged or persisted.
- `lock_on_disconnect` uses the private `SACLockScreenImmediate`.

---

## 8. Signing, TCC and the Keychain

- **Ad-hoc signatures change with every build,** so macOS re-asked for Screen Recording and
  Accessibility, and Keychain "Always Allow" never stuck. A self-signed code-signing identity
  (`macrdp-dev`) gives a stable designated requirement: `certificate leaf = H"…"`.
- **The Swift runtime was loaded twice.** `build.rs` looked for `libswift_Concurrency.dylib` on
  disk. On macOS 12+ it lives only in the dyld shared cache, so the build rpath'd Xcode's
  swift-5.5 copy as well. The fix is to rpath `/usr/lib/swift` only.

---

## 9. What is still missing

- Rotation with the iOS client still reconnects. The client never uses Display Control with
  this server, and the reason is not known yet.
- Only tested with the iOS Windows App, on one Mac and one monitor, over the LAN. mstsc,
  Android, the macOS client, WAN and cellular are untested.
- Private APIs (`CGVirtualDisplay`, `IOAVService`, `SACLockScreenImmediate`) can break with
  macOS updates, and they rule out the Mac App Store.
