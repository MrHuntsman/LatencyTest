package com.latencytest.sensor

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import kotlin.math.abs
import kotlin.math.sqrt

/**
 * Minimal sensor UI (SPEC.md §4.7): live preview with the two ROIs drawn as
 * overlays, a luminance/contrast readout so the user can frame and expose
 * correctly, a run button, and a status panel.
 *
 * Milestone 2 (§8) also requires proving auto-exposure is genuinely off: the
 * AE-drift readout below tracks the dark-baseline luminance while no flash is
 * active. With AE off it stays flat; with AE on it ramps after every flash.
 */
class MainActivity : Activity(), SurfaceHolder.Callback {

    private lateinit var preview: SurfaceView
    private lateinit var overlay: RoiOverlayView
    private lateinit var statusView: TextView
    private lateinit var readoutView: TextView
    private lateinit var reportView: TextView
    private lateinit var startButton: Button
    private lateinit var focusButton: Button
    private lateinit var exposureButton: Button

    private var sensor: CameraSensor? = null
    private val ui = Handler(Looper.getMainLooper())

    private var previewReady = false
    private var pendingStart = false

    // AE-drift check: baseline luminance while the flash is off
    private val darkSamples = ArrayList<Float>()
    private var lastFlashMean = 0f
    private var lastUpdateMs = 0L
    private var exposureNs = CameraSelection.DEFAULT_EXPOSURE_NS
    private var iso = CameraSelection.DEFAULT_SENSITIVITY

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.parseColor("#0b0e14"))
            setPadding(24, 24, 24, 24)
        }

        val title = TextView(this).apply {
            text = "📱 Latency Sensor"
            textSize = 20f
            setTextColor(Color.parseColor("#e6e9f0"))
        }

        preview = SurfaceView(this)
        overlay = RoiOverlayView(this)
        preview.holder.addCallback(this)

        // Preview + overlay stacked in a 16:9 box
        val previewBox = android.widget.FrameLayout(this)
        val matchParent = android.widget.FrameLayout.LayoutParams(
            android.view.ViewGroup.LayoutParams.MATCH_PARENT,
            android.view.ViewGroup.LayoutParams.MATCH_PARENT,
        )
        previewBox.addView(preview, matchParent)
        previewBox.addView(overlay, android.widget.FrameLayout.LayoutParams(matchParent))

        statusView = TextView(this).apply {
            text = "Requesting camera permission…"
            textSize = 13f
            setTextColor(Color.parseColor("#8b93a7"))
            setPadding(0, 16, 0, 8)
        }
        readoutView = TextView(this).apply {
            textSize = 13f
            setTextColor(Color.parseColor("#e6e9f0"))
            typeface = android.graphics.Typeface.MONOSPACE
        }
        reportView = TextView(this).apply {
            textSize = 11f
            setTextColor(Color.parseColor("#8b93a7"))
        }

        startButton = Button(this).apply {
            text = "Start"
            setOnClickListener { toggleStart() }
        }
        focusButton = Button(this).apply {
            text = "Focus lock"
            isEnabled = false
            setOnClickListener { sensor?.focusLock { } }
        }
        exposureButton = Button(this).apply {
            text = "Cycle exposure"
            isEnabled = false
            setOnClickListener { cycleExposure() }
        }

        val buttons = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(startButton)
            addView(focusButton)
            addView(exposureButton)
        }

        root.addView(title)
        root.addView(
            previewBox,
            LinearLayout.LayoutParams(-1, 0, 1f).apply { topMargin = 16; bottomMargin = 16 },
        )
        root.addView(buttons)
        root.addView(statusView)
        root.addView(readoutView)
        root.addView(
            ScrollView(this).apply {
                addView(reportView)
                layoutParams = LinearLayout.LayoutParams(-1, 0, 0.6f)
            }
        )
        setContentView(root)

        reportView.text = CameraSelection.report(this)

        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            onPermissionReady()
        } else {
            requestPermissions(arrayOf(Manifest.permission.CAMERA), 1)
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) {
            onPermissionReady()
        } else {
            statusView.text = "Camera permission denied — the sensor cannot run."
        }
    }

    private fun onPermissionReady() {
        statusView.text = "Ready. Frame the monitor inside the dashed box, then Start."
    }

    private var isRunning = false

    private fun toggleStart() {
        if (isRunning) {
            sensor?.stop()
            sensor = null
            isRunning = false
            darkSamples.clear()
            startButton.text = "Start"
            focusButton.isEnabled = false
            exposureButton.isEnabled = false
            statusView.text = "Stopped."
            return
        }
        if (!previewReady) {
            pendingStart = true
            statusView.text = "Waiting for the preview surface…"
            return
        }
        startSensor()
    }

    private fun startSensor() {
        val pick = CameraSelection.pickMainWide(this)
        if (pick == null) {
            statusView.text = "No suitable rear camera found."
            return
        }
        val sensor = CameraSensor(
            context = this,
            cameraId = pick.id,
            onStatus = { msg -> ui.post { statusView.text = msg } },
            onFrame = { sample -> onFrame(sample) },
        )
        this.sensor = sensor
        sensor.setExposureAndSensitivity(exposureNs, iso)
        sensor.start(preview.holder.surface)

        isRunning = true
        startButton.text = "Stop"
        focusButton.isEnabled = true
        exposureButton.isEnabled = true
        statusView.text = "Capturing on camera ${pick.id} (main wide)."
    }

    private fun cycleExposure() {
        val options = longArrayOf(100_000L, 250_000L, 500_000L, 1_000_000L)
        val idx = (options.indexOf(exposureNs) + 1) % options.size
        exposureNs = options[idx]
        sensor?.setExposureAndSensitivity(exposureNs, iso)
    }

    // ------------------------------------------------------------ frame handling

    private fun onFrame(sample: CameraSensor.FrameSample) {
        val now = System.currentTimeMillis()
        val flashOn = sample.flashMean > 60f
        lastFlashMean = sample.flashMean

        // AE-drift check: only sample the dark baseline (flash off) so the flash
        // itself does not pollute the statistic.
        if (!flashOn) {
            darkSamples.add(sample.flashMean)
            if (darkSamples.size > 240) darkSamples.removeAt(0) // ~4 s at 60 fps
        }

        overlay.post {
            overlay.flashMean = sample.flashMean
            overlay.setBarcode(sample.barcodeCells)
            overlay.barcodeLocked = sample.barcodeIndex != 0
        }

        if (now - lastUpdateMs < 200) return
        lastUpdateMs = now

        val s = sensor ?: return
        val (mean, sd) = meanAndSd(darkSamples)
        val driftPct = if (mean > 1f) sd / mean * 100.0 else 0.0
        val driftVerdict = when {
            darkSamples.size < 30 -> "collecting…"
            driftPct < 3.0 -> "STABLE (AE off ✓)"
            driftPct < 8.0 -> "noisy"
            else -> "DRIFTING (AE may be on!)"
        }

        val exposureOk = (sample.exposureNs ?: -1L) in 1..2_000_000L
        val sensitivityOk = (sample.sensitivity ?: -1) in 1..12_800

        ui.post {
            readoutView.text = buildString {
                append("frame  ${sample.frameNumber}\n")
                append("fps    %.1f\n".format(s.measuredFps))
                append("flash  %.1f  (min %d / max %d)\n".format(sample.flashMean, sample.flashMin, sample.flashMax))
                append("barcode idx ${sample.barcodeIndex}  cells ${sample.barcodeCells.joinToString("")}\n")
                append("exposure ${sample.exposureNs ?: -1} ns   iso ${sample.sensitivity ?: -1}\n")
                append("skew   ${sample.rollingShutterSkewNs ?: -1} ns\n")
                append("ts     ${sample.sensorTimestampNs}\n")
                append("dark baseline mean %.1f  sd %.1f  drift %.1f%%\n".format(mean, sd, driftPct))
                append("AE state ${sample.aeState ?: -1}   verdict: $driftVerdict\n")
                append(
                    "exposure/iso applied: " +
                        (if (exposureOk) "yes" else "NO (was the request accepted?)") + " / " +
                        (if (sensitivityOk) "yes" else "NO")
                )
            }
        }
    }

    private fun meanAndSd(values: List<Float>): Pair<Float, Float> {
        if (values.isEmpty()) return 0f to 0f
        val mean = values.sum() / values.size
        var acc = 0.0
        for (v in values) {
            val d = (v - mean).toDouble()
            acc += d * d
        }
        return mean to sqrt(acc / values.size).toFloat()
    }

    // ------------------------------------------------------------ SurfaceHolder

    override fun surfaceCreated(holder: SurfaceHolder) {
        previewReady = true
        if (pendingStart) {
            pendingStart = false
            startSensor()
        }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        // Restart the session if the surface size changes while running.
        if (isRunning && sensor != null) {
            statusView.text = "Preview resized — restart to reconfigure the session."
        }
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        previewReady = false
    }

    override fun onDestroy() {
        sensor?.stop()
        sensor = null
        super.onDestroy()
    }
}
