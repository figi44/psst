package io.github.figi44.psst

import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.os.Handler
import android.os.Looper
import android.os.Process
import android.util.Log
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread
import kotlin.math.log10
import kotlin.math.max
import kotlin.math.sqrt

/**
 * Records one dictation into the core, from [start] until [stop] or [cancel], on a thread of its
 * own. It sends the `stop` itself after its last chunk, so no audio follows it.
 */
class Recorder(private val events: Events) {
    /** Called on the main thread. */
    interface Events {
        /** 0 to 1, smoothed, after every chunk until [stop]. */
        fun onLevel(recorder: Recorder, level: Float)

        /** The microphone did not open. Nothing goes to the core; [message] says what to do. */
        fun onFailed(recorder: Recorder, message: Int)
    }

    private val main = Handler(Looper.getMainLooper())

    // Guards `done`, and every call into the core, so that once `done` is set nothing more reaches it:
    // the cancel the app sends after [cancel] is final.
    private val lock = Any()
    private var done = false

    /** [stop] or [cancel] was called. */
    @Volatile
    var stopping = false
        private set

    @Volatile
    private var level = 0f
    private val levelPosted = AtomicBoolean()
    private val postLevel = Runnable {
        levelPosted.set(false)
        if (!stopping) events.onLevel(this, level)
    }

    fun start() {
        thread(name = "psst-recorder") { record() }
    }

    /** Sends what the microphone has captured so far, then the stop. */
    fun stop() {
        stopping = true
    }

    /** Sends nothing more to the core, not even the stop, once this returns. */
    fun cancel() {
        synchronized(lock) { done = true }
        stopping = true
    }

    private fun record() {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
        val record = open() ?: return fail(R.string.ime_mic_unavailable)
        try {
            capture(record)
        } finally {
            record.release()
        }
    }

    private fun open(): AudioRecord? {
        val format = AudioFormat.Builder()
            .setSampleRate(SAMPLE_RATE)
            .setChannelMask(AudioFormat.CHANNEL_IN_MONO)
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .build()
        val minimum =
            AudioRecord.getMinBufferSize(SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT)
        val record = try {
            AudioRecord.Builder()
                .setAudioSource(MediaRecorder.AudioSource.VOICE_RECOGNITION)
                .setAudioFormat(format)
                .setBufferSizeInBytes(max(minimum, BUFFER_BYTES))
                .build()
        } catch (e: SecurityException) {
            Log.w(TAG, "No permission to record", e)
            return null
        } catch (e: RuntimeException) {
            Log.w(TAG, "Cannot open the microphone", e)
            return null
        }
        if (record.state != AudioRecord.STATE_INITIALIZED) {
            Log.w(TAG, "The microphone did not initialise")
            record.release()
            return null
        }
        return record
    }

    private fun capture(record: AudioRecord) {
        try {
            record.startRecording()
        } catch (e: IllegalStateException) {
            Log.w(TAG, "Cannot start recording", e)
        }
        if (record.recordingState != AudioRecord.RECORDSTATE_RECORDING) return fail(R.string.ime_mic_unavailable)
        val chunk = ShortArray(CHUNK)
        var smoothed = 0f
        while (!stopping) {
            val count = record.read(chunk, 0, CHUNK)
            if (count < 0) {
                // What was recorded until then is still worth its final.
                if (!stopping) Log.w(TAG, "Recording failed: AudioRecord.read returned $count")
                break
            }
            if (count == 0) continue
            if (!send(chunk, count)) return
            val loudness = loudness(chunk, count)
            smoothed += (loudness - smoothed) * if (loudness > smoothed) ATTACK else RELEASE
            level = smoothed
            if (levelPosted.compareAndSet(false, true)) main.post(postLevel)
        }
        // The buffer still holds what was said up to the tap.
        for (i in 0 until MAX_DRAIN) {
            val count = record.read(chunk, 0, CHUNK, AudioRecord.READ_NON_BLOCKING)
            if (count <= 0 || !send(chunk, count)) break
        }
        synchronized(lock) {
            if (!done) {
                done = true
                Psst.stopDictation()
            }
        }
        record.stop()
    }

    private fun send(chunk: ShortArray, count: Int): Boolean = synchronized(lock) {
        if (!done) Psst.audio(chunk, count)
        !done
    }

    private fun fail(message: Int) {
        val failed = synchronized(lock) { (!done).also { done = true } }
        if (failed) main.post { events.onFailed(this, message) }
    }
}

/** How loud a chunk is, from 0 (silence) to 1 (shouting into the phone). */
private fun loudness(chunk: ShortArray, count: Int): Float {
    var sum = 0L
    for (i in 0 until count) {
        val sample = chunk[i].toLong()
        sum += sample * sample
    }
    val rms = sqrt(sum.toDouble() / count) / Short.MAX_VALUE
    val db = 20 * log10(max(rms, 1e-6))
    return ((db - FLOOR_DB) / (CEILING_DB - FLOOR_DB)).toFloat().coerceIn(0f, 1f)
}

/**
 * How much a [Recorder.Events.onLevel] level should swell the recording visuals, 0 to 1, as the
 * desktop overlay's do: not at all for room noise, under about −52 dBFS, and fully at −16.
 */
fun swell(level: Float): Float {
    val db = FLOOR_DB + level * (CEILING_DB - FLOOR_DB)
    val t = ((db - QUIET_DB) / (LOUD_DB - QUIET_DB)).toFloat().coerceIn(0f, 1f)
    return t * t * (3 - 2 * t)
}

private const val TAG = "psst"
private const val SAMPLE_RATE = 16_000

/** 20 ms. */
private const val CHUNK = SAMPLE_RATE / 50

/** Half a second of 16-bit samples, so a late read loses nothing. */
private const val BUFFER_BYTES = SAMPLE_RATE
private const val MAX_DRAIN = BUFFER_BYTES / 2 / CHUNK + 1
private const val FLOOR_DB = -60.0
private const val CEILING_DB = -15.0
private const val QUIET_DB = -52.0
private const val LOUD_DB = -16.0

// Per chunk: the meter jumps up with the voice and settles back slowly.
private const val ATTACK = 0.5f
private const val RELEASE = 0.12f
