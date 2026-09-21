Click-to-Photon Latency Rig — Build Specification

Target hardware: LG V50 ThinQ (SM8150 / Snapdragon 855, Android 11) as the optical+acoustic sensor, Windows PC as the device under test and emitter.

Goal: measure end-to-end click-to-photon latency to ±1–2 ms, and decompose it into OS input, application, presentation, and display stages — with no purpose-built hardware.

1. What is actually being measured
The chain, with the clock available at each boundary:

Stage	Boundary	Timebase	Measurable by
Switch actuation → HID report → OS	t_input	QPC	PC agent (WM_INPUT)
App reacts, renders, calls Present	t_present	QPC	PC agent
Present → scanout (vsync)	t_vsync	QPC	PC agent (DXGI_FRAME_STATISTICS.SyncQPCTime)
Scanout → photons (panel processing + response)	t_photon	phone clock	phone only

The PC can measure the first three stages by itself, to sub-millisecond precision. The phone exists solely to observe t_photon — but reporting t_photon − t_input requires a clock bridge between the phone and the PC.

Design note: the screen cannot be its own clock bridge
It is tempting to encode a timecode on the monitor and let the phone read it to sync clocks. This does not work for the total measurement. The timecode reaches the phone through the same panel, delayed by the same unknown display lag D; solving for the phone↔PC offset from it silently absorbs D, and D then cancels out of the flash measurement. You end up measuring everything except display lag — the one thing only the phone can see.

The timecode is still worth rendering (§3.4), but as a frame-identification and cross-check channel, not as the clock bridge.

The clock bridge must be a path that does not traverse the display. Two are available, in order of preference:

USB (primary) — NTP-style handshake over an adb reverse TCP socket. Min-RTT over USB 3.1 is ~0.2–0.8 ms, so the offset resolves to ~±0.4 ms. No acoustics, no environmental dependence.
Microphone (fallback) — the physical mouse click is a real acoustic event observed by the phone. With AudioRecord.getTimestamp() the HAL capture time is known, so mic input latency is accounted for rather than estimated. Residual error ~±2 ms plus acoustic propagation (2.9 ms/m, correct for it by measuring the mic-to-mouse distance). Use this when no agent can be installed on the device under test (console, locked-down machine).

2. System architecture
PC (device under test)                 LG V50
┌────────────────────────────┐        ┌──────────────────────────────┐
│ raw input → QPC t_input    │        │ Camera2, manual exposure     │
│ D3D11 flip swapchain       │        │ ImageReader YUV 720p60       │
│  ├ flash rect (white)      │ ──────▶│  ├ flash ROI luminance       │
│  └ frame-ID barcode strip  │ photons│  └ barcode ROI decode        │
│ DXGI frame stats → t_vsync │        │ SENSOR_TIMESTAMP + skew      │
│                            │        │   → sub-frame photon time    │
│ TCP event/sync server ◀────┼── USB ─┼─ clock sync + event pull     │
└────────────────────────────┘ adb    └──────────────────────────────┘

Both sides log raw events; all analysis happens on the phone (or offline from the two logs), never in the capture loop.

3. PC side — Windows emitter agent
Suggested stack: Rust + windows crate (windows-rs), D3D11, single binary, no installer.

3.1 Input capture
RegisterRawInputDevices with usage page 0x01, usage 0x02 (mouse), flag RIDEV_INPUTSINK so it works unfocused.
On WM_INPUT, call QueryPerformanceCounter as the first statement in the handler, before parsing the packet. Message-queue delivery costs ~0.1–0.5 ms; that is the agent's input-side noise floor and should be stated in the report rather than hidden.
Record button-down only. Debounce at 150 ms.
Optional stretch goal: read the mouse's HID interrupt endpoint directly via WinUSB to remove the message-queue hop. Only worth it if you later want to characterise the input stack itself.

3.2 Presentation path
Use the lowest-latency present path available so the emitter contributes as little as possible:

D3D11 device, IDXGISwapChain3, DXGI_SWAP_EFFECT_FLIP_DISCARD, BufferCount = 2.
DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT; SetMaximumFrameLatency(1); block on the waitable handle at the top of each frame.
Exclusive fullscreen (SetFullscreenState(TRUE)) or fullscreen-borderless — verify independent flip is engaged (PresentMon reports Hardware: Independent Flip). If DWM composition creeps in, the measurement gains a frame and the result is meaningless.
Offer a tearing mode (DXGI_PRESENT_ALLOW_TEARING + SyncInterval = 0) as a second run configuration. Comparing the two isolates the vsync quantisation component.
timeBeginPeriod(1), process priority HIGH_PRIORITY_CLASS, render thread registered with MMCSS (AvSetMmThreadCharacteristics("Games")).

3.3 Frame accounting
Per present, record:

t_present (QPC, immediately before Present)
PresentCount returned by GetFrameStatistics
SyncQPCTime and PresentRefreshCount for that present

DXGI_FRAME_STATISTICS is known to be quirky under composition; validate against ETW/PresentMon during bring-up and fall back to PresentMon's provider if the numbers disagree. This yields t_vsync for the frame in which the flash first appeared.

3.4 Render content
Two regions on a black background:

Flash rect — large (≥ 40% of screen area), pure white, drawn starting with the first frame presented after a click, held for 4 refreshes, then black. Place it where the camera will see it cleanly, away from screen edges.
Barcode strip — a horizontal band (e.g. full width × 64 px) at a fixed vertical position, split into 16 cells. Each cell is black or white, encoding the present index mod 2¹⁶ in Gray code (one cell changes per frame, so a cell caught mid-transition costs at most 1 LSB). Render it every frame, unconditionally, whether or not a flash is active.

The agent logs present_index → (t_present, t_vsync) so the phone can resolve any observed barcode value to a PC-side timestamp.

3.5 Sync and transport
TCP listener on 127.0.0.1:<port>; the phone reaches it via adb reverse tcp:<port> tcp:<port>. No Wi-Fi, no discovery, no firewall prompts.
Clock sync: phone sends T1 (its clock), PC stamps T2 and T3 (QPC), phone stamps T4 on receipt. offset ≈ ((T2 − T1) + (T3 − T4)) / 2, rtt = (T4 − T1) − (T3 − T2).
Take ~200 samples spread across the session, keep the lowest-RTT decile, and fit offset(t) = a + b·t by least squares. The b term matters: QPC and the phone's CLOCK_BOOTTIME run off different crystals, and 20 ppm over a 60 s run is 1.2 ms of drift.
Stream click and present events to the phone as they happen; also expose a bulk pull at end-of-run so nothing depends on live delivery.

4. Android side — LG V50 sensor app
Suggested stack: Kotlin, minSdk 29, Camera2 directly (CameraX abstracts away exactly the controls you need; if you prefer its lifecycle handling, use Camera2Interop for everything in §4.2).

4.1 Camera selection
Enumerate CameraCharacteristics and pick the main 12 MP wide (27 mm, f/1.5, 1/2.55", dual-pixel PDAF, OIS). It has by far the largest sensor and fastest lens of the three rear cameras — decisive when you are shooting a dark room at 500 µs exposure. Identify it by LENS_INFO_AVAILABLE_FOCAL_LENGTHS, not by index.

Verify at startup and fail loudly if absent:

INFO_SUPPORTED_HARDWARE_LEVEL ≥ FULL
REQUEST_AVAILABLE_CAPABILITIES contains MANUAL_SENSOR and READ_SENSOR_SETTINGS
SENSOR_INFO_TIMESTAMP_SOURCE — branch on the result (§4.4)

4.2 Capture configuration
Everything automatic must be off; auto-exposure drift is what makes the browser version unreliable.

CONTROL_MODE                      = OFF
CONTROL_AE_MODE                   = OFF
CONTROL_AF_MODE                   = OFF
CONTROL_AWB_MODE                  = OFF
LENS_FOCUS_DISTANCE               = <manual, set once on a focus target>
LENS_OPTICAL_STABILIZATION_MODE   = OFF        // critical — see below
CONTROL_VIDEO_STABILIZATION_MODE  = OFF
SENSOR_EXPOSURE_TIME              = 500_000 ns (1/2000 s)
SENSOR_SENSITIVITY                = 1600–3200
SENSOR_FRAME_DURATION             = 16_666_666 ns (60 fps)
NOISE_REDUCTION_MODE              = OFF
EDGE_MODE                         = OFF

OIS must be off. Optical stabilisation shifts the image during readout, which corrupts the row→time mapping that the whole sub-frame method rests on. Same for EIS. Mount the phone on a tripod or clamp instead.

Short exposure is what makes the rolling-shutter boundary a sharp line rather than a gradient. 500 µs is a good starting point; the monitor should be at maximum brightness in a dark room. If the flash region clips to white while the barcode is still readable, you are in the right place.

4.3 Output surface
ImageReader with YUV_420_888, 1280×720, maxImages = 4.
Read the Y plane only — no RGB conversion, no bitmap allocation. Two ROIs: the flash rect and the barcode strip. At 720p a couple of narrow ROIs is a few hundred microseconds of work per frame on an 855, so 60 fps analysis is comfortable on a background HandlerThread.
Do not pursue a 240 fps constrained high-speed session. CameraConstrainedHighSpeedCaptureSession only accepts preview and MediaCodec/MediaRecorder surfaces — ImageReader YUV is not a legal target — so you would be recording to a file and decoding offline for no accuracy gain. The rolling-shutter method below already beats 240 fps by an order of magnitude.

4.4 Timebase alignment
Read SENSOR_INFO_TIMESTAMP_SOURCE once:

UNKNOWN → SENSOR_TIMESTAMP is CLOCK_MONOTONIC, comparable with System.nanoTime().
REALTIME → it is CLOCK_BOOTTIME, comparable with SystemClock.elapsedRealtimeNanos(). Common on Qualcomm HALs; expect this on the V50.

Pick one timebase for the whole app and convert at the edges. If you use the mic fallback, request AudioRecord.getTimestamp(ts, AudioTimestamp.TIMEBASE_BOOTTIME) so both sensors land in the same clock without conversion. The USB sync handshake should use the same clock.

4.5 Sub-frame photon timing (the core method)
Per TotalCaptureResult:

SENSOR_TIMESTAMP → t0, start of exposure of the first row.
SENSOR_ROLLING_SHUTTER_SKEW → S, the time between first-row and last-row exposure start.
For a feature whose onset appears at row r of H:

t_feature = t0 + S · r / (H − 1)        (± exposure_time / 2)

With H = 720 and a typical S of 15–25 ms, one row is 21–35 µs. The camera's 60 fps frame rate is irrelevant to precision — it only bounds how often you can sample. This is what makes the phone accurate: a 60 fps sensor used as a 30 kHz line-scan timer.

Procedure per flash:

Find the first frame where the flash ROI's mean luminance rises past threshold.
Within that frame, scan down the flash ROI and find the row r_f of the rising boundary (the flash appears part-way through readout, so the ROI is dark above the boundary and bright below, or vice versa depending on readout direction — determine direction empirically during calibration).
t_photon = t0 + S · r_f / (H − 1).
In the same frame, decode the barcode above and below the boundary to identify which monitor refresh carried the flash. Cross-check against the PC's present_index log.
latency = (t_photon + offset_phone→QPC) − t_input.
Also report the decomposition: t_present − t_input, t_vsync − t_present, t_photon − t_vsync (display lag + panel response).

If SENSOR_ROLLING_SHUTTER_SKEW is unavailable or looks wrong, calibrate S from the barcode itself: the barcode changes value once per monitor refresh, so the row spacing between successive barcode transitions within a single captured frame equals refresh_period · (H − 1) / S. Solve for S. This is a good self-check even when the HAL does report it.

4.6 Microphone fallback mode
Only used when no PC agent is running.

AudioRecord, MediaRecorder.AudioSource.UNPROCESSED (check PROPERTY_SUPPORT_AUDIO_SOURCE_UNPROCESSED; fall back to VOICE_RECOGNITION, never MIC or CAMCORDER — those apply processing), 48 kHz mono, 16-bit.
Buffer size from getMinBufferSize, request the low-latency path via AudioManager.PROPERTY_OUTPUT_FRAMES_PER_BUFFER-aligned reads. Consider AAudio in PERFORMANCE_MODE_LOW_LATENCY if AudioRecord latency is poor.
Onset detection: high-pass at ~2 kHz (mouse clicks are broadband transients), then an envelope follower with fast attack and an adaptive threshold at median + k·MAD of the recent noise floor — not a fixed constant.
Convert the onset's frame position to a timestamp via AudioRecord.getTimestamp(): t_onset = ts.nanoTime + (framePosition_onset − ts.framePosition) / sampleRate.
Subtract acoustic propagation: prompt for the mic-to-mouse distance, subtract d / 343 m·s⁻¹.

4.7 UI
Minimal. Live preview with the two ROIs drawn as overlays, a luminance/contrast readout so the user can frame and expose correctly, a run button, and a results view showing median, IQR, n, the stage decomposition, and the per-stage error bars. Export runs as JSON/CSV to the app-specific external dir.

5. Statistics
Report median and IQR, not mean and stdev — the distribution is right-skewed by scheduler outliers.
Report standard error of the median as 1.253 · σ̂ / √n with σ̂ = 1.4826 · MAD, separately from the resolution term. Don't fuse spread and resolution into one number.
There is no half-frame quantisation bias to correct in this design — the rolling-shutter method has no frame-rate-dependent bias. If you ever fall back to whole-frame detection, subtract frame_interval / 2.
Collect ≥ 30 pairs per run. Discard the first 3 (thermal/cache warmup).
Vary click timing randomly relative to vsync. If clicks are periodic they can phase-lock to the refresh and understate or overstate the vsync-wait component.

6. Error budget (full USB mode)
Source	Magnitude
Rolling-shutter row quantisation	±0.02 ms
Exposure window (500 µs)	±0.25 ms
SENSOR_TIMESTAMP HAL accuracy	±0.5 ms
USB clock offset (min-RTT filtered, drift-corrected)	±0.4 ms
WM_INPUT queue delivery	±0.3 ms
Switch actuation → HID report	±0.5 ms, uncorrected
Total (RSS)	≈ ±0.9 ms

Mic-fallback mode adds ~±2 ms from AudioRecord timestamp accuracy and ~±0.3 ms from distance estimation, giving roughly ±2.5 ms.

7. Validation
Do these before trusting any number:

Adjacency null test — render the barcode strip and a second flash rect directly adjacent. Their measured onset difference must equal their vertical separation divided by the monitor's scanout rate. If it doesn't, the row→time mapping is wrong.
Refresh-rate sweep — measure at 60/120/144 Hz. Latency should drop by roughly the expected fraction of a refresh period. A flat result means something upstream is dominating (likely composition).
Tearing vs vsync — ALLOW_TEARING should cut roughly half a refresh period off the median.
Sync-channel agreement — run USB and mic modes simultaneously on the same clicks. They should agree within their combined error bars. This is the single best end-to-end check.
Drift check — plot the fitted clock offset across a 5-minute run; a clean straight line means the model is right, a curve means you need a better fit or more frequent resync.

8. Build order
PC agent: window, D3D11 flip swapchain, raw input, barcode + flash rendering, QPC logging to a file. Verify independent flip with PresentMon.
Android: Camera2 opened on the wide lens with full manual control, live preview, Y-plane ROI luminance readout. Confirm auto-exposure is genuinely off (luminance must not drift when the flash fires).
Rolling-shutter boundary detection on a static test pattern; self-calibrate S from the barcode and compare to SENSOR_ROLLING_SHUTTER_SKEW.
adb reverse transport + clock sync handshake; validate drift over 5 minutes.
End-to-end single measurement, then batching and statistics.
Mic fallback path.
Validation suite from §7.

9. LG V50 specifics and pitfalls
OIS/EIS off — stated above, repeated because it is the failure mode that silently corrupts results rather than obviously breaking them.
Lens choice — the ultrawide has no autofocus and a much smaller sensor; the telephoto is f/2.4. Use the main wide.
240 fps is an LG camera-app feature, not necessarily exposed through Camera2 high-speed sessions. Check getHighSpeedVideoFpsRangesFor out of curiosity, but the design does not need it.
Thermal throttling — SD855 ramps clocks down under sustained load. Call Window.setSustainedPerformanceMode(true) and keep runs under ~2 minutes.
Phone screen glare — the V50's P-OLED will reflect off the monitor in a dark room. Drop phone brightness to minimum during capture and keep the screen out of the monitor's line of sight.
Android 11 storage — write logs to getExternalFilesDir(); no storage permission needed, and adb pull still reaches it.
USB 3.1 Type-C — use a good cable. A flaky USB link shows up as RTT outliers, which the min-RTT filter mostly handles, but a link that renegotiates mid-run will inject a clock-offset step.

10. Scope note
This measures the full chain for the emitter you point it at. A browser page is not a game: Chrome aligns input dispatch to requestAnimationFrame, adding one to two frames that a native D3D application does not pay. Keep the native emitter as the reference and treat any browser-based emitter as an upper bound, labelled as such.
