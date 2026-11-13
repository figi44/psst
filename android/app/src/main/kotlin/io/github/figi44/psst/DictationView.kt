package io.github.figi44.psst

import android.content.Context
import android.content.res.ColorStateList
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.RectF
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.Drawable
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.InsetDrawable
import android.graphics.drawable.LayerDrawable
import android.graphics.drawable.RippleDrawable
import android.graphics.drawable.ShapeDrawable
import android.graphics.drawable.shapes.OvalShape
import android.os.SystemClock
import android.text.TextUtils
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.WindowInsets
import android.view.inputmethod.EditorInfo
import android.widget.Button
import android.widget.FrameLayout
import android.widget.ImageButton
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import kotlin.math.PI
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt
import kotlin.math.sin
import io.github.figi44.psst.DictationView.Mic

/** The keyboard's panel. It shows what [PsstIme] decides and passes the user's taps to [actions]. */
class DictationView(context: Context) : FrameLayout(context) {
    interface Actions {
        fun onMic()
        fun onCancel()
        fun onBackspace()
        fun onEnter()
        fun onAction(action: Action)
        fun onHistory()
        fun onHistoryBack()
        fun onHistoryPick(entry: Entry)
        fun onHistoryDiscard(entry: Entry)
        fun onSwitchKeyboard()
    }

    enum class Mic { START, STOP, BUSY }

    enum class Tone { PLAIN, MUTED, WARN, DANGER }

    enum class Action(val label: Int) {
        SET_UP(R.string.ime_set_up),
        ALLOW_MIC(R.string.ime_allow_mic),
        OPEN(R.string.ime_open_psst),
        INSERT(R.string.ime_insert),
        DISMISS(R.string.ime_dismiss),
    }

    sealed interface Ui {
        /** The round button, the meter (live while [listening]), and one status line. */
        data class Dial(
            val mic: Mic,
            val status: CharSequence,
            val tone: Tone,
            val listening: Boolean = false,
        ) : Ui

        /** One line and up to two buttons, in place of the dial. */
        data class Message(
            val text: CharSequence,
            val tone: Tone,
            val primary: Action,
            val secondary: Action? = null,
        ) : Ui

        /**
         * Earlier dictations to type again, in place of everything else: the [saved] ones first, then
         * the server's [entries], none yet while loading.
         */
        data class History(
            val saved: List<Entry>,
            val entries: List<Entry>,
            val note: CharSequence?,
            val tone: Tone,
        ) : Ui
    }

    /** One dictation in the history. [text] is null while the server hasn't transcribed a saved one. */
    data class Entry(
        val sessionId: String,
        val text: String?,
        val time: CharSequence,
        val saved: Boolean = false,
        /** Its next discard deletes the recording. */
        val confirming: Boolean = false,
    )

    var actions: Actions? = null

    /** There is another keyboard to switch to. */
    var switchable = false
        set(value) {
            field = value
            switchKey.visibility = if (value) VISIBLE else INVISIBLE
        }

    // Phones in landscape: a shorter panel leaves the app some room.
    private val compact = resources.configuration.screenHeightDp < COMPACT_BELOW_DP
    private val dialSize = px(if (compact) 84 else 96)
    private val keySize = px(if (compact) 48 else 56)

    private val meter = Meter(context)
    private val mic = MicButton(context)
    private val dial = FrameLayout(context)
    private val status = TextView(context)
    private val message = TextView(context)
    private val primary = pill(filled = true)
    private val secondary = pill(filled = false)
    private val buttons = LinearLayout(context)
    private val discard = ImageButton(context)
    private val history = ImageButton(context)
    private val historyDot = View(context)
    private val switchKey = ImageButton(context)
    private val content = LinearLayout(context)
    private val historyPane = LinearLayout(context)
    private val historyNote = TextView(context)
    private val savedLabel = TextView(context)
    private val historyList = LinearLayout(context)
    private val historyScroll = ScrollView(context)
    private var shownHistory: Ui.History? = null
    private val backspace = RepeatKey(context)
    private val enter = ImageButton(context)
    private var primaryAction: Action? = null
    private var secondaryAction: Action? = null

    /** A saved dictation has text that never went in: the history key says so. */
    var unused = false
        set(value) {
            field = value
            showDot()
        }

    init {
        val layers = arrayOf<Drawable>(ColorDrawable(color(R.color.line)), ColorDrawable(color(R.color.bg)))
        background = LayerDrawable(layers).apply {
            // A hairline on top sets the panel apart from a black app above it.
            setLayerInset(1, 0, px(1), 0, 0)
        }
        // The window reaches under the navigation bar: the panel is black there too, its controls clear of it.
        setOnApplyWindowInsetsListener { _, insets ->
            val bars = insets.getInsets(WindowInsets.Type.navigationBars() or WindowInsets.Type.displayCutout())
            setPadding(bars.left, 0, bars.right, bars.bottom)
            insets
        }

        meter.thickness = dialSize - 2f * mic.inset
        dial.addView(meter, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        dial.addView(mic, LayoutParams(dialSize, dialSize, Gravity.CENTER))
        mic.setOnClickListener { actions?.onMic() }

        status.textSize = 14f
        status.gravity = Gravity.CENTER_HORIZONTAL
        // Two lines always, so the button doesn't move when a status wraps.
        status.setLines(2)
        status.ellipsize = TextUtils.TruncateAt.END

        message.textSize = 15f
        message.gravity = Gravity.CENTER
        message.maxLines = 3
        message.ellipsize = TextUtils.TruncateAt.END
        message.accessibilityLiveRegion = ACCESSIBILITY_LIVE_REGION_POLITE

        buttons.orientation = LinearLayout.HORIZONTAL
        buttons.gravity = Gravity.CENTER
        buttons.addView(primary, LinearLayout.LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT))
        buttons.addView(
            secondary,
            LinearLayout.LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT).apply {
                marginStart = px(12)
            },
        )
        primary.setOnClickListener { primaryAction?.let { actions?.onAction(it) } }
        secondary.setOnClickListener { secondaryAction?.let { actions?.onAction(it) } }

        val centre = LinearLayout(context)
        centre.orientation = LinearLayout.VERTICAL
        centre.gravity = Gravity.CENTER
        centre.addView(dial, LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, dialSize))
        centre.addView(message, LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT))
        centre.addView(
            buttons,
            LinearLayout.LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT).apply {
                topMargin = px(16)
            },
        )

        // Like a keyboard's right edge: delete above enter.
        val editing = LinearLayout(context)
        editing.orientation = LinearLayout.VERTICAL
        editing.addView(key(backspace, R.drawable.ime_backspace, R.string.ime_backspace), keySize, keySize)
        editing.addView(key(enter, R.drawable.ime_enter, R.string.ime_enter), keySize, keySize)

        // Every control sits level with the button, clear of the navigation bar's own keys below.
        val row = LinearLayout(context)
        row.gravity = Gravity.CENTER_VERTICAL
        // One slot: the history while idle, discarding while there is a dictation.
        val slot = FrameLayout(context)
        slot.addView(key(history, R.drawable.ime_history, R.string.ime_history), keySize, keySize)
        slot.addView(key(discard, R.drawable.ime_discard, R.string.ime_discard_dictation), keySize, keySize)
        historyDot.background = ShapeDrawable(OvalShape()).apply { paint.color = color(R.color.accent) }
        historyDot.importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_NO
        slot.addView(
            historyDot,
            LayoutParams(px(8), px(8), Gravity.TOP or Gravity.END).apply {
                topMargin = px(12)
                marginEnd = px(12)
            },
        )
        val side = LinearLayout(context)
        side.orientation = LinearLayout.VERTICAL
        side.addView(key(switchKey, R.drawable.ime_keyboard, R.string.ime_switch_keyboard), keySize, keySize)
        side.addView(slot, keySize, keySize)
        switchKey.visibility = INVISIBLE
        row.addView(side)
        row.addView(centre, LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f))
        row.addView(editing)

        content.orientation = LinearLayout.VERTICAL
        content.gravity = Gravity.CENTER
        content.addView(row, LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT))
        content.addView(
            status,
            LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT).apply {
                topMargin = px(8)
            },
        )
        addView(
            content,
            LayoutParams(LayoutParams.MATCH_PARENT, px(if (compact) 160 else 200)).apply {
                marginStart = px(8)
                marginEnd = px(8)
            },
        )

        // As the overlay's menu: a muted header over a hairline, then rows.
        val back = key(ImageButton(context), R.drawable.ime_back, R.string.ime_history_back)
        back.imageTintList = ColorStateList.valueOf(color(R.color.muted))
        back.setOnClickListener { actions?.onHistoryBack() }
        val title = TextView(context)
        title.setText(R.string.ime_history)
        title.textSize = 15f
        title.setTextColor(color(R.color.muted))
        title.gravity = Gravity.CENTER_VERTICAL
        val header = LinearLayout(context)
        header.addView(back, keySize, keySize)
        header.addView(title, LinearLayout.LayoutParams(0, LayoutParams.MATCH_PARENT, 1f))
        val rule = View(context)
        rule.setBackgroundColor(color(R.color.line))

        historyNote.textSize = 14f
        historyNote.gravity = Gravity.CENTER_HORIZONTAL
        historyNote.setPadding(px(16), px(24), px(16), 0)
        historyNote.accessibilityLiveRegion = ACCESSIBILITY_LIVE_REGION_POLITE
        historyList.orientation = LinearLayout.VERTICAL
        historyList.setPadding(0, px(4), 0, px(4))
        savedLabel.setText(R.string.ime_history_not_inserted)
        savedLabel.textSize = 12f
        savedLabel.setTextColor(color(R.color.muted))
        savedLabel.setPadding(px(12), px(8), px(12), px(4))
        historyScroll.addView(historyList, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT))
        historyPane.orientation = LinearLayout.VERTICAL
        historyPane.addView(header, LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, keySize))
        historyPane.addView(
            rule,
            LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, px(1)).apply {
                marginStart = px(6)
                marginEnd = px(6)
            },
        )
        historyPane.addView(historyScroll, LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, 0, 1f))
        // Taller than the dial, for a few lines of each dictation.
        addView(
            historyPane,
            LayoutParams(LayoutParams.MATCH_PARENT, px(if (compact) 160 else 296)).apply {
                marginStart = px(8)
                marginEnd = px(8)
            },
        )

        history.setOnClickListener { actions?.onHistory() }
        switchKey.setOnClickListener { actions?.onSwitchKeyboard() }
        discard.setOnClickListener { actions?.onCancel() }
        backspace.setOnClickListener { actions?.onBackspace() }
        enter.setOnClickListener { actions?.onEnter() }

        show(Ui.Dial(Mic.START, "", Tone.MUTED))
    }

    fun show(ui: Ui) {
        content.visibility = if (ui is Ui.History) GONE else VISIBLE
        historyPane.visibility = if (ui is Ui.History) VISIBLE else GONE
        if (ui !is Ui.History) shownHistory = null
        val isDial = ui is Ui.Dial
        dial.visibility = if (isDial) VISIBLE else GONE
        status.visibility = dial.visibility
        message.visibility = if (isDial) GONE else VISIBLE
        buttons.visibility = message.visibility
        when (ui) {
            is Ui.Dial -> {
                mic.mode = ui.mic
                mic.listening = ui.listening
                mic.warning = ui.tone == Tone.WARN
                mic.contentDescription = when (ui.mic) {
                    Mic.START -> context.getString(R.string.ime_start)
                    Mic.STOP -> context.getString(R.string.ime_stop)
                    Mic.BUSY -> ui.status
                }
                meter.shown = ui.mic != Mic.START
                meter.active = ui.listening
                setText(status, ui.status, ui.tone)
                // Read out while recording, the status would be recorded too.
                status.accessibilityLiveRegion =
                    if (ui.mic == Mic.STOP) ACCESSIBILITY_LIVE_REGION_NONE else ACCESSIBILITY_LIVE_REGION_POLITE
                discard.visibility = if (ui.mic == Mic.START) INVISIBLE else VISIBLE
                history.visibility = if (ui.mic == Mic.START) VISIBLE else INVISIBLE
                history.contentDescription = context.getString(
                    if (unused) R.string.ime_history_with_unused else R.string.ime_history,
                )
                primaryAction = null
                secondaryAction = null
            }
            is Ui.Message -> {
                meter.shown = false
                meter.active = false
                setText(message, ui.text, ui.tone)
                primaryAction = ui.primary
                primary.setText(ui.primary.label)
                secondaryAction = ui.secondary
                secondary.visibility = if (ui.secondary == null) GONE else VISIBLE
                ui.secondary?.let { secondary.setText(it.label) }
                discard.visibility = INVISIBLE
                history.visibility = INVISIBLE
            }
            is Ui.History -> {
                meter.shown = false
                meter.active = false
                showHistory(ui)
            }
        }
        showDot()
    }

    private fun showHistory(ui: Ui.History) {
        if (ui == shownHistory) return
        // Only a change to the saved ones (a text arriving, one discarded) keeps the scroll.
        val sameList = shownHistory?.let { it.entries == ui.entries && it.note == ui.note } == true
        shownHistory = ui
        historyList.removeAllViews()
        if (ui.saved.isNotEmpty()) historyList.addView(savedLabel)
        for (entry in ui.saved) historyList.addView(entryView(entry))
        ui.note?.let {
            setText(historyNote, it, ui.tone)
            historyList.addView(historyNote, LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT)
        }
        for (entry in ui.entries) historyList.addView(entryView(entry))
        if (!sameList) historyScroll.scrollTo(0, 0)
    }

    /** As a row of the overlay's menu: the text, and its time on the right, level with its first line. */
    private fun entryView(entry: Entry): View {
        val text = TextView(context)
        when {
            entry.confirming -> {
                text.setText(R.string.ime_history_confirm_discard)
                text.setTextColor(color(R.color.danger))
            }
            entry.text == null -> {
                text.setText(R.string.ime_history_waiting)
                text.setTextColor(color(R.color.muted))
            }
            else -> {
                text.text = entry.text
                text.setTextColor(color(R.color.fg))
            }
        }
        text.textSize = 15f
        text.maxLines = 2
        text.ellipsize = TextUtils.TruncateAt.END
        val time = TextView(context)
        time.textSize = 13f
        time.maxLines = 1
        time.text = entry.time
        time.setTextColor(color(R.color.muted))
        val line = LinearLayout(context)
        line.addView(text, LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f))
        line.addView(
            time,
            LinearLayout.LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT).apply {
                marginStart = px(12)
            },
        )
        val row = LinearLayout(context)
        row.gravity = Gravity.CENTER_VERTICAL
        row.setPadding(px(12), px(6), if (entry.saved) 0 else px(12), px(6))
        row.minimumHeight = px(48)
        val mask = GradientDrawable().apply {
            cornerRadius = dp(14f)
            setColor(color(R.color.fg))
        }
        row.background = RippleDrawable(ColorStateList.valueOf(color(R.color.ripple)), null, mask)
        row.isSoundEffectsEnabled = false
        if (entry.saved) {
            // Filled once there's text to insert, a ring while it waits for the server.
            val mark = View(context)
            mark.background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL
                if (entry.text == null) {
                    setStroke(dp(1.5f).roundToInt(), color(R.color.muted))
                } else {
                    setColor(color(R.color.accent))
                }
            }
            row.addView(mark, LinearLayout.LayoutParams(px(8), px(8)).apply { marginEnd = px(12) })
        }
        row.addView(line, LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f))
        if (entry.saved) {
            val discard = key(ImageButton(context), R.drawable.ime_discard, R.string.ime_discard)
            discard.setOnClickListener { actions?.onHistoryDiscard(entry) }
            row.addView(discard, px(48), px(48))
        }
        row.setOnClickListener { actions?.onHistoryPick(entry) }
        return row
    }

    private fun showDot() {
        historyDot.visibility = if (unused && history.visibility == VISIBLE) VISIBLE else INVISIBLE
    }

    /** The microphone's level, 0 to 1, while it records. */
    fun level(level: Float) {
        meter.level(level)
        mic.level(level)
    }

    /** Shows what the enter key does in [editor]: the same choice as [android.inputmethodservice.InputMethodService.sendKeyChar]. */
    fun setEditor(editor: EditorInfo) {
        val options = editor.imeOptions
        val action = if ((options and EditorInfo.IME_FLAG_NO_ENTER_ACTION) != 0) {
            EditorInfo.IME_ACTION_NONE
        } else {
            options and EditorInfo.IME_MASK_ACTION
        }
        val (icon, label) = when (action) {
            EditorInfo.IME_ACTION_GO -> R.drawable.ime_next to R.string.ime_go
            EditorInfo.IME_ACTION_NEXT -> R.drawable.ime_next to R.string.ime_next
            EditorInfo.IME_ACTION_PREVIOUS -> R.drawable.ime_enter to R.string.ime_previous
            EditorInfo.IME_ACTION_SEARCH -> R.drawable.ime_search to R.string.ime_search
            EditorInfo.IME_ACTION_SEND -> R.drawable.ime_send to R.string.ime_send
            EditorInfo.IME_ACTION_DONE -> R.drawable.ime_done to R.string.ime_done
            else -> R.drawable.ime_enter to R.string.ime_enter
        }
        enter.setImageResource(icon)
        enter.contentDescription = context.getString(label)
    }

    // Setting the same text again would make a live region announce it again.
    private fun setText(view: TextView, text: CharSequence, tone: Tone) {
        if (!TextUtils.equals(view.text, text)) view.text = text
        view.setTextColor(
            color(
                when (tone) {
                    Tone.PLAIN -> R.color.fg
                    Tone.MUTED -> R.color.muted
                    Tone.WARN -> R.color.warn
                    Tone.DANGER -> R.color.danger
                },
            ),
        )
    }

    private fun key(key: ImageButton, icon: Int, description: Int): ImageButton {
        key.setImageResource(icon)
        key.contentDescription = context.getString(description)
        key.imageTintList = ColorStateList.valueOf(color(R.color.fg))
        key.scaleType = ImageView.ScaleType.CENTER
        key.background = RippleDrawable(ColorStateList.valueOf(color(R.color.ripple)), null, null).apply {
            radius = px(24)
        }
        key.isSoundEffectsEnabled = false
        return key
    }

    /** A primary capsule, [filled] with the foreground, or a secondary one. */
    private fun pill(filled: Boolean) = Button(context).apply {
        isAllCaps = false
        stateListAnimator = null
        isSoundEffectsEnabled = false
        textSize = 15f
        setTextColor(color(if (filled) R.color.bg else R.color.fg))
        minHeight = px(48)
        minimumHeight = px(48)
        minWidth = px(96)
        minimumWidth = px(96)
        setPadding(px(20), 0, px(20), 0)
        val shape = GradientDrawable()
        shape.cornerRadius = dp(24f)
        shape.setColor(color(if (filled) R.color.fg else R.color.line))
        val ripple = color(if (filled) R.color.ripple_dark else R.color.ripple)
        background = RippleDrawable(ColorStateList.valueOf(ripple), shape, null)
    }
}

/**
 * The one primary control: a foreground disc to start, then the overlay's well inside the [Meter]'s
 * pill, a dot while recording and a turning arc while the core works.
 */
private class MicButton(context: Context) : View(context) {
    var mode = Mic.START
        set(value) {
            if (field == value) return
            field = value
            loudness = 0f
            isEnabled = value != Mic.BUSY
            ripple.setColor(ColorStateList.valueOf(color(if (value == Mic.START) R.color.ripple_dark else R.color.ripple)))
            invalidate()
        }

    /** The microphone is open: a dot, where a ring says it is still opening. */
    var listening = false
        set(value) {
            if (field == value) return
            field = value
            invalidate()
        }

    /** Amber, as the overlay's while offline or running out of time. */
    var warning = false
        set(value) {
            if (field == value) return
            field = value
            invalidate()
        }

    /** Room around the disc, as on the [Meter]'s pill. */
    val inset = px(8)
    private val fg = color(R.color.fg)
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG)
    private val icon = requireNotNull(context.getDrawable(R.drawable.ime_mic)).mutate().apply {
        setTint(color(R.color.bg))
    }
    private val well = Well(context)
    private var loudness = 0f
    private val ripple = RippleDrawable(
        ColorStateList.valueOf(color(R.color.ripple_dark)),
        null,
        InsetDrawable(ShapeDrawable(OvalShape()), inset),
    )

    init {
        isClickable = true
        isFocusable = true
        isSoundEffectsEnabled = false
        foreground = ripple
    }

    fun level(level: Float) {
        if (mode != Mic.STOP) return
        loudness = swell(level)
        invalidate()
    }

    override fun getAccessibilityClassName(): CharSequence = Button::class.java.name

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        val cx = w / 2f
        val cy = h / 2f
        val half = (min(w, h) / 2f - inset) * 0.45f
        icon.setBounds((cx - half).roundToInt(), (cy - half).roundToInt(), (cx + half).roundToInt(), (cy + half).roundToInt())
    }

    override fun onDraw(canvas: Canvas) {
        val cx = width / 2f
        val cy = height / 2f
        val radius = min(width, height) / 2f - inset
        val color = color(if (warning) R.color.warn else R.color.accent)
        when (mode) {
            Mic.START -> {
                fill.color = fg
                canvas.drawCircle(cx, cy, radius, fill)
                icon.draw(canvas)
            }
            Mic.STOP -> if (listening) {
                well.recording(canvas, cx, cy, radius * Well.SIZE, color, loudness)
            } else {
                well.starting(canvas, cx, cy, radius * Well.SIZE, color)
            }
            Mic.BUSY -> {
                well.busy(canvas, cx, cy, radius * Well.SIZE)
                // Runs only while the window is drawn and the core is busy.
                postInvalidateOnAnimation()
            }
        }
    }
}

/**
 * The overlay's pill round the button while a dictation runs, with bars that run outwards from the
 * button with the voice. They move only while [active], and then also in silence, so that a quiet
 * room still shows that the microphone is on.
 */
private class Meter(context: Context) : View(context) {
    /** The pill's height: the button's. */
    var thickness = 0f

    var shown = false
        set(value) {
            if (field == value) return
            field = value
            invalidate()
        }

    var active = false
        set(value) {
            if (field == value) return
            field = value
            levels.fill(0f)
            peak = 0f
            chunks = 0
            pushedAt = SystemClock.uptimeMillis()
            invalidate()
        }

    // A ring, newest bar at `newest`.
    private val levels = FloatArray(MAX_BARS)
    private var newest = 0
    private var peak = 0f
    private var chunks = 0
    private var pushedAt = 0L
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val edge = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        color = color(R.color.line)
    }
    private val pill = RectF()
    private val bar = RectF()
    private val pitch = dp(7f)
    private val barWidth = dp(3f)
    private val ripple = dp(4f)
    private val bg = color(R.color.bg)
    private val live = color(R.color.fg)
    private val still = color(R.color.faint)

    init {
        importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_NO
    }

    fun level(level: Float) {
        if (!active) return
        peak = max(peak, level)
        if (++chunks < CHUNKS_PER_BAR) return
        newest = (newest + 1) % MAX_BARS
        levels[newest] = peak
        peak = 0f
        chunks = 0
        pushedAt = SystemClock.uptimeMillis()
    }

    override fun onDraw(canvas: Canvas) {
        if (!shown) return
        val centre = width / 2f
        val middle = height / 2f
        val half = thickness / 2
        // A hairline edge, as the overlay's, sets the pill apart from the black panel.
        val hair = max(1f, dp(0.5f))
        edge.strokeWidth = hair
        pill.set(hair / 2, middle - half + hair / 2, width - hair / 2, middle + half - hair / 2)
        paint.color = bg
        canvas.drawRoundRect(pill, half, half, paint)
        canvas.drawRoundRect(pill, half, half, edge)

        // As the overlay's: clear of the well, and of the pill's round end.
        val gap = half + dp(1f)
        val count = min(MAX_BARS - 1, ((centre - half * 0.9f - gap) / pitch).toInt() - 1)
        if (count <= 0) return
        val now = SystemClock.uptimeMillis()
        // Between two bars, all of them drift outwards, so the motion is smooth.
        val drift = if (active) min(1f, (now - pushedAt) / BAR_MS) * pitch else 0f
        val phase = now % RIPPLE_MS * (2 * PI.toFloat()) / RIPPLE_MS
        val tallest = thickness * 0.62f - barWidth
        val radius = barWidth / 2
        paint.color = if (active) live else still
        for (i in 0 until count) {
            var size = barWidth
            if (active) {
                val wave = ripple * (0.5f + 0.5f * sin(phase - i * 0.8f))
                size += max(levels[(newest - i + MAX_BARS) % MAX_BARS] * tallest, wave)
            }
            paint.alpha = (255 * (1 - 0.6f * i / count)).toInt()
            val offset = gap + i * pitch + drift
            bar.set(centre + offset, middle - size / 2, centre + offset + barWidth, middle + size / 2)
            canvas.drawRoundRect(bar, radius, radius, paint)
            bar.offsetTo(centre - offset - barWidth, bar.top)
            canvas.drawRoundRect(bar, radius, radius, paint)
        }
        if (active) postInvalidateOnAnimation()
    }
}

/** Clicks on touch, then again and again while held. */
private class RepeatKey(context: Context) : ImageButton(context) {
    private val repeat = object : Runnable {
        override fun run() {
            callOnClick()
            postDelayed(this, REPEAT_MS)
        }
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                drawableHotspotChanged(event.x, event.y)
                isPressed = true
                performClick()
                postDelayed(repeat, FIRST_REPEAT_MS)
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> release()
        }
        return true
    }

    // For lint, which checks that a view handling its own touches still clicks through here, as
    // accessibility services do.
    override fun performClick() = super.performClick()

    override fun onWindowVisibilityChanged(visibility: Int) {
        super.onWindowVisibilityChanged(visibility)
        if (visibility != VISIBLE) release()
    }

    private fun release() {
        isPressed = false
        removeCallbacks(repeat)
    }
}

private fun View.dp(value: Float) = value * resources.displayMetrics.density

private fun View.px(value: Int) = dp(value.toFloat()).roundToInt()

private fun View.color(id: Int) = context.getColor(id)

private const val COMPACT_BELOW_DP = 480
private const val FIRST_REPEAT_MS = 400L
private const val REPEAT_MS = 150L
private const val MAX_BARS = 24
private const val CHUNKS_PER_BAR = 3
private const val BAR_MS = 60f
private const val RIPPLE_MS = 1200L
