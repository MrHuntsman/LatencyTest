package com.latencytest.sensor

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.util.AttributeSet
import android.view.View
import kotlin.math.min

/**
 * Draws the two regions the analyser actually reads, using the same fractional
 * coordinates as the PC agent's renderer so the user can frame the monitor by
 * eye. Also draws a live luminance bar inside the flash ROI.
 */
class RoiOverlayView @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null,
) : View(context, attrs) {

    private val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeWidth = 6f
    }
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
        color = Color.argb(120, 0x37, 0xd6, 0x7a)
    }

    private val flashRect = RectF()
    private val barcodeRect = RectF()
    private val cells = IntArray(16) { -1 }

    /** 0..255 mean luminance of the flash ROI, -1 when unknown. */
    var flashMean: Float = -1f
        set(value) {
            field = value
            invalidate()
        }

    /** binarised barcode cells (1 = bright), or null when not yet decoded. */
    fun setBarcode(cellStates: IntArray?) {
        cellStates?.copyInto(cells) ?: cells.fill(-1)
        invalidate()
    }

    var barcodeLocked: Boolean = false
        set(value) {
            field = value
            invalidate()
        }

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        val w = width.toFloat()
        val h = height.toFloat()

        // Barcode strip ROI
        barcodeRect.set(
            RoiSpec.BARCODE_X0 * w, RoiSpec.BARCODE_Y0 * h,
            RoiSpec.BARCODE_X1 * w, RoiSpec.BARCODE_Y1 * h,
        )
        stroke.color = if (barcodeLocked) Color.parseColor("#37d67a") else Color.parseColor("#ffb020")
        canvas.drawRect(barcodeRect, stroke)

        // Barcode cell dividers + decoded state
        val cellW = barcodeRect.width() / 16f
        val divider = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = 2f
            color = Color.argb(140, 255, 255, 255)
        }
        val cellFill = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.FILL }
        for (i in 0 until 16) {
            val left = barcodeRect.left + cellW * i
            canvas.drawLine(left, barcodeRect.top, left, barcodeRect.bottom, divider)
            when (cells[i]) {
                1 -> {
                    cellFill.color = Color.argb(90, 255, 255, 255)
                    canvas.drawRect(left, barcodeRect.top, left + cellW, barcodeRect.bottom, cellFill)
                }
                0 -> {
                    cellFill.color = Color.argb(26, 255, 255, 255)
                    canvas.drawRect(left, barcodeRect.top, left + cellW, barcodeRect.bottom, cellFill)
                }
            }
        }

        // Flash ROI
        flashRect.set(
            RoiSpec.FLASH_X0 * w, RoiSpec.FLASH_Y0 * h,
            RoiSpec.FLASH_X1 * w, RoiSpec.FLASH_Y1 * h,
        )
        val flashHot = flashMean > 60f
        stroke.color = if (flashHot) Color.parseColor("#37d67a") else Color.parseColor("#ffb020")
        canvas.drawRect(flashRect, stroke)

        // Live luminance fill: height proportional to mean luminance.
        if (flashMean >= 0f) {
            val level = min(1f, flashMean / 255f)
            val barH = flashRect.height() * level
            fill.color = if (flashHot) Color.argb(90, 0x37, 0xd6, 0x7a) else Color.argb(60, 0x4f, 0x8c, 0xff)
            canvas.drawRect(
                flashRect.left,
                flashRect.bottom - barH,
                flashRect.right,
                flashRect.bottom,
                fill,
            )
        }
    }
}
