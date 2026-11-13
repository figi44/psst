package io.github.figi44.psst

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.inputmethodservice.InputMethodService
import android.os.Handler
import android.os.Looper
import android.text.format.DateUtils
import android.view.ContextThemeWrapper
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.View
import android.view.WindowInsetsController
import android.view.inputmethod.EditorInfo
import java.util.EnumSet
import io.github.figi44.psst.DictationView.Action
import io.github.figi44.psst.DictationView.Mic
import io.github.figi44.psst.DictationView.Tone
import io.github.figi44.psst.DictationView.Ui

/**
 * The keyboard: a dictation panel. It records while the core's dictation is `recording`, and
 * types each final into the field it serves.
 */
class PsstIme : InputMethodService(), DictationView.Actions, Recorder.Events {
    private val main = Handler(Looper.getMainLooper())
    private var view: DictationView? = null

    private var recorder: Recorder? = null

    /** The core has said `recording` since [recorder] started. */
    private var confirmed = false

    /**
     * Counts the inputs served: a final is typed only into the one its dictation started in. A
     * restarted input counts too, because that is also how a web page moves between its fields.
     */
    private var field = 0
    private var dictationField = 0

    /** The window was hidden: the next [onStartInputView] opens the panel. */
    private var opening = true

    private var inputViewStarted = false
    private var micGranted = false
    private var startFailed = false
    private var notice: Notice? = null

    /** The panel shows the history: what the server said of it, or nothing yet. */
    private var showingHistory = false
    private var history: History? = null

    private val stateListener: (State) -> Unit = { onCoreState(it) }
    private val consumeFinal: (Final) -> Boolean = { onFinal(it) }
    private val historyListener: (History) -> Unit = {
        history = it
        render()
    }
    private val startTimeout = Runnable { onStartTimeout() }
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
        Psst.finalConsumer = consumeFinal
        Psst.historyListener = historyListener
    }

    override fun onDestroy() {
        // Like hiding the keyboard: what was said is still sent.
        recorder?.stop()
        recorder = null
        main.removeCallbacksAndMessages(null)
        Psst.removeListener(stateListener)
        if (Psst.finalConsumer === consumeFinal) Psst.finalConsumer = null
        if (Psst.historyListener === historyListener) Psst.historyListener = null
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
            startFailed = false
            notice = null
            main.removeCallbacks(clearNotice)
        }
        render()
    }

    override fun onFinishInputView(finishingInput: Boolean) {
        inputViewStarted = false
        super.onFinishInputView(finishingInput)
    }

    override fun onWindowHidden() {
        super.onWindowHidden()
        opening = true
        showingHistory = false
        // Hiding is not cancelling: the recording is sent as usual.
        recorder?.stop()
        render()
    }

    private fun onCoreState(state: State) {
        val active = recorder
        if (active != null) {
            if (state.dictation == Dictation.RECORDING) {
                confirmed = true
                main.removeCallbacks(startTimeout)
            } else if (confirmed) {
                // Either it took our stop, or it ended the recording at the limit: either way the
                // microphone is done, and must not follow the core's stop with more audio.
                if (!active.stopping) tick()
                active.cancel()
                recorder = null
            }
        }
        render()
    }

    private fun canStart(state: State) = recorder == null && micGranted && !startFailed &&
        Psst.pendingFinal == null && state.dictation == Dictation.IDLE && state.server != null &&
        state.link !in BLOCKING

    private fun start() {
        Psst.startDictation()
        // With the command, not on the core's answer to it: the first word is not lost to a round trip.
        recorder = Recorder(this).also { it.start() }
        confirmed = false
        dictationField = field
        notice = null
        main.removeCallbacks(clearNotice)
        main.postDelayed(startTimeout, START_TIMEOUT_MS)
        tick()
        render()
    }

    // The core ignores a start it cannot take (a server address that doesn't parse), and then says
    // nothing. The cancel settles it should the core take it after all, so the two can't disagree.
    private fun onStartTimeout() {
        val active = recorder ?: return
        active.cancel()
        recorder = null
        startFailed = true
        Psst.cancelDictation()
        render()
    }

    override fun onMic() {
        val active = recorder
        when {
            active != null -> if (!active.stopping) {
                active.stop()
                tick()
                render()
            }
            // Recording with no recorder here: a dictation this panel has let go of.
            Psst.state.dictation == Dictation.RECORDING -> {
                Psst.stopDictation()
                tick()
            }
            else -> {
                micGranted = hasMic()
                if (canStart(Psst.state)) start() else render()
            }
        }
    }

    override fun onCancel() {
        recorder?.cancel()
        recorder = null
        main.removeCallbacks(startTimeout)
        Psst.cancelDictation()
        render()
    }

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
            Action.RETRY -> Psst.retryDictation()
            Action.DISCARD -> {
                Psst.discardDictation()
            }
            Action.INSERT -> Psst.pendingFinal?.let { if (insert(it)) Psst.takePendingFinal() }
            Action.DISMISS -> Psst.dismissPendingFinal()
        }
        render()
    }

    override fun onLevel(recorder: Recorder, level: Float) {
        if (recorder === this.recorder) view?.level(level)
    }

    override fun onFailed(recorder: Recorder, message: Int) {
        if (recorder !== this.recorder) return
        this.recorder = null
        main.removeCallbacks(startTimeout)
        Psst.cancelDictation()
        notice = Notice(message, Tone.DANGER)
        main.removeCallbacks(clearNotice)
        render()
    }

    private fun onFinal(final: Final): Boolean {
        if (final.text.isEmpty()) {
            notice = Notice(R.string.ime_nothing_heard, Tone.MUTED)
            main.removeCallbacks(clearNotice)
            main.postDelayed(clearNotice, NOTICE_MS)
            render()
            return true
        }
        if (field != dictationField || !insert(final)) {
            // Psst keeps it, and the panel offers it: at once in another field, else once it is open on one again.
            main.post { render() }
            return false
        }
        return true
    }

    override fun onHistory() {
        showingHistory = true
        history = null
        Psst.history()
        render()
    }

    override fun onHistoryBack() {
        showingHistory = false
        render()
    }

    override fun onHistoryPick(entry: DictationView.Entry) {
        if (type(entry.text)) showingHistory = false
        render()
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
        val spaced = if (!text.first().isWhitespace() && needsSpace(connection.getTextBeforeCursor(2, 0))) {
            " $text"
        } else {
            text
        }
        return connection.commitText(spaced, 1)
    }

    private fun render() {
        view?.show(ui(Psst.state))
    }

    private fun ui(state: State): Ui {
        val active = recorder
        if (active != null) {
            if (!active.stopping) return recording(state, listening = true)
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
            Dictation.FAILED -> Ui.Message(
                state.dictationMessage ?: getString(R.string.ime_failed),
                Tone.DANGER,
                Action.RETRY,
                Action.DISCARD,
            )
        }
    }

    private fun historyUi(): Ui {
        val history = history ?: return Ui.History(emptyList(), getString(R.string.ime_history_loading), Tone.MUTED)
        history.error?.let { return Ui.History(emptyList(), it, Tone.WARN) }
        if (history.items.isEmpty()) return Ui.History(emptyList(), getString(R.string.ime_history_empty), Tone.MUTED)
        val now = System.currentTimeMillis()
        val entries = history.items.map {
            val time = if (now - it.createdMs < DateUtils.MINUTE_IN_MILLIS) {
                getString(R.string.ime_history_just_now)
            } else {
                DateUtils.getRelativeTimeSpanString(it.createdMs, now, DateUtils.MINUTE_IN_MILLIS)
            }
            DictationView.Entry(it.text, time)
        }
        return Ui.History(entries, null, Tone.MUTED)
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
        if (startFailed) return Ui.Message(getString(R.string.ime_start_failed), Tone.WARN, Action.OPEN)
        when (state.link) {
            Link.UNCONFIGURED -> return Ui.Message(getString(R.string.ime_unconfigured), Tone.PLAIN, Action.SET_UP)
            Link.NEEDS_PAIRING, Link.UPDATE_APP, Link.UPDATE_SERVER ->
                return Ui.Message(state.linkMessage.orEmpty(), Tone.WARN, Action.OPEN)
            else -> {}
        }
        if (!micGranted) return Ui.Message(getString(R.string.ime_no_mic), Tone.PLAIN, Action.ALLOW_MIC)
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

    private fun hasMic() = checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    private class Notice(val text: Int, val tone: Tone)
}

/**
 * Whether dictated text goes after [before], the text before the cursor, with a space: not at the
 * start of the field, after whitespace, or after an opening bracket or quote.
 */
private fun needsSpace(before: CharSequence?): Boolean {
    if (before.isNullOrEmpty()) return false
    val last = before.last()
    if (last.isWhitespace() || last in OPENERS) return false
    if (last == '"' || last == '\'') {
        // A straight quote opens when it starts a word.
        val previous = before.getOrNull(before.length - 2) ?: return false
        return !previous.isWhitespace() && previous !in OPENERS
    }
    return true
}

/** How many characters end [before] with its last word and the whitespace after it. */
private fun lastWord(before: CharSequence?): Int {
    if (before.isNullOrEmpty()) return 0
    var start = before.length
    while (start > 0 && before[start - 1].isWhitespace()) start--
    while (start > 0 && !before[start - 1].isWhitespace()) start--
    return before.length - start
}

private const val OPENERS = "([{<“‘«‹„‚¿¡"

/** The states in which the panel shows what to fix instead of the button. */
private val BLOCKING = EnumSet.of(Link.UNCONFIGURED, Link.NEEDS_PAIRING, Link.UPDATE_APP, Link.UPDATE_SERVER)

/** The core answers a start in milliseconds; this is for when it ignores one. */
private const val START_TIMEOUT_MS = 2000L
private const val NOTICE_MS = 2500L
private const val WORD_LOOKBACK = 64
