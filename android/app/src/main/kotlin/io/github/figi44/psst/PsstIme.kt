package io.github.figi44.psst

import android.content.Intent
import android.inputmethodservice.InputMethodService
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.text.format.DateUtils
import android.view.ContextThemeWrapper
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.View
import android.view.WindowInsetsController
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.view.inputmethod.InputMethodSubtype
import io.github.figi44.psst.DictationView.Action
import io.github.figi44.psst.DictationView.Mic
import io.github.figi44.psst.DictationView.Tone
import io.github.figi44.psst.DictationView.Ui

/**
 * The keyboard: a dictation panel. It types the finals of the dictations it starts, or takes over
 * from the bubble, into the field it serves.
 */
class PsstIme : InputMethodService(), DictationView.Actions, Recording.Listener {
    private val main = Handler(Looper.getMainLooper())
    private var view: DictationView? = null

    /**
     * Counts the inputs served: a final is typed only into the one its dictation started in. A
     * restarted input counts too, because that is also how a web page moves between its fields.
     */
    private var field = 0
    private var dictationField = 0

    /** The window was hidden: the next [onStartInputView] opens the panel. */
    private var opening = true

    /** The bubble opened the panel: its keyboard comes back once that dictation is done. */
    private var returnTo: Handoff.Request? = null
    private val returning get() = returnTo != null

    private var inputViewStarted = false
    private var micGranted = false
    private var notice: Notice? = null

    /** The panel shows the history: what the server said of it, or nothing yet. */
    private var showingHistory = false
    private var history: History? = null

    /** The saved dictation whose recording the next tap on its discard button deletes. */
    private var confirmingDiscard: String? = null

    private val stateListener: (State) -> Unit = { render() }
    private val consumeFinal: (Final) -> Boolean = { onFinal(it) }
    private val historyListener: (History) -> Unit = {
        history = it
        render()
    }
    private val onSaved: () -> Unit = { notify(R.string.ime_saved, Tone.WARN) }
    private val clearNotice = Runnable {
        notice = null
        render()
    }

    override fun onCreate() {
        super.onCreate()
        micGranted = hasMic()
        // The panel is black whatever the system theme: the navigation bar's handle and buttons must be light.
        window.window?.insetsController?.setSystemBarsAppearance(
            0,
            WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS,
        )
        Psst.addListener(stateListener)
        Recording.addListener(this)
        Psst.historyListener = historyListener
        Psst.savedListener = onSaved
    }

    override fun onDestroy() {
        // Like hiding the keyboard: what was said is still sent.
        Recording.stop()
        main.removeCallbacksAndMessages(null)
        Psst.removeListener(stateListener)
        Recording.removeListener(this)
        if (Psst.finalConsumer === consumeFinal) Psst.finalConsumer = null
        if (Psst.historyListener === historyListener) Psst.historyListener = null
        if (Psst.savedListener === onSaved) Psst.savedListener = null
        view = null
        super.onDestroy()
    }

    override fun onCreateInputView(): View {
        val view = DictationView(ContextThemeWrapper(this, R.style.Theme_Psst))
        view.actions = this
        currentInputEditorInfo?.let(view::setEditor)
        this.view = view
        render()
        return view
    }

    // Landscape would otherwise get the full-screen extract editor instead of the panel.
    override fun onEvaluateFullscreenMode() = false

    override fun onStartInput(editor: EditorInfo?, restarting: Boolean) {
        super.onStartInput(editor, restarting)
        field++
    }

    override fun onStartInputView(editor: EditorInfo, restarting: Boolean) {
        super.onStartInputView(editor, restarting)
        inputViewStarted = true
        view?.setEditor(editor)
        if (opening) {
            opening = false
            Psst.warm()
            micGranted = hasMic()
            notice = null
            main.removeCallbacks(clearNotice)
            takeOver(Handoff.take())
        }
        view?.switchable = returning || shouldOfferSwitchingToNextInputMethod()
        render()
    }

    /** Takes over from the bubble, if it opened the panel. */
    private fun takeOver(handoff: Handoff.Request?) {
        returnTo = handoff
        if (handoff == null) return
        if (Recording.active || Psst.state.dictation != Dictation.IDLE) {
            // The bubble's dictation, which it started in this field.
            Psst.finalConsumer = consumeFinal
            Psst.savedListener = onSaved
            dictationField = field
        }
        if (handoff.insertPending && insertPending()) main.post { leave() }
    }

    override fun onFinishInputView(finishingInput: Boolean) {
        inputViewStarted = false
        super.onFinishInputView(finishingInput)
    }

    override fun onWindowHidden() {
        super.onWindowHidden()
        opening = true
        showingHistory = false
        confirmingDiscard = null
        // Hiding is not cancelling: the recording is sent as usual.
        Recording.stop()
        // The panel has shown it.
        Recording.forget()
        // The next field is for the keyboard the bubble switched from.
        if (returning) leave()
        render()
    }

    override fun onRecordingChanged() = render()

    override fun onLevel(level: Float) {
        view?.level(level)
    }

    override fun onLimit() = tick()

    private fun start() {
        Recording.start()
        Psst.finalConsumer = consumeFinal
        Psst.savedListener = onSaved
        dictationField = field
        notice = null
        main.removeCallbacks(clearNotice)
        tick()
    }

    override fun onMic() {
        when {
            Recording.stop() -> tick()
            Recording.active -> Unit
            else -> {
                micGranted = hasMic()
                if (Recording.canStart(micGranted)) start() else render()
            }
        }
    }

    override fun onCancel() {
        Recording.cancel()
        if (returning) leave()
    }

    override fun onSwitchKeyboard() = leave()

    /** Deletes the selection, else the word before the cursor: dictation is fixed a word at a time. */
    override fun onBackspace() {
        val connection = currentInputConnection ?: return
        if (!connection.getSelectedText(0).isNullOrEmpty()) {
            connection.commitText("", 1)
            return
        }
        val word = lastWord(connection.getTextBeforeCursor(WORD_LOOKBACK, 0))
        // A field that doesn't say what it holds (a terminal) still takes the key.
        if (word > 0) connection.deleteSurroundingText(word, 0) else sendDownUpKeyEvents(KeyEvent.KEYCODE_DEL)
    }

    override fun onEnter() = sendKeyChar('\n')

    override fun onAction(action: Action) {
        when (action) {
            Action.SET_UP, Action.ALLOW_MIC, Action.OPEN -> startActivity(
                Intent(this, SetupActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    .putExtra(SetupActivity.ASK_MICROPHONE, action == Action.ALLOW_MIC),
            )
            Action.INSERT -> if (insertPending() && returning) leave()
            Action.DISMISS -> {
                Psst.dismissPendingFinal()
                if (returning) leave()
            }
        }
        render()
    }

    private fun notify(text: Int, tone: Tone) {
        notice = Notice(text, tone)
        main.removeCallbacks(clearNotice)
        main.postDelayed(clearNotice, NOTICE_MS)
        render()
    }

    private fun onFinal(final: Final): Boolean {
        if (final.text.isEmpty()) {
            notify(R.string.ime_nothing_heard, Tone.MUTED)
            return true
        }
        if (field != dictationField || !insert(final)) {
            // Psst keeps it, and the panel offers it: at once in another field, else once it is open on one again.
            main.post { render() }
            return false
        }
        if (returning) leave()
        return true
    }

    override fun onHistory() {
        showingHistory = true
        history = null
        confirmingDiscard = null
        Psst.history()
        render()
    }

    override fun onHistoryBack() {
        showingHistory = false
        render()
    }

    override fun onHistoryPick(entry: DictationView.Entry) {
        // One still waiting for the server has nothing to insert yet.
        val text = entry.text ?: return
        confirmingDiscard = null
        if (type(text)) {
            showingHistory = false
            if (entry.saved) saved(entry)?.let(Psst::inserted)
            if (returning) leave()
        }
        render()
    }

    /** Only its audio is left while it waits for the server, gone for good: that takes a second tap. */
    override fun onHistoryDiscard(entry: DictationView.Entry) {
        if (entry.text == null && confirmingDiscard != entry.sessionId) {
            confirmingDiscard = entry.sessionId
        } else {
            confirmingDiscard = null
            saved(entry)?.let(Psst::discard)
        }
        render()
    }

    private fun saved(entry: DictationView.Entry) = Psst.state.saved.find { it.sessionId == entry.sessionId }

    private fun insertPending(): Boolean {
        val final = Psst.pendingFinal ?: return false
        if (!insert(final)) return false
        Psst.takePendingFinal()
        return true
    }

    private fun insert(final: Final): Boolean {
        if (!type(final.text)) return false
        Psst.inserted(final)
        return true
    }

    /** Types [text] into the field the panel is open on, if it is. */
    private fun type(text: String): Boolean {
        val connection = currentInputConnection
        if (!inputViewStarted || !currentInputStarted || connection == null) return false
        return connection.commitText(spaced(connection.getTextBeforeCursor(2, 0), text), 1)
    }

    /**
     * Back to the keyboard the bubble switched from, in the language it was in, else to the last
     * one used. The bubble's switch doesn't always make that the last one.
     */
    private fun leave() {
        val back = returnTo
        returnTo = null
        val keyboard = back?.keyboard
        val enabled = getSystemService(InputMethodManager::class.java).enabledInputMethodList
        if (keyboard != null && enabled.any { it.id == keyboard }) {
            switchInputMethod(keyboard, back.subtype)
        } else if (!switchToPreviousInputMethod()) {
            switchToNextInputMethod(false)
        }
    }

    private fun render() {
        val view = view ?: return
        view.unused = Psst.state.saved.any { it.text != null }
        view.show(ui(Psst.state))
    }

    private fun ui(state: State): Ui {
        if (Recording.active) {
            if (Recording.listening) return recording(state, listening = true)
            // Until the core takes the stop and says what comes next.
            return busy(if (state.dictationMessage == null) R.string.ime_transcribing else R.string.ime_sending)
        }
        if (showingHistory && state.dictation == Dictation.IDLE) return historyUi()
        return when (state.dictation) {
            Dictation.IDLE -> idle(state)
            // Just cancelled, the core about to say so. Or stuck: the button stops it.
            Dictation.RECORDING -> recording(state, listening = false)
            Dictation.TRANSCRIBING -> busy(R.string.ime_transcribing)
            Dictation.LOADING_MODEL -> busy(R.string.ime_loading_model)
            Dictation.SENDING -> busy(R.string.ime_sending)
            Dictation.WAITING_FOR_SERVER -> busy(R.string.ime_waiting, Tone.WARN)
        }
    }

    /** The saved dictations come from the core at once, the rest from the server once it answers. */
    private fun historyUi(): Ui {
        val now = System.currentTimeMillis()
        val saved = Psst.state.saved.map {
            val time = if (it.text == null) "${time(it.createdMs, now)} · ${length(it.durationMs)}" else time(it.createdMs, now)
            DictationView.Entry(it.sessionId, it.text, time, saved = true, confirming = it.sessionId == confirmingDiscard)
        }
        val history = history ?: return Ui.History(saved, emptyList(), getString(R.string.ime_history_loading), Tone.MUTED)
        history.error?.let { return Ui.History(saved, emptyList(), it, Tone.WARN) }
        val entries = history.items.filter { item -> saved.none { it.sessionId == item.sessionId } }
            .map { DictationView.Entry(it.sessionId, it.text, time(it.createdMs, now)) }
        if (entries.isEmpty() && saved.isEmpty()) {
            return Ui.History(saved, entries, getString(R.string.ime_history_empty), Tone.MUTED)
        }
        return Ui.History(saved, entries, null, Tone.MUTED)
    }

    private fun time(createdMs: Long, now: Long): CharSequence = if (now - createdMs < DateUtils.MINUTE_IN_MILLIS) {
        getString(R.string.ime_history_just_now)
    } else {
        DateUtils.getRelativeTimeSpanString(createdMs, now, DateUtils.MINUTE_IN_MILLIS)
    }

    private fun recording(state: State, listening: Boolean): Ui {
        val left = state.secondsLeft
        val message = state.dictationMessage
        return when {
            left != null -> Ui.Dial(
                Mic.STOP,
                resources.getQuantityString(R.plurals.ime_stops_in, left, left),
                Tone.WARN,
                listening,
            )
            message != null -> Ui.Dial(Mic.STOP, message, Tone.WARN, listening)
            else -> Ui.Dial(Mic.STOP, getString(R.string.ime_listening), Tone.MUTED, listening)
        }
    }

    private fun busy(status: Int, tone: Tone = Tone.MUTED) = Ui.Dial(Mic.BUSY, getString(status), tone)

    private fun idle(state: State): Ui {
        Psst.pendingFinal?.let {
            return Ui.Message(getString(R.string.ime_pending, it.text.trim()), Tone.PLAIN, Action.INSERT, Action.DISMISS)
        }
        if (Recording.startFailed) return Ui.Message(getString(R.string.ime_start_failed), Tone.WARN, Action.OPEN)
        when (state.link) {
            Link.UNCONFIGURED -> return Ui.Message(getString(R.string.ime_unconfigured), Tone.PLAIN, Action.SET_UP)
            Link.NEEDS_PAIRING, Link.UPDATE_APP, Link.UPDATE_SERVER ->
                return Ui.Message(state.linkMessage.orEmpty(), Tone.WARN, Action.OPEN)
            else -> {}
        }
        if (!micGranted) return Ui.Message(getString(R.string.ime_no_mic), Tone.PLAIN, Action.ALLOW_MIC)
        Recording.failure?.let { return Ui.Dial(Mic.START, getString(it), Tone.DANGER) }
        notice?.let { return Ui.Dial(Mic.START, getString(it.text), it.tone) }
        return when (state.link) {
            Link.ONLINE -> Ui.Dial(Mic.START, getString(R.string.ime_tap_to_speak), Tone.MUTED)
            Link.OFFLINE -> Ui.Dial(Mic.START, state.linkMessage ?: getString(R.string.ime_offline), Tone.WARN)
            else -> Ui.Dial(Mic.START, getString(R.string.ime_connecting), Tone.MUTED)
        }
    }

    private fun tick() {
        view?.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
    }

    private class Notice(val text: Int, val tone: Tone)
}

/** What the bubble asks of the panel it switches to. */
object Handoff {
    /**
     * With [insertPending], the panel types the final the bubble could not. [keyboard] and
     * [subtype] are what to switch back to.
     */
    class Request(val insertPending: Boolean, val keyboard: String?, val subtype: InputMethodSubtype?)

    private var request: Request? = null
    private var requestedAt = 0L

    fun request(request: Request) {
        this.request = request
        requestedAt = SystemClock.uptimeMillis()
    }

    fun cancel() {
        request = null
    }

    /** The bubble's request, if it made one just before this opening of the panel. */
    fun take(): Request? {
        val taken = request?.takeIf { SystemClock.uptimeMillis() - requestedAt < HANDOFF_MS }
        request = null
        return taken
    }
}

/** How many characters end [before] with its last word and the whitespace after it. */
private fun lastWord(before: CharSequence?): Int {
    if (before.isNullOrEmpty()) return 0
    var start = before.length
    while (start > 0 && before[start - 1].isWhitespace()) start--
    while (start > 0 && !before[start - 1].isWhitespace()) start--
    return before.length - start
}

/** A switch of keyboards takes well under a second; this is for one that doesn't happen. */
private const val HANDOFF_MS = 3000L
private const val NOTICE_MS = 2500L
private const val WORD_LOOKBACK = 64

/** A recording's length, as minutes and seconds. */
private fun length(durationMs: Long): String {
    val seconds = durationMs / 1000
    return "%d:%02d".format(seconds / 60, seconds % 60)
}
