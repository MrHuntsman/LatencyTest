package com.latencytest.sensor

import android.content.Context
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.hardware.camera2.params.StreamConfigurationMap
import android.util.Range
import android.util.Size
import android.util.SizeF

/**
 * Lens selection and capability verification (SPEC.md §4.1).
 *
 * The LG V50 has three rear cameras. The ultrawide has no autofocus and a much
 * smaller sensor; the telephoto is slower (f/2.4). The main 12 MP wide has the
 * largest sensor and fastest lens, which is decisive when shooting a dark room
 * at 500 us exposure.
 *
 * Selection rule, in order:
 *   1. BACK-facing
 *   2. has real autofocus (CONTROL_AF_AVAILABLE_MODES beyond OFF) - excludes the ultrawide
 *   3. largest SENSOR_INFO_PHYSICAL_SIZE area - picks the main over the tele
 *   4. widest focal length as a tie-break
 */
object CameraSelection {

    data class Candidate(
        val id: String,
        val focalLengths: FloatArray,
        val physicalSize: SizeF?,
        val hasAf: Boolean,
        val hardwareLevel: Int,
        val timestampSource: Int,
        val exposureRange: Range<Long>?,
        val sensitivityRange: Range<Int>?,
        val rollingShutterSkewNs: Long?,
        val hasManualSensor: Boolean,
        val hasReadSensorSettings: Boolean,
        /** LENS_INFO_MINIMUM_FOCUS_DISTANCE: closest focus distance in dioptres. */
        val minimumFocusDistance: Float?,
    ) {
        val sensorArea: Float
            get() = physicalSize?.let { it.width * it.height } ?: 0f

        val widestFocal: Float
            get() = focalLengths.minOrNull() ?: Float.MAX_VALUE

        fun describe(): String = buildString {
            append("camera $id: ")
            append("focal=").append(focalLengths.joinToString { "%.1f".format(it) }).append("mm")
            physicalSize?.let { append(", sensor=%.1fx%.1fmm".format(it.width, it.height)) }
            append(", area=%.1fmm²".format(sensorArea))
            append(", af=").append(hasAf)
            append(", level=").append(hardwareLevelName(hardwareLevel))
            append(", ts=").append(timestampSourceName(timestampSource))
        }
    }

    fun candidates(context: Context): List<Candidate> {
        val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
        val out = ArrayList<Candidate>()
        for (id in manager.cameraIdList) {
            val c = manager.getCameraCharacteristics(id)
            if (c.get(CameraCharacteristics.LENS_FACING) != CameraCharacteristics.LENS_FACING_BACK) continue

            val afModes = c.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES) ?: IntArray(0)
            val caps = c.get(CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES) ?: IntArray(0)

            out.add(
                Candidate(
                    id = id,
                    focalLengths = c.get(CameraCharacteristics.LENS_INFO_AVAILABLE_FOCAL_LENGTHS) ?: FloatArray(0),
                    physicalSize = c.get(CameraCharacteristics.SENSOR_INFO_PHYSICAL_SIZE),
                    hasAf = afModes.any { it != CameraCharacteristics.CONTROL_AF_MODE_OFF },
                    hardwareLevel = c.get(CameraCharacteristics.INFO_SUPPORTED_HARDWARE_LEVEL)
                        ?: CameraCharacteristics.INFO_SUPPORTED_HARDWARE_LEVEL_LEGACY,
                    timestampSource = c.get(CameraCharacteristics.SENSOR_INFO_TIMESTAMP_SOURCE)
                        ?: CameraCharacteristics.SENSOR_INFO_TIMESTAMP_SOURCE_UNKNOWN,
                    exposureRange = c.get(CameraCharacteristics.SENSOR_INFO_EXPOSURE_TIME_RANGE),
                    sensitivityRange = c.get(CameraCharacteristics.SENSOR_INFO_SENSITIVITY_RANGE),
                    rollingShutterSkewNs = null, // CaptureResult key: read per frame
                    hasManualSensor = caps.contains(
                        CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES_MANUAL_SENSOR
                    ),
                    hasReadSensorSettings = caps.contains(
                        CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES_READ_SENSOR_SETTINGS
                    ),
                    minimumFocusDistance = c.get(CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE),
                )
            )
        }
        return out
    }

    /** @return camera id of the main wide lens, or null if none qualifies. */
    fun pickMainWide(context: Context): Candidate? {
        val all = candidates(context)
        return all.filter { it.hasAf }
            .maxWithOrNull(compareBy({ it.sensorArea }, { -it.widestFocal }))
            ?: all.maxByOrNull { it.sensorArea }
    }

    fun streamConfig(context: Context, id: String): StreamConfigurationMap? {
        val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
        return manager.getCameraCharacteristics(id)
            .get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
    }

    /**
     * Picks a preview size matching the capture aspect (16:9) to keep the ROI
     * overlays aligned between preview and analysed frames.
     */
    fun pickPreviewSize(map: StreamConfigurationMap, targetAspect: Double): Size? {
        val sizes = map.getOutputSizes(android.view.SurfaceHolder::class.java) ?: return null
        return sizes.filter { it.width <= 1920 && it.height <= 1080 }
            .minByOrNull { size ->
                val aspect = size.width.toDouble() / size.height
                // prefer correct aspect first, then the largest area
                val aspectPenalty = if (aspect in (targetAspect - 0.02)..(targetAspect + 0.02)) 0.0 else 100.0
                aspectPenalty - (size.width.toLong() * size.height) / 1_000_000.0
            }
    }

    /** Verifies the checks from §4.1 that must fail loudly if absent. */
    fun verify(c: Candidate, requiredExposureNs: Long, requiredSensitivity: Int): List<String> {
        val problems = ArrayList<String>()
        if (c.hardwareLevel < CameraCharacteristics.INFO_SUPPORTED_HARDWARE_LEVEL_FULL) {
            problems += "hardware level is ${hardwareLevelName(c.hardwareLevel)}, need FULL or better"
        }
        if (!c.hasManualSensor) problems += "missing MANUAL_SENSOR capability"
        if (!c.hasReadSensorSettings) problems += "missing READ_SENSOR_SETTINGS capability"
        c.exposureRange?.let { r ->
            if (requiredExposureNs < r.lower || requiredExposureNs > r.upper) {
                problems += "exposure ${requiredExposureNs}ns outside sensor range ${r.lower}..${r.upper}"
            }
        } ?: problems.add("no SENSOR_INFO_EXPOSURE_TIME_RANGE")
        c.sensitivityRange?.let { r ->
            if (requiredSensitivity < r.lower || requiredSensitivity > r.upper) {
                problems += "sensitivity $requiredSensitivity outside sensor range ${r.lower}..${r.upper}"
            }
        } ?: problems.add("no SENSOR_INFO_SENSITIVITY_RANGE")
        // SENSOR_ROLLING_SHUTTER_SKEW is per-frame CaptureResult metadata; whether
        // the HAL populates it can only be confirmed at runtime. §4.5 covers the
        // fallback: self-calibrate the skew from barcode spacing within a frame.
        if (c.minimumFocusDistance == null) {
            problems += "LENS_INFO_MINIMUM_FOCUS_DISTANCE unavailable (manual focus may be coarse)"
        }
        return problems
    }

    fun hardwareLevelName(level: Int): String = when (level) {
        CameraCharacteristics.INFO_SUPPORTED_HARDWARE_LEVEL_LEGACY -> "LEGACY"
        CameraCharacteristics.INFO_SUPPORTED_HARDWARE_LEVEL_LIMITED -> "LIMITED"
        CameraCharacteristics.INFO_SUPPORTED_HARDWARE_LEVEL_FULL -> "FULL"
        CameraCharacteristics.INFO_SUPPORTED_HARDWARE_LEVEL_3 -> "LEVEL_3"
        CameraCharacteristics.INFO_SUPPORTED_HARDWARE_LEVEL_EXTERNAL -> "EXTERNAL"
        else -> "?"
    }

    fun timestampSourceName(src: Int): String = when (src) {
        CameraCharacteristics.SENSOR_INFO_TIMESTAMP_SOURCE_REALTIME -> "REALTIME (CLOCK_BOOTTIME)"
        CameraCharacteristics.SENSOR_INFO_TIMESTAMP_SOURCE_UNKNOWN -> "UNKNOWN (CLOCK_MONOTONIC)"
        else -> "?"
    }

    /** Full text report shown in the UI so the user can override by hand if needed. */
    fun report(context: Context): String = buildString {
        val all = candidates(context)
        append("Rear cameras found: ").append(all.size).append("\n")
        all.forEach { append("  ").append(it.describe()).append('\n') }
        val pick = pickMainWide(context)
        append("\nSelected: ")
        append(pick?.id ?: "none")
        if (pick != null) {
            append("  (area %.1fmm², focal %s)".format(pick.sensorArea, pick.focalLengths.joinToString()))
            append("\nProblems:\n")
            val problems = verify(context, pick)
            if (problems.isEmpty()) append("  none\n") else problems.forEach { append("  - ").append(it).append('\n') }
        }
    }

    private fun verify(context: Context, c: Candidate): List<String> {
        val maxArea = candidates(context).maxOfOrNull { it.sensorArea } ?: 0f
        return verify(c, DEFAULT_EXPOSURE_NS, DEFAULT_SENSITIVITY) +
            if (c.sensorArea < maxArea) listOf("not the largest rear sensor") else emptyList()
    }

    const val DEFAULT_EXPOSURE_NS: Long = 500_000L      // 1/2000 s (§4.2)
    const val DEFAULT_SENSITIVITY: Int = 1600
    const val MAX_SENSITIVITY: Int = 3200
    const val FRAME_DURATION_60FPS_NS: Long = 16_666_666L
}
