package io.github.figi44.psst

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.RectF
import android.os.SystemClock
import kotlin.math.roundToInt

/**
 * The desktop overlay's well: the disc at the start of its pill, whose mark says what a dictation
 * is doing. The keyboard's button and the bubble draw it to the overlay's proportions.
 */
class Well(context: Context) {
    private val rest = context.getColor(R.color.well)
    private val fg = context.getColor(R.color.fg)
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG)
    private val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeCap = Paint.Cap.ROUND
    }
    private val arc = RectF()

    /** Listening: a dot of [color] in a well tinted with it, the more for the louder [swell]. */
    fun recording(canvas: Canvas, cx: Float, cy: Float, radius: Float, color: Int, swell: Float) {
        fill.color = color
        fill.alpha = (255 * (0.16f + 0.5f * swell)).roundToInt()
        canvas.drawCircle(cx, cy, radius, fill)
        fill.alpha = 255
        canvas.drawCircle(cx, cy, radius * MARK, fill)
    }

    /** The microphone opening: a ring of [color]. */
    fun starting(canvas: Canvas, cx: Float, cy: Float, radius: Float, color: Int) {
        fill.color = rest
        canvas.drawCircle(cx, cy, radius, fill)
        stroke.color = color
        stroke.strokeWidth = radius * LINE
        canvas.drawCircle(cx, cy, radius * MARK, stroke)
    }

    /** The core at work: an arc that turns, for as long as the caller keeps drawing. */
    fun busy(canvas: Canvas, cx: Float, cy: Float, radius: Float) {
        fill.color = rest
        canvas.drawCircle(cx, cy, radius, fill)
        stroke.color = fg
        stroke.strokeWidth = radius * LINE
        val ring = radius * ARC
        arc.set(cx - ring, cy - ring, cx + ring, cy + ring)
        val start = SystemClock.uptimeMillis() % SPIN_MS * 360f / SPIN_MS
        canvas.drawArc(arc, start, 270f, false, stroke)
    }

    companion object {
        /** The well's radius, out of the pill's half height. */
        const val SIZE = 0.72f
    }
}

// The overlay's icons, in sixteenths of the well's radius.
private const val MARK = 5.5f / 16
private const val ARC = 6f / 16
private const val LINE = 2f / 16
private const val SPIN_MS = 1000L
