package com.latencytest.sensor

import android.content.Context
import android.graphics.ImageFormat
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.CaptureResult
import android.hardware.camera2.TotalCaptureResult
import android.media.Image
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.util.Size
import android.view.Surface
import java.io.BufferedWriter
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Camera2 sensor (SPEC.md §4). Everything automatic is off: auto-exposure drift
 * is exactly what makes frame-based measurement unreliable.
 *
 * Capture settings per §4.2:
 *   CONTROL_MODE = OFF, AE/AF/AWB = OFF, OIS/EIS = OFF,
 *   SENSOR_EXPOSURE_TIME = 500 us, SENSOR_SENSITIVITY = 1600-3200,
 *   SENSOR_FRAME_DURATION = 16.67 ms, NOISE_REDUCTION/EDGE = OFF
 */
class CameraSensor(
    private val context: Context,
    private val cameraId: String,
    private val onStatus: (String) -> Unit,
    private val onFrame: (FrameSample) -> Unit,
) {

    data class FrameSample(
        val sensorTimestampNs: Long,
        val frameNumber: Long,
        val exposureNs: Long?,
        val sensitivity: Int?,
        val flashMean: Float,
        val flashMin: Int,
        val flashMax: Int,
        val barcodeCells: IntArray,
        val barcodeIndex: Int,
        val rollingShutterSkewNs: Long?,
        val aeState: Int?,
    )

    private val cameraManager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
    private val characteristics: CameraCharacteristics = cameraManager.getCameraCharacteristics(cameraId)

    private var device: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var reader: ImageReader? = null
    private var requestBuilder: CaptureRequest.Builder? = null
    private var previewSurface: Surface? = null

    private lateinit var cameraThread: HandlerThread
    private lateinit var cameraHandler: Handler
    private lateinit var analysisThread: HandlerThread
    private lateinit var analysisHandler: Handler

    private val running = AtomicBoolean(false)

    /** Latest metadata per sensor timestamp, filled by the capture callback. */
    private val metadata = HashMap<Long, TotalCaptureResult>()
    private val metadataLock = Any()

    var exposureNs: Long = CameraSelection.DEFAULT_EXPOSURE_NS
    var sensitivity: Int = CameraSelection.DEFAULT_SENSITIVITY
    var captureSize: Size = Size(1280, 720)

    /**
     * Rolling-shutter skew, in nanoseconds, from the latest frame's metadata.
     *
     * SENSOR_ROLLING_SHUTTER_SKEW is a CaptureResult key (per-frame), not a
     * CameraCharacteristics key, so it is read per frame rather than once.
     * If the HAL omits it, §4.5 says to self-calibrate S from the barcode
     * spacing instead - that path is milestone 3.
     */
    @Volatile var rollingShutterSkewNs: Long? = null

    // Frame health counters surfaced in the UI. @Volatile: written on the
    // analysis thread, read from the UI thread.
    @Volatile var framesSeen: Long = 0; private set
    @Volatile var framesAnalyzed: Long = 0; private set
    @Volatile var framesDropped: Long = 0; private set
    @Volatile var lastFrameTimestampNs: Long = 0; private set
    @Volatile var measuredFps: Double = 0.0; private set

    private var csv: BufferedWriter? = null
    var loggingEnabled: Boolean = true
    var logFile: File? = null; private set

    private var lastFpsStamp = System.nanoTime()
    private var analysisFrameIndex: Long = 0

    fun start(previewSurface: Surface) {
        this.previewSurface = previewSurface
        cameraThread = HandlerThread("sensor-camera").also { it.start() }
        cameraHandler = Handler(cameraThread.looper)
        analysisThread = HandlerThread("sensor-analysis").also { it.start() }
        analysisHandler = Handler(analysisThread.looper)

        if (loggingEnabled) openLog()

        reader = ImageReader.newInstance(captureSize.width, captureSize.height, ImageFormat.YUV_420_888, 4)
        // acquireNextImage (not acquireLatestImage): dropping frames silently
        // would mean missing the very frame the flash onset lands in.
        reader!!.setOnImageAvailableListener({ r -> onImageAvailable(r.acquireNextImage()) }, analysisHandler)

        @Suppress("MissingPermission")
        cameraManager.openCamera(cameraId, object : CameraDevice.StateCallback() {
            override fun onOpened(camera: CameraDevice) {
                device = camera
                createSession(previewSurface)
            }

            override fun onDisconnected(camera: CameraDevice) {
                camera.close()
                device = null
                onStatus("camera disconnected")
            }

            override fun onError(camera: CameraDevice, error: Int) {
                camera.close()
                device = null
                onStatus("camera error $error")
            }
        }, cameraHandler)
    }

    private fun createSession(previewSurface: Surface) {
        val camera = device ?: return
        val surfaces = listOf(previewSurface, reader!!.surface)
        @Suppress("DEPRECATION")
        camera.createCaptureSession(surfaces, object : CameraCaptureSession.StateCallback() {
            override fun onConfigured(s: CameraCaptureSession) {
                session = s
                startRepeating()
            }

            override fun onConfigureFailed(s: CameraCaptureSession) {
                onStatus("capture session configuration failed")
            }
        }, cameraHandler)
    }

    /**
     * Fully manual request (§4.2). AE/AF/AWB off, OIS and EIS off, manual
     * exposure/sensitivity, fixed frame duration, NR/EDGE off.
     */
    private fun buildManualRequest(): CaptureRequest.Builder {
        val camera = device!!
        val b = camera.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW)
        b.addTarget(reader!!.surface)
        previewSurface?.let { b.addTarget(it) }

        b.set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_OFF)
        b.set(CaptureRequest.CONTROL_AE_MODE, CameraMetadata.CONTROL_AE_MODE_OFF)
        b.set(CaptureRequest.CONTROL_AF_MODE, CameraMetadata.CONTROL_AF_MODE_OFF)
        b.set(CaptureRequest.CONTROL_AWB_MODE, CameraMetadata.CONTROL_AWB_MODE_OFF)
        b.set(CaptureRequest.CONTROL_AE_LOCK, true)
        b.set(CaptureRequest.CONTROL_AWB_LOCK, true)

        // Stabilisation must be off: OIS/EIS shift the image during readout,
        // which corrupts the row -> time mapping the sub-frame method relies on.
        b.set(CaptureRequest.LENS_OPTICAL_STABILIZATION_MODE, CameraMetadata.LENS_OPTICAL_STABILIZATION_MODE_OFF)
        b.set(CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE, CameraMetadata.CONTROL_VIDEO_STABILIZATION_MODE_OFF)

        b.set(CaptureRequest.SENSOR_EXPOSURE_TIME, exposureNs)
        b.set(CaptureRequest.SENSOR_SENSITIVITY, sensitivity)
        b.set(CaptureRequest.SENSOR_FRAME_DURATION, CameraSelection.FRAME_DURATION_60FPS_NS)

        // Not every device advertises OFF for these; setting an unsupported
        // value makes the whole request fail, so fall back to FAST.
        if (supportsNoiseReductionOff) {
            b.set(CaptureRequest.NOISE_REDUCTION_MODE, CameraMetadata.NOISE_REDUCTION_MODE_OFF)
        }
        if (supportsEdgeOff) {
            b.set(CaptureRequest.EDGE_MODE, CameraMetadata.EDGE_MODE_OFF)
        }

        manualFocusDistance?.let { b.set(CaptureRequest.LENS_FOCUS_DISTANCE, it) }
        return b
    }

    /** Set by [focusLock]; null means "do not touch focus distance". */
    var manualFocusDistance: Float? = null
        private set

    private val supportsNoiseReductionOff: Boolean by lazy {
        characteristics.get(CameraCharacteristics.NOISE_REDUCTION_AVAILABLE_NOISE_REDUCTION_MODES)
            ?.contains(CameraMetadata.NOISE_REDUCTION_MODE_OFF) ?: false
    }

    private val supportsEdgeOff: Boolean by lazy {
        characteristics.get(CameraCharacteristics.EDGE_AVAILABLE_EDGE_MODES)
            ?.contains(CameraMetadata.EDGE_MODE_OFF) ?: false
    }

    private fun startRepeating() {
        val b = buildManualRequest()
        requestBuilder = b
        session?.setRepeatingRequest(b.build(), captureCallback, cameraHandler)
        running.set(true)
        onStatus("capturing: ${captureSize.width}x${captureSize.height}, exposure ${exposureNs}ns, ISO $sensitivity")
    }

    private val captureCallback = object : CameraCaptureSession.CaptureCallback() {
        override fun onCaptureCompleted(
            s: CameraCaptureSession,
            request: CaptureRequest,
            result: TotalCaptureResult,
        ) {
            val ts = result.get(CaptureResult.SENSOR_TIMESTAMP) ?: return
            synchronized(metadataLock) {
                metadata[ts] = result
                if (metadata.size > 32) {
                    val oldest = metadata.keys.minOrNull()
                    if (oldest != null) metadata.remove(oldest)
                }
            }
        }
    }

    private fun onImageAvailable(image: Image?) {
        if (image == null) return
        framesSeen++
        try {
            if (!running.get()) { framesDropped++; return }

            val ts = image.timestamp
            val result = synchronized(metadataLock) { metadata.remove(ts) }
            if (result == null) framesDropped++
            result?.get(CaptureResult.SENSOR_ROLLING_SHUTTER_SKEW)?.let { rollingShutterSkewNs = it }

            val flash = YPlaneAnalyzer.roiStats(
                image, RoiSpec.FLASH_X0, RoiSpec.FLASH_Y0, RoiSpec.FLASH_X1, RoiSpec.FLASH_Y1
            )
            val cells = YPlaneAnalyzer.barcodeCells(image)
            val index = YPlaneAnalyzer.grayToBinary(cells)

            // SENSOR_FRAME_NUMBER is a hidden key, so the analysed-frame counter
            // is used for indexing; SENSOR_TIMESTAMP is the real frame identity
            // (it is what the PC-side log pairs against).
            val sample = FrameSample(
                sensorTimestampNs = ts,
                frameNumber = analysisFrameIndex++,
                exposureNs = result?.get(CaptureResult.SENSOR_EXPOSURE_TIME),
                sensitivity = result?.get(CaptureResult.SENSOR_SENSITIVITY),
                flashMean = flash.mean,
                flashMin = flash.min,
                flashMax = flash.max,
                barcodeCells = cells,
                barcodeIndex = index,
                rollingShutterSkewNs = rollingShutterSkewNs,
                aeState = result?.get(CaptureResult.CONTROL_AE_STATE),
            )

            framesAnalyzed++
            lastFrameTimestampNs = ts
            writeCsv(sample)
            onFrame(sample)

            val now = System.nanoTime()
            if (now - lastFpsStamp >= 1_000_000_000L) {
                measuredFps = framesAnalyzed.toDouble() / ((now - lastFpsStamp) / 1e9)
                framesAnalyzed = 0
                framesSeen = 0
                framesDropped = 0
                lastFpsStamp = now
            }
        } catch (t: Throwable) {
            onStatus("analysis error: ${t.message}")
        } finally {
            image.close()
        }
    }

    /**
     * Locks focus once, then holds the achieved lens distance with AF mode OFF
     * for the rest of the session (§4.2: focus is set once on a focus target).
     */
    fun focusLock(onDone: (Float?) -> Unit) {
        val camera = device ?: run { onDone(null); return }
        val b = camera.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW)
        reader?.surface?.let { b.addTarget(it) }
        previewSurface?.let { b.addTarget(it) }
        b.set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_AUTO)
        b.set(CaptureRequest.CONTROL_AF_MODE, CameraMetadata.CONTROL_AF_MODE_AUTO)
        b.set(CaptureRequest.CONTROL_AF_TRIGGER, CameraMetadata.CONTROL_AF_TRIGGER_START)
        b.set(CaptureRequest.CONTROL_AE_MODE, CameraMetadata.CONTROL_AE_MODE_OFF)
        b.set(CaptureRequest.SENSOR_EXPOSURE_TIME, exposureNs)
        b.set(CaptureRequest.SENSOR_SENSITIVITY, sensitivity)

        var done = false
        val cb = object : CameraCaptureSession.CaptureCallback() {
            override fun onCaptureCompleted(
                s: CameraCaptureSession,
                request: CaptureRequest,
                result: TotalCaptureResult,
            ) {
                val state = result.get(CaptureResult.CONTROL_AF_STATE)
                val distance = result.get(CaptureResult.LENS_FOCUS_DISTANCE)
                val focused = state == CameraMetadata.CONTROL_AF_STATE_FOCUSED_LOCKED ||
                    state == CameraMetadata.CONTROL_AF_STATE_PASSIVE_FOCUSED
                if (!done && focused && distance != null) {
                    done = true
                    manualFocusDistance = distance
                    onStatus("focus locked at %.2f dioptres".format(distance))
                    // Back to the fully manual request with the locked distance.
                    startRepeating()
                    onDone(distance)
                }
            }
        }
        session?.setRepeatingRequest(b.build(), cb, cameraHandler)
        cameraHandler.postDelayed({
            if (!done) {
                onStatus("focus lock timed out - keeping previous distance")
                startRepeating()
                onDone(null)
            }
        }, 3000)
    }

    fun setExposureAndSensitivity(exposure: Long, iso: Int) {
        exposureNs = exposure
        sensitivity = iso
        requestBuilder?.let {
            it.set(CaptureRequest.SENSOR_EXPOSURE_TIME, exposureNs)
            it.set(CaptureRequest.SENSOR_SENSITIVITY, sensitivity)
            session?.setRepeatingRequest(it.build(), captureCallback, cameraHandler)
            onStatus("exposure ${exposureNs}ns, ISO $sensitivity")
        }
    }

    fun stop() {
        running.set(false)
        try {
            session?.stopRepeating()
            session?.close()
        } catch (_: Throwable) {
        }
        session = null
        device?.close()
        device = null
        reader?.close()
        reader = null
        closeLog()
        if (this::cameraThread.isInitialized) cameraThread.quitSafely()
        if (this::analysisThread.isInitialized) analysisThread.quitSafely()
    }

    // ---------------------------------------------------------------- logging

    private fun openLog() {
        val dir = context.getExternalFilesDir(null) ?: context.filesDir
        val f = File(dir, "frames.csv")
        logFile = f
        csv = f.bufferedWriter().apply {
            write("ts_ns,frame_number,exposure_ns,iso,flash_mean,flash_min,flash_max,barcode_index,skew_ns,ae_state\n")
            flush()
        }
    }

    private fun writeCsv(s: FrameSample) {
        val w = csv ?: return
        w.write(
            "${s.sensorTimestampNs},${s.frameNumber},${s.exposureNs ?: -1},${s.sensitivity ?: -1}," +
                "%.2f,%d,%d,%d,%d,%d\n".format(
                    s.flashMean, s.flashMin, s.flashMax, s.barcodeIndex,
                    s.rollingShutterSkewNs ?: -1, s.aeState ?: -1
                )
        )
        // Flush roughly once a second; per-frame flush would add jitter.
        if (framesAnalyzed % 60L == 0L) w.flush()
    }

    private fun closeLog() {
        try {
            csv?.flush()
            csv?.close()
        } catch (_: Throwable) {
        }
        csv = null
    }
}
