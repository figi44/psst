package io.github.figi44.psst

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Handler
import android.os.Looper
import java.util.EnumSet

/**
 * The microphone, for whichever surface dictates: the keyboard's panel or the bubble. It records
 * while the core's dictation is `recording`. Belongs to the main thread.
 */
object Recording : Recorder.Events {
    interface Listener {
        fun onRecordingChanged()

        /** 0 to 1, smoothed, while [listening]. */
        fun onLevel(level: Float)

        /** The core ended the recording itself, at the limit. */
        fun onLimit()
    }

    private val main = Handler(Looper.getMainLooper())
    private val listeners = mutableListOf<Listener>()
    private var recorder: Recorder? = null

    /** The core has said `recording` since [recorder] started. */
    private var confirmed = false
    private val startTimeout = Runnable { onStartTimeout() }

    /** The microphone is open, from [start] to [stop]. */
    val listening get() = recorder?.stopping == false

    /** [listening], or sending what it heard last. */
    val active get() = recorder != null

    /** The core ignored the last start. Until [forget]. */
    var startFailed = false
        private set

    /** Why the microphone did not open, as a string. Until [forget] or the next start. */
    var failure: Int? = null
        private set

    init {
        Psst.addListener(::onCoreState)
    }

    fun addListener(listener: Listener) {
        listeners += listener
    }

    fun removeListener(listener: Listener) {
        listeners -= listener
    }

    fun canStart(micGranted: Boolean, state: State = Psst.state) = recorder == null && micGranted &&
        !startFailed && Psst.pendingFinal == null && state.dictation == Dictation.IDLE && state.server != null &&
        state.link !in BLOCKING

    fun start() {
        Psst.startDictation()
        // With the command, not on the core's answer to it: the first word is not lost to a round trip.
        recorder = Recorder(this).also { it.start() }
        confirmed = false
        failure = null
        main.postDelayed(startTimeout, START_TIMEOUT_MS)
        changed()
    }

    /** Sends what was heard, then the stop. False if there was nothing to stop. */
    fun stop(): Boolean {
        val active = recorder
        if (active != null) {
            if (active.stopping) return false
            active.stop()
            changed()
            return true
        }
        // Recording with no recorder here: a dictation the app has let go of.
        if (Psst.state.dictation != Dictation.RECORDING) return false
        Psst.stopDictation()
        return true
    }

    fun cancel() {
        recorder?.cancel()
        recorder = null
        main.removeCallbacks(startTimeout)
        Psst.cancelDictation()
        changed()
    }

    /** What the last start left to say has been said. */
    fun forget() {
        if (!startFailed && failure == null) return
        startFailed = false
        failure = null
        changed()
    }

    private fun onCoreState(state: State) {
        val active = recorder ?: return
        if (state.dictation == Dictation.RECORDING) {
            confirmed = true
            main.removeCallbacks(startTimeout)
        } else if (confirmed) {
            // Either it took our stop, or it ended the recording at the limit: either way the
            // microphone is done, and must not follow the core's stop with more audio.
            if (!active.stopping) for (listener in listeners.toList()) listener.onLimit()
            active.cancel()
            recorder = null
            changed()
        }
    }

    // The core ignores a start it cannot take (a server address that doesn't parse), and then says
    // nothing. The cancel settles it should the core take it after all, so the two can't disagree.
    private fun onStartTimeout() {
        val active = recorder ?: return
        active.cancel()
        recorder = null
        startFailed = true
        Psst.cancelDictation()
        changed()
    }

    override fun onLevel(recorder: Recorder, level: Float) {
        if (recorder !== this.recorder) return
        for (listener in listeners.toList()) listener.onLevel(level)
    }

    override fun onFailed(recorder: Recorder, message: Int) {
        if (recorder !== this.recorder) return
        this.recorder = null
        main.removeCallbacks(startTimeout)
        Psst.cancelDictation()
        failure = message
        changed()
    }

    private fun changed() {
        for (listener in listeners.toList()) listener.onRecordingChanged()
    }
}

fun Context.hasMic() = checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

/** The states in which a surface shows what to fix instead of starting. */
val BLOCKING: EnumSet<Link> = EnumSet.of(Link.UNCONFIGURED, Link.NEEDS_PAIRING, Link.UPDATE_APP, Link.UPDATE_SERVER)

/** The core answers a start in milliseconds; this is for when it ignores one. */
private const val START_TIMEOUT_MS = 2000L
