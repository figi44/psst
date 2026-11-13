package io.github.figi44.psst

import android.content.Context
import android.content.res.ColorStateList
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RectF
import android.graphics.drawable.InsetDrawable
import android.graphics.drawable.RippleDrawable
import android.graphics.drawable.ShapeDrawable
import android.graphics.drawable.shapes.OvalShape
import android.os.SystemClock
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import android.widget.Button
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.hypot
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt
import kotlin.math.sin

/**
 * psst's button over other apps' keyboards. It shows what [BubbleService] decides, and passes it
 * taps and drags.
 */
class BubbleView(context: Context) : View(context) {
    interface Actions {
        fun onTap()

        /** The finger moved by [dx], [dy] pixels. */
        fun onDrag(dx: Int, dy: Int)
        fun onDrop()
    }

    /** As on the panel's button, and [ALERT] when the panel has something to say. */
    enum class Mode { START, STOP, BUSY, ALERT }

    var actions: Actions? = null

    var mode = Mode.START
        set(value) {
            if (field == value) return
            field = value
            level = 0f
            spring = 0f
            springVelocity = 0f
            haloAt = 0L
            contentDescription = context.getString(
                when (value) {
                    Mode.START -> R.string.bubble_start
                    Mode.STOP -> R.string.ime_stop
                    Mode.BUSY -> R.string.bubble_busy
                    Mode.ALERT -> R.string.bubble_alert
                },
            )
            invalidate()
        }

    // Room around the circle for the level, the busy arc and the shadow.
    private val pad = dp(6f)
    private val strong = context.getColor(R.color.ime_accent_strong)
    private val surface = context.getColor(R.color.surface)
    private val line = context.getColor(R.color.line)
    private val accent = context.getColor(R.color.accent)
    private val white = context.getColor(R.color.fg)
    private val warn = context.getColor(R.color.warn)
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply { setShadowLayer(dp(3f), 0f, dp(1f), SHADOW) }
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeCap = Paint.Cap.ROUND
    }
    private val icon = requireNotNull(context.getDrawable(R.drawable.ime_mic)).mutate()
    private val square = RectF()
    private val arc = RectF()
    private val blob = Path()
    private var level = 0f

    /** The halo's size, chasing [level] on a spring, and when it was last drawn. */
    private var spring = 0f
    private var springVelocity = 0f
    private var haloAt = 0L

    private val slop = ViewConfiguration.get(context).scaledTouchSlop
    private var downX = 0f
    private var downY = 0f
    private var lastX = 0f
    private var lastY = 0f
    private var dragging = false

    init {
        isClickable = true
        isFocusable = true
        isSoundEffectsEnabled = false
        contentDescription = context.getString(R.string.bubble_start)
        setOnClickListener { actions?.onTap() }
        val inset = pad.roundToInt()
        foreground = RippleDrawable(ColorStateList.valueOf(RIPPLE), null, InsetDrawable(ShapeDrawable(OvalShape()), inset))
    }

    /** The microphone's level, 0 to 1, while it records. */
    fun level(level: Float) {
        if (mode == Mode.STOP) this.level = level
    }

    override fun getAccessibilityClassName(): CharSequence = Button::class.java.name

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        val cx = w / 2f
        val cy = h / 2f
        val radius = min(w, h) / 2f - pad
        val half = radius * 0.5f
        icon.setBounds((cx - half).roundToInt(), (cy - half).roundToInt(), (cx + half).roundToInt(), (cy + half).roundToInt())
        val side = radius * 0.3f
        square.set(cx - side, cy - side, cx + side, cy + side)
        val ring = radius + pad / 2
        arc.set(cx - ring, cy - ring, cx + ring, cy + ring)
    }

    override fun onDraw(canvas: Canvas) {
        val cx = width / 2f
        val cy = height / 2f
        val radius = min(width, height) / 2f - pad
        if (mode == Mode.STOP) drawHalo(canvas, cx, cy, radius)
        fill.color = if (mode == Mode.BUSY) surface else strong
        canvas.drawCircle(cx, cy, radius, fill)
        when (mode) {
            Mode.START, Mode.ALERT -> {
                icon.setTint(white)
                icon.draw(canvas)
            }
            Mode.STOP -> {
                paint.color = white
                canvas.drawRoundRect(square, dp(3f), dp(3f), paint)
                // Runs only while the bubble is shown and listening.
                postInvalidateOnAnimation()
            }
            Mode.BUSY -> {
                stroke.color = line
                stroke.strokeWidth = dp(1.5f)
                canvas.drawCircle(cx, cy, radius, stroke)
                icon.setTint(context.getColor(R.color.muted))
                icon.draw(canvas)
                stroke.color = accent
                stroke.strokeWidth = dp(3f)
                val start = SystemClock.uptimeMillis() % SPIN_MS * 360f / SPIN_MS
                canvas.drawArc(arc, start, 90f, false, stroke)
                // Runs only while the bubble is shown and the core is busy.
                postInvalidateOnAnimation()
            }
        }
        if (mode == Mode.ALERT) {
            val x = cx + radius * 0.7f
            val y = cy - radius * 0.7f
            paint.color = strong
            canvas.drawCircle(x, y, dp(6f), paint)
            paint.color = warn
            canvas.drawCircle(x, y, dp(4.5f), paint)
        }
    }

    /**
     * A blob that jiggles with the voice: its size chases the level on a lightly damped spring, so
     * it overshoots and settles, and three turning waves bend its edge the more, the louder. In
     * silence it breathes, so a quiet room still shows that the microphone is on.
     */
    private fun drawHalo(canvas: Canvas, cx: Float, cy: Float, radius: Float) {
        val now = SystemClock.uptimeMillis()
        val dt = if (haloAt == 0L) 0f else min(MAX_STEP_S, (now - haloAt) / 1000f)
        haloAt = now
        // In seconds, as a double: a phone up for days would otherwise lose the waves' smoothness.
        val t = now / 1000.0
        val breath = BREATH * (0.5f + 0.5f * sin(t * 2 * PI * 1000 / BREATH_MS).toFloat())
        springVelocity += (SPRING * (max(level, breath) - spring) - DAMPING * springVelocity) * dt
        spring += springVelocity * dt
        val reach = max(0f, spring)
        drawBlob(canvas, cx, cy, radius, t, reach, grow = 0.85f, wobble = 0.6f, alpha = 0.35f, phase = 1.3)
        drawBlob(canvas, cx, cy, radius, t, reach, grow = 0.55f, wobble = 0.45f, alpha = 0.6f, phase = 0.0)
    }

    private fun drawBlob(
        canvas: Canvas,
        cx: Float,
        cy: Float,
        radius: Float,
        t: Double,
        reach: Float,
        grow: Float,
        wobble: Float,
        alpha: Float,
        phase: Double,
    ) {
        blob.rewind()
        for (i in 0..BLOB_POINTS) {
            val angle = i * 2 * PI / BLOB_POINTS
            val wave = 0.5 * sin(3 * angle + t * 2.1 + phase) + 0.3 * sin(4 * angle - t * 2.9 + 2 * phase) +
                0.2 * sin(5 * angle + t * 3.7)
            val r = radius + pad * (reach * (grow + wobble * wave.toFloat())).coerceIn(0f, 1f)
            val x = cx + r * cos(angle).toFloat()
            val y = cy + r * sin(angle).toFloat()
            if (i == 0) blob.moveTo(x, y) else blob.lineTo(x, y)
        }
        blob.close()
        paint.color = accent
        paint.alpha = (255 * alpha).roundToInt()
        canvas.drawPath(blob, paint)
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                downX = event.rawX
                downY = event.rawY
                dragging = false
                drawableHotspotChanged(event.x, event.y)
                isPressed = true
            }
            MotionEvent.ACTION_MOVE -> {
                if (!dragging && hypot(event.rawX - downX, event.rawY - downY) > slop) {
                    dragging = true
                    isPressed = false
                    lastX = downX
                    lastY = downY
                }
                if (dragging) drag(event)
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                isPressed = false
                if (dragging) {
                    drag(event)
                    dragging = false
                    actions?.onDrop()
                } else if (event.actionMasked == MotionEvent.ACTION_UP) {
                    performClick()
                }
            }
        }
        return true
    }

    // In screen pixels: the window moves under the finger, and with it the view's own.
    private fun drag(event: MotionEvent) {
        val dx = (event.rawX - lastX).roundToInt()
        val dy = (event.rawY - lastY).roundToInt()
        if (dx == 0 && dy == 0) return
        lastX += dx
        lastY += dy
        actions?.onDrag(dx, dy)
    }

    // For lint, which checks that a view handling its own touches still clicks through here, as
    // accessibility services do.
    override fun performClick() = super.performClick()

    private fun dp(value: Float) = value * resources.displayMetrics.density
}

private const val RIPPLE = 0x33ffffff
private const val SHADOW = 0x66000000
private const val BREATH = 0.3f
private const val BREATH_MS = 1400L
private const val SPRING = 180f
private const val DAMPING = 9f

/** The longest step the spring takes at once, as after a pause in drawing. */
private const val MAX_STEP_S = 0.05f
private const val BLOB_POINTS = 72
private const val SPIN_MS = 1000L
