package com.latencytest.sensor

import android.media.Image
import kotlin.math.max
import kotlin.math.min

/**
 * Reads only the Y plane (SPEC.md §4.3): no RGB conversion, no bitmap
 * allocation. A couple of narrow ROIs on a 720p frame is a few hundred
 * microseconds of work, so 60 fps analysis is comfortable.
 *
 * Coordinates are fractions of the frame, matching exactly what the PC agent
 * renders (SPEC.md §3.4) so the phone can be framed against the overlays:
 *   barcode strip : x 0.10 .. 0.90, y 0.02 .. 0.06
 *   flash rect    : x 0.15 .. 0.85, y 0.12 .. 0.92
 */
object RoiSpec {
    const val BARCODE_X0 = 0.10f
    const val BARCODE_X1 = 0.90f
    const val BARCODE_Y0 = 0.02f
    const val BARCODE_Y1 = 0.06f

    const val FLASH_X0 = 0.15f
    const val FLASH_X1 = 0.85f
    const val FLASH_Y0 = 0.12f
    const val FLASH_Y1 = 0.92f
}

data class RoiStats(
    val mean: Float,
    val min: Int,
    val max: Int,
    val samples: Int,
)

object YPlaneAnalyzer {

    /**
     * Mean/min/max luminance over a fractional ROI.
     *
     * @param stepSubsample read one pixel every N in both axes. 4 keeps a
     *        720p ROI well under a millisecond while preserving mean accuracy.
     */
    fun roiStats(image: Image, x0: Float, y0: Float, x1: Float, y1: Float, stepSubsample: Int = 4): RoiStats {
        val plane = image.planes[0]
        val buffer = plane.buffer
        val rowStride = plane.rowStride
        val pixelStride = plane.pixelStride
        val width = image.width
        val height = image.height

        val px0 = (x0 * width).toInt().coerceIn(0, width - 1)
        val px1 = (x1 * width).toInt().coerceIn(px0 + 1, width)
        val py0 = (y0 * height).toInt().coerceIn(0, height - 1)
        val py1 = (y1 * height).toInt().coerceIn(py0 + 1, height)

        val step = max(1, stepSubsample)
        var sum = 0L
        var count = 0
        var lo = 255
        var hi = 0

        var y = py0
        while (y < py1) {
            val rowStart = y * rowStride
            var x = px0
            while (x < px1) {
                val index = rowStart + x * pixelStride
                if (index < buffer.limit()) {
                    val v = buffer.get(index).toInt() and 0xFF
                    sum += v
                    count++
                    if (v < lo) lo = v
                    if (v > hi) hi = v
                }
                x += step
            }
            y += step
        }

        if (count == 0) return RoiStats(0f, 0, 0, 0)
        return RoiStats(sum.toFloat() / count, lo, hi, count)
    }

    /**
     * Column-averaged luminance for every row inside the ROI. This is the input
     * to the sub-frame rolling-shutter boundary search (SPEC.md §4.5): the row
     * at which the flash boundary sits converts to a time via
     * t = t0 + skew * row / (H - 1).
     *
     * Only called for frames that are already candidates, so the extra cost is
     * paid rarely.
     */
    fun rowProfile(image: Image, x0: Float, x1: Float, y0: Float, y1: Float, stepSubsample: Int = 8): FloatArray {
        val plane = image.planes[0]
        val buffer = plane.buffer
        val rowStride = plane.rowStride
        val pixelStride = plane.pixelStride
        val width = image.width
        val height = image.height

        val px0 = (x0 * width).toInt().coerceIn(0, width - 1)
        val px1 = (x1 * width).toInt().coerceIn(px0 + 1, width)
        val py0 = (y0 * height).toInt().coerceIn(0, height - 1)
        val py1 = (y1 * height).toInt().coerceIn(py0 + 1, height)

        val step = max(1, stepSubsample)
        val out = FloatArray(py1 - py0)
        var y = py0
        while (y < py1) {
            val rowStart = y * rowStride
            var sum = 0L
            var count = 0
            var x = px0
            while (x < px1) {
                val index = rowStart + x * pixelStride
                if (index < buffer.limit()) {
                    sum += buffer.get(index).toInt() and 0xFF
                    count++
                }
                x += step
            }
            out[y - py0] = if (count == 0) 0f else sum.toFloat() / count
            y++
        }
        return out
    }

    /**
     * Binarises a barcode strip ROI into 16 cells by averaging each cell's
     * luminance. Returns 0/1 per cell (1 = bright). Decoding to a Gray-code
     * index happens in milestone 3; this gives the raw cell states.
     */
    fun barcodeCells(image: Image, cells: Int = 16, stepSubsample: Int = 4): IntArray {
        val out = IntArray(cells)
        val span = (RoiSpec.BARCODE_X1 - RoiSpec.BARCODE_X0) / cells
        var brightest = -1f
        var darkest = 256f
        val means = FloatArray(cells)
        for (i in 0 until cells) {
            val x0 = RoiSpec.BARCODE_X0 + span * i
            val x1 = x0 + span
            val mean = roiStats(image, x0, RoiSpec.BARCODE_Y0, x1, RoiSpec.BARCODE_Y1, stepSubsample).mean
            means[i] = mean
            if (mean > brightest) brightest = mean
            if (mean < darkest) darkest = mean
        }
        // Adaptive threshold at the midpoint of the observed range.
        val threshold = (brightest + darkest) / 2f
        for (i in 0 until cells) {
            out[i] = if (means[i] > threshold) 1 else 0
        }
        return out
    }

    /**
     * Gray-code decode of 16 cells into the present index. Gray code changes one
     * bit per step, so a cell caught mid-transition costs at most 1 LSB
     * (SPEC.md §3.4).
     */
    fun grayToBinary(cells: IntArray): Int {
        var gray = 0
        for (i in cells.indices) {
            if (cells[i] != 0) gray = gray or (1 shl i)
        }
        var binary = gray
        var shift = 1
        while (shift < cells.size) {
            binary = binary xor (gray shr shift)
            shift = shift shl 1
        }
        return binary and ((1 shl cells.size) - 1)
    }

    /** clamp helper kept local to avoid importing kotlin.math in callers */
    fun clamp(v: Int, lo: Int, hi: Int): Int = min(hi, max(lo, v))
}
