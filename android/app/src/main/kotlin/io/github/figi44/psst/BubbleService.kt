package io.github.figi44.psst

import android.accessibilityservice.AccessibilityService
import android.annotation.SuppressLint
import android.content.Context
import android.content.SharedPreferences
import android.graphics.PixelFormat
import android.graphics.Rect
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.text.InputType
import android.view.ContextThemeWrapper
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.View
import android.view.View.MeasureSpec
import android.view.WindowInsets
import android.view.WindowManager
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import android.view.accessibility.AccessibilityNodeInfo.AccessibilityAction
import android.view.accessibility.AccessibilityWindowInfo
import android.view.inputmethod.InputMethodManager
import android.widget.TextView
import io.github.figi44.psst.BubbleView.Mode
import kotlin.math.roundToInt

/**
 * The bubble: psst's button beside other keyboards. A tap records at once. With typing in place
 * on, the words go straight into a field that allows it; otherwise the bubble switches to the psst
 * keyboard, which types them and switches back.
 */
class BubbleService : AccessibilityService(), BubbleView.Actions, Recording.Listener {
    private val main = Handler(Looper.getMainLooper())
    private lateinit var windowManager: WindowManager
    private lateinit var imm: InputMethodManager
    private lateinit var prefs: SharedPreferences
    private lateinit var params: WindowManager.LayoutParams
    private var view: BubbleView? = null
    private var shown = false
    private var dragging = false
    private val keyboard = Rect()

    /** The field the bubble's dictation started in: its words go there or are kept. */
    private var target: AccessibilityNodeInfo? = null

    /** Typed in place, until the field shows it kept the words. */
    private var confirming = false

    /**
     * A few words coming out of the bubble: Android hides an app's toasts while another app is in
     * front. Its window comes and goes with the bubble's, beneath it, so that the disc covers the
     * end of its pill.
     */
    private lateinit var note: TextView
    private lateinit var noteParams: WindowManager.LayoutParams
    private var noteShown = false
    private val hideNote = Runnable {
        dismissNote()
        update()
    }

    private val stateListener: (State) -> Unit = { render() }
    private val consumeFinal: (Final) -> Boolean = { onFinal(it) }
    private val onSaved: () -> Unit = {
        note(R.string.ime_saved)
        update()
    }

    override fun onServiceConnected() {
        windowManager = getSystemService(WindowManager::class.java)
        imm = getSystemService(InputMethodManager::class.java)
        prefs = bubblePrefs()
        val size = px(SIZE_DP)
        params = overlay(size, size, 0)
        params.title = getString(R.string.bubble_label)
        val theme = ContextThemeWrapper(this, R.style.Theme_Psst)
        view = BubbleView(theme).also { it.actions = this }
        note = TextView(theme).apply {
            textSize = 14f
            setTextColor(getColor(R.color.fg))
            gravity = Gravity.CENTER_VERTICAL
            minHeight = px(PILL_DP)
            // The overlay's pill: black, with its hairline.
            background = GradientDrawable().apply {
                cornerRadius = px(PILL_DP) / 2f
                setColor(getColor(R.color.bg))
                setStroke(px(1), getColor(R.color.line))
            }
            visibility = View.INVISIBLE
        }
        noteParams = overlay(
            WindowManager.LayoutParams.WRAP_CONTENT,
            WindowManager.LayoutParams.WRAP_CONTENT,
            WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE,
        )
        Psst.addListener(stateListener)
        Recording.addListener(this)
        update()
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent) = update()

    override fun onInterrupt() = Unit

    override fun onDestroy() {
        main.removeCallbacksAndMessages(null)
        Psst.removeListener(stateListener)
        Recording.removeListener(this)
        if (Psst.finalConsumer === consumeFinal) {
            Psst.finalConsumer = null
            Recording.stop()
        }
        if (Psst.savedListener === onSaved) Psst.savedListener = null
        if (shown) {
            windowManager.removeView(view)
            windowManager.removeView(note)
        }
        shown = false
        view = null
        super.onDestroy()
    }

    /** Beside another keyboard on a field to speak into, and for as long as its own dictation lasts. */
    private fun update() {
        val view = view ?: return
        val window = windows.find { it.type == AccessibilityWindowInfo.TYPE_INPUT_METHOD }
        window?.getBoundsInScreen(keyboard)
        // A dictation saved for later brings no final: it is over for the bubble too.
        val owned = Psst.finalConsumer === consumeFinal && (Recording.active || Psst.state.dictation != Dictation.IDLE)
        val ime = if (window != null || owned || noteShown) psstIme() else null
        // Psst's own panel does what the bubble would.
        val panel = window != null && ime == Settings.Secure.getString(contentResolver, Settings.Secure.DEFAULT_INPUT_METHOD)
        // A note keeps the bubble it comes out of, so that it can be read after the keyboard has gone.
        val show = ime != null && !panel &&
            (owned || noteShown || window != null && takesSpeech(findFocus(AccessibilityNodeInfo.FOCUS_INPUT)))
        if (show && !shown) {
            place()
            // A later window lies over an earlier one.
            windowManager.addView(note, noteParams)
            windowManager.addView(view, params)
            shown = true
            // So that a tap finds the server's connection up.
            Psst.warm()
        } else if (!show && shown) {
            shown = false
            windowManager.removeView(view)
            windowManager.removeView(note)
        } else if (show && !dragging) {
            place()
        }
        render()
    }

    private fun render() {
        val state = Psst.state
        view?.mode = when {
            Recording.listening || state.dictation == Dictation.RECORDING -> Mode.STOP
            Recording.active || confirming -> Mode.BUSY
            state.dictation != Dictation.IDLE -> Mode.BUSY
            Psst.pendingFinal != null || Recording.startFailed || Recording.failure != null ||
                state.link in BLOCKING || !hasMic() -> Mode.ALERT
            else -> Mode.START
        }
    }

    /** At the edge the user dragged it to, as far from the keyboard's top as they left it. */
    private fun place() {
        val metrics = windowManager.currentWindowMetrics
        val screen = metrics.bounds
        val top = metrics.windowInsets.getInsets(WindowInsets.Type.statusBars()).top
        val x = if (prefs.getBoolean(LEFT, false)) px(MARGIN_DP) else screen.width() - params.width - px(MARGIN_DP)
        val y = keyboard.top + px(prefs.getInt(OFFSET, DEFAULT_OFFSET_DP)) - params.height / 2
        move(x, y.coerceIn(top, screen.height() - params.height))
    }

    /** Shows [text] coming out of the bubble, toward the side with room, for a few seconds. */
    private fun note(text: Int) {
        if (!shown) return
        note.setText(text)
        noteShown = true
        placeNote()
        note.visibility = View.VISIBLE
        main.removeCallbacks(hideNote)
        main.postDelayed(hideNote, NOTE_MS)
    }

    private fun dismissNote() {
        main.removeCallbacks(hideNote)
        noteShown = false
        note.visibility = View.INVISIBLE
    }

    private fun placeNote() {
        val screen = windowManager.currentWindowMetrics.bounds
        val cx = params.x + params.width / 2
        val cy = params.y + params.height / 2
        val right = cx < screen.width() / 2
        // The pill starts under the disc's centre, its round end inside the disc.
        val tuck = px(PILL_DP) / 2
        val disc = params.width / 2 - (BubbleView.PAD_DP * resources.displayMetrics.density).roundToInt()
        val inner = tuck + disc + px(NOTE_GAP_DP)
        val outer = px(16)
        if (right) note.setPadding(inner, px(10), outer, px(10)) else note.setPadding(outer, px(10), inner, px(10))
        val room = (if (right) screen.width() - cx else cx) + tuck - px(MARGIN_DP)
        note.measure(MeasureSpec.makeMeasureSpec(room, MeasureSpec.AT_MOST), MeasureSpec.UNSPECIFIED)
        noteParams.width = note.measuredWidth
        noteParams.height = note.measuredHeight
        noteParams.x = if (right) cx - tuck else cx + tuck - note.measuredWidth
        noteParams.y = cy - note.measuredHeight / 2
        if (shown) windowManager.updateViewLayout(note, noteParams)
    }

    private fun move(x: Int, y: Int) {
        if (x == params.x && y == params.y) return
        params.x = x
        params.y = y
        if (shown) windowManager.updateViewLayout(view, params)
        if (noteShown) placeNote()
    }

    override fun onDrag(dx: Int, dy: Int) {
        // Moving the bubble's window is a change of windows too, which would put it back.
        dragging = true
        val screen = windowManager.currentWindowMetrics.bounds
        move(
            (params.x + dx).coerceIn(0, screen.width() - params.width),
            (params.y + dy).coerceIn(0, screen.height() - params.height),
        )
    }

    override fun onDrop() {
        dragging = false
        val screen = windowManager.currentWindowMetrics.bounds
        val offset = (params.y + params.height / 2 - keyboard.top) / resources.displayMetrics.density
        prefs.edit()
            .putBoolean(LEFT, params.x + params.width / 2 < screen.width() / 2)
            .putInt(OFFSET, offset.roundToInt())
            .apply()
        place()
    }

    override fun onTap() {
        // Each branch updates the bubble, which the note no longer keeps.
        if (noteShown) dismissNote()
        when {
            Recording.stop() -> tick()
            // Busy, failed, or blocked: the panel says with what, and what to do about it.
            Recording.active || confirming || Psst.state.dictation != Dictation.IDLE -> openPanel(insertPending = false)
            !Recording.canStart(hasMic()) -> openPanel(insertPending = false)
            else -> start()
        }
    }

    private fun start() {
        val field = findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
        Recording.start()
        Psst.finalConsumer = consumeFinal
        Psst.savedListener = onSaved
        target = field
        tick()
        if (field == null || !typesInPlace(field)) openPanel(insertPending = false)
        update()
    }

    /** Switches to the psst keyboard, which takes over from the bubble. */
    private fun openPanel(insertPending: Boolean) {
        val ime = psstIme() ?: return
        val keyboard = Settings.Secure.getString(contentResolver, Settings.Secure.DEFAULT_INPUT_METHOD)
        Handoff.request(Handoff.Request(insertPending, keyboard.takeIf { it != ime }, imm.currentInputMethodSubtype))
        if (!softKeyboardController.switchToInputMethod(ime)) Handoff.cancel()
    }

    private fun onFinal(final: Final): Boolean {
        Psst.finalConsumer = null
        val target = target
        this.target = null
        main.post(::update)
        if (final.text.isEmpty()) {
            note(R.string.ime_nothing_heard)
            return true
        }
        // Not in the field it was said for: Psst keeps it, and the bubble offers it.
        val field = findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
        if (target == null || field != target) return false
        val typed = if (typesInPlace(field)) typeInPlace(field, final.text) else null
        if (typed == null) {
            main.post { openPanel(insertPending = true) }
            return false
        }
        // Some fields take text and then drop it. Psst keeps the words until the field shows them.
        confirming = true
        main.postDelayed({ confirm(final, field, typed) }, CONFIRM_MS)
        return false
    }

    private fun confirm(final: Final, field: AccessibilityNodeInfo, typed: Typed) {
        confirming = false
        val now = if (field.refresh()) textOf(field) else null
        when {
            Psst.pendingFinal !== final -> {}
            shows(now, typed.expected) -> {
                Psst.takePendingFinal()
                Psst.inserted(final)
            }
            // Dropped: the psst keyboard types the words instead.
            now == typed.old && field == findFocus(AccessibilityNodeInfo.FOCUS_INPUT) -> openPanel(insertPending = true)
            // Otherwise changed, perhaps by the user typing on: the bubble offers the words, rather than
            // typing what may already be there.
        }
        update()
    }

    /** Puts [text] at [field]'s cursor; null if the field wouldn't take it. */
    private fun typeInPlace(field: AccessibilityNodeInfo, text: String): Typed? {
        val old = textOf(field) ?: return null
        var start = field.textSelectionStart
        var end = field.textSelectionEnd
        // Where the cursor is unknown, the words go at the end.
        if (start !in 0..old.length || end !in 0..old.length) {
            start = old.length
            end = old.length
        }
        if (start > end) start = end.also { end = start }
        val words = spaced(old.substring(0, start), text)
        val new = old.substring(0, start) + words + old.substring(end)
        val args = Bundle()
        args.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, new)
        if (!field.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) return null
        val cursor = Bundle()
        cursor.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, start + words.length)
        cursor.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, start + words.length)
        field.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, cursor)
        return Typed(old, new)
    }

    /** The field held [old], and should now hold [expected]. */
    private class Typed(val old: String, val expected: String)

    /** Typing in place is on, and [field] holds plain text that the bubble can replace. */
    private fun typesInPlace(field: AccessibilityNodeInfo) = prefs.getBoolean(IN_PLACE, false) &&
        field.isEditable && !field.isPassword && AccessibilityAction.ACTION_SET_TEXT in field.actionList &&
        textOf(field) != null && field.extras.getString(CHROME_ROLE).let { it == null || it in WEB_TEXT_FIELDS }

    override fun onRecordingChanged() = update()

    override fun onLevel(level: Float) {
        view?.level(level)
    }

    override fun onLimit() = tick()

    private fun psstIme() = imm.enabledInputMethodList.find { it.packageName == packageName }?.id

    private fun tick() {
        view?.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
    }

    private fun px(dp: Int) = (dp * resources.displayMetrics.density).roundToInt()
}

/**
 * Both of the bubble's windows count from the screen's corner, whatever the language's direction,
 * and the status bar and cutout are no exception: a window kept clear of the cutout lands lower than
 * its coordinates say, out of line with the other.
 */
@SuppressLint("RtlHardcoded")
private fun overlay(width: Int, height: Int, flags: Int) = WindowManager.LayoutParams(
    width,
    height,
    WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY,
    // Never focused: the field keeps the keyboard.
    WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or flags,
    PixelFormat.TRANSLUCENT,
).apply {
    gravity = Gravity.TOP or Gravity.LEFT
    fitInsetsTypes = 0
    layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
}

/** The bubble's settings, the user's own: typing in place, and where they dragged it. */
fun Context.bubblePrefs(): SharedPreferences = getSharedPreferences("bubble", Context.MODE_PRIVATE)

const val IN_PLACE = "in_place"
private const val LEFT = "left"

/** From the keyboard's top to the bubble's centre: negative above it. */
private const val OFFSET = "offset_dp"

/** Not a password, a number or a date. A field that doesn't say what it is counts. */
private fun takesSpeech(field: AccessibilityNodeInfo?): Boolean {
    if (field == null) return true
    if (field.isPassword) return false
    return field.inputType and InputType.TYPE_MASK_CLASS !in NOT_SPEECH
}

/**
 * The field holds [expected], if not to the letter: a field may change its case or spacing, and
 * typing the words again would double them.
 */
private fun shows(now: String?, expected: String) = now != null && plain(now) == plain(expected)

private fun plain(text: String) = text.trim().replace(WHITESPACE, " ").lowercase()

private val WHITESPACE = Regex("\\s+")

/** The field's text, empty while it shows its hint; null if it doesn't say. */
private fun textOf(field: AccessibilityNodeInfo): String? = if (field.isShowingHintText) "" else field.text?.toString()

private val NOT_SPEECH = setOf(InputType.TYPE_CLASS_NUMBER, InputType.TYPE_CLASS_PHONE, InputType.TYPE_CLASS_DATETIME)

// Chrome and WebViews name each field's role on a page. A rich text editor is not a text field:
// set anew, its text loses its formatting, and its cursor goes back to the start.
private const val CHROME_ROLE = "AccessibilityNodeInfo.chromeRole"
private val WEB_TEXT_FIELDS = setOf("textField", "searchBox", "textFieldWithComboBox")

/** The window: the circle with room around it for its halo and shadow. */
private const val SIZE_DP = 62
private const val MARGIN_DP = 2
private const val NOTE_MS = 3000L

/** The note's height on one line: short of the disc's, so that it reads as coming out of it. */
private const val PILL_DP = 40
private const val NOTE_GAP_DP = 10

// On the keyboard's top row, at its end, where keyboards keep a voice key of their own: what the
// bubble covers there is no part of the app.
private const val DEFAULT_OFFSET_DP = 24

/** Long enough for a field that updates on its next frame. */
private const val CONFIRM_MS = 250L
