# Latency Sensor — Android app

Optical sensor for the click-to-photon rig (see `../SPEC.md` §4). Runs on the
LG V50 (Android 11, minSdk 29), opens the **main wide rear camera** with every
automatic control disabled, and reports ROI luminance + barcode state per frame.

Milestone status (SPEC.md §8):

- **M2 (this app):** camera open with full manual control, live preview,
  Y-plane ROI luminance readout, AE-off confirmation. ✅
- **M3 (next):** rolling-shutter boundary detection, self-calibrated skew.
- **M4:** adb-reverse clock sync. **M5:** end-to-end measurement. **M6:** mic fallback.

## Build

Needs JDK 17 + Android SDK (platform 34, build-tools). Either open the folder in
Android Studio (**File → Open → `sensor/`**) or build from the command line:

```bash
cd sensor
./gradlew assembleDebug
```

The resulting APK lands in `sensor/app/build/outputs/apk/debug/app-debug.apk`.

### Toolchain used for the verified build

A portable, admin-free toolchain lives in `~/latency-tools` (outside the repo):
Temurin JDK 17, Android command-line tools + platform 34/build-tools 34.0.0,
and Gradle 8.7. `sensor/local.properties` points at that SDK and is gitignored,
so set your own `sdk.dir` if you build with Android Studio's SDK instead.

```bash
export JAVA_HOME="$HOME/latency-tools/jdk-17.0.20.1+1"
export PATH="$JAVA_HOME/bin:$PATH"
cd sensor && ./gradlew assembleDebug
```

Note: `sdk.dir` in `local.properties` must use forward slashes on Windows; a
backslash-escaped path makes Gradle fail with a "filename or volume label
syntax is incorrect" error.

## Install and run

```bash
adb install -r sensor/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n com.latencytest.sensor/.MainActivity
```

1. Point the phone's **main wide camera** at the monitor, screen-side toward the
   PC but angled so the phone's own screen doesn't reflect off the monitor
   (spec §9: keep brightness low and the display out of the line of sight).
2. Frame the monitor so the PC's **white flash fills the amber box** and the
   **barcode strip sits inside the top strip**. Both overlays turn green when
   they see content.
3. Tap **Start**, then **Focus lock** (aim at the monitor first).
4. Click the mouse on the PC; `flash` in the readout should jump on each click.
5. The capability report at the bottom lists every rear camera found, the lens
   chosen, and any §4.1 problems.

## Verifying auto-exposure is really off (M2 acceptance check)

With AE disabled the **dark baseline** (mean luminance while no flash is
present) should stay flat: the readout should settle on `STABLE (AE off ✓)`
with drift under ~3%. If you see `DRIFTING`, AE is still active — the numbers
would then be contaminated by exposure ramping after every flash.

## Frame log

Every analysed frame is appended to the app-specific external dir
(no storage permission needed, reachable via adb):

```
/sdcard/Android/data/com.latencytest.sensor/files/frames.csv
```

```bash
adb pull /sdcard/Android/data/com.latencytest.sensor/files/frames.csv
```

Columns: `ts_ns, frame_number, exposure_ns, iso, flash_mean, flash_min,
flash_max, barcode_index, skew_ns, ae_state`.

## Implementation notes

- **No AndroidX.** Camera2 is used directly and the UI is a plain `Activity`
  with a `SurfaceView`, so the dependency set is just the Kotlin stdlib. This
  keeps the build fast and avoids abstractions that hide the controls that
  matter (CameraX would obscure everything in §4.2).
- **Y plane only** (§4.3): no RGB conversion, no bitmap allocations.
- **`acquireNextImage`, not `acquireLatestImage`**: silently dropping frames
  would mean missing the frame the flash onset lands in.
- **Stabilisation off** (§9): OIS/EIS shift the image during readout, which
  corrupts the row→time mapping the sub-frame method depends on. Use a tripod;
  do not hand-hold.
- **`SENSOR_INFO_TIMESTAMP_SOURCE` is reported** in the capability list. On the
  V50 expect `REALTIME` (CLOCK_BOOTTIME); the app picks its timebase from this
  (§4.4) so timestamps and `AudioRecord.getTimestamp` land in one clock.
