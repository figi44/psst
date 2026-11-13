package io.github.figi44.psst

import android.content.Context
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.util.Log
import kotlin.concurrent.thread
import org.json.JSONException
import org.json.JSONObject

enum class Link {
    UNCONFIGURED,
    CONNECTING,
    ONLINE,
    OFFLINE,
    NEEDS_PAIRING,
    UPDATE_APP,
    UPDATE_SERVER,
}

enum class Dictation {
    IDLE,
    RECORDING,
    TRANSCRIBING,
    LOADING_MODEL,
    SENDING,
    WAITING_FOR_SERVER,
    FAILED,
}

data class State(
    val server: String?,
    val serverVersion: String?,
    val link: Link,
    val linkMessage: String?,
    val dictation: Dictation,
    val dictationMessage: String?,
    val secondsLeft: Int?,
)

/** Text for a field: one dictation's, or several joined when none could be inserted in between. */
class Final internal constructor(val text: String, internal val sessionIds: List<String>)

class HistoryItem(val text: String, val createdMs: Long)

/** This device's earlier dictations as the server has them, newest first, or [error]: why it couldn't say. */
class History(val items: List<HistoryItem>, val error: String?)

/**
 * The only way to the core. Commands never block and may be sent from any thread. Everything else
 * belongs to the main thread, which is where listeners and the final consumer are called.
 */
object Psst {
    private const val TAG = "psst"

    private val main = Handler(Looper.getMainLooper())
    private val listeners = mutableListOf<(State) -> Unit>()
    private var started = false

    /** `connecting` until the core's first event, which comes right after it starts. */
    var state = State(null, null, Link.CONNECTING, null, Dictation.IDLE, null, null)
        private set

    /**
     * Gets every final while set. Returning false leaves the final pending, as when there is no
     * field to put it in; so does having no consumer. An empty final is never pending.
     */
    var finalConsumer: ((Final) -> Boolean)? = null

    var pendingFinal: Final? = null
        private set

    /** Gets the answer to each [history]. */
    var historyListener: ((History) -> Unit)? = null

    fun start(context: Context) {
        if (started) return
        started = true
        // No backup: a token restored onto another device would let that device pass for this one.
        Native.start(context.noBackupFilesDir.path, deviceName(context))
        thread(name = "psst-events", isDaemon = true) {
            while (true) receive(Native.nextEvent())
        }
    }

    /** Calls [listener] with the current state at once, and again on every change. */
    fun addListener(listener: (State) -> Unit) {
        listeners += listener
        listener(state)
    }

    fun removeListener(listener: (State) -> Unit) {
        listeners -= listener
    }

    fun takePendingFinal(): Final? = pendingFinal.also { pendingFinal = null }

    fun dismissPendingFinal() {
        pendingFinal = null
    }

    fun configure(server: String) = send("configure") { put("server", server) }

    fun pair(code: String) = send("pair") { put("code", code) }

    fun warm() = send("warm")

    fun startDictation() = send("start")

    fun stopDictation() = send("stop")

    fun cancelDictation() = send("cancel")

    fun retryDictation() = send("retry")

    fun discardDictation() = send("discard")

    /** [final]'s text is in a field. */
    fun inserted(final: Final) {
        for (id in final.sessionIds) send("inserted") { put("session_id", id) }
    }

    fun history() = send("history")

    fun audio(samples: ShortArray, count: Int) = Native.audio(samples, count)

    private inline fun send(type: String, fields: JSONObject.() -> Unit = {}) {
        Native.send(JSONObject().put("type", type).apply(fields).toString())
    }

    private fun receive(json: String) {
        try {
            val event = JSONObject(json)
            when (val type = event.getString("type")) {
                "state" -> parseState(event.getJSONObject("state")).let { main.post { update(it) } }
                "final" -> {
                    val final = Final(event.getString("text"), listOf(event.getString("session_id")))
                    main.post { deliver(final) }
                }
                "history" -> {
                    val items = event.getJSONArray("items")
                    val history = History(
                        List(items.length()) {
                            val item = items.getJSONObject(it)
                            HistoryItem(item.getString("text"), item.getLong("created_ms"))
                        },
                        event.stringOrNull("error"),
                    )
                    main.post { historyListener?.invoke(history) }
                }
                else -> Log.w(TAG, "Ignoring an event of type $type")
            }
        } catch (e: JSONException) {
            Log.e(TAG, "Ignoring a malformed event: $json", e)
        }
    }

    private fun parseState(state: JSONObject) = State(
        server = state.stringOrNull("server"),
        serverVersion = state.stringOrNull("server_version"),
        link = state.enum("link"),
        linkMessage = state.stringOrNull("link_message"),
        dictation = state.enum("dictation"),
        dictationMessage = state.stringOrNull("dictation_message"),
        secondsLeft = if (state.isNull("seconds_left")) null else state.getInt("seconds_left"),
    )

    private fun update(new: State) {
        if (new == state) return
        state = new
        for (listener in listeners.toList()) listener(new)
    }

    private fun deliver(final: Final) {
        if (finalConsumer?.invoke(final) == true || final.text.isEmpty()) return
        val pending = pendingFinal
        // Nothing said is lost: a second final with nowhere to go joins the first.
        pendingFinal = if (pending == null) {
            final
        } else {
            Final("${pending.text} ${final.text}", pending.sessionIds + final.sessionIds)
        }
    }

    private fun deviceName(context: Context): String =
        Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME) ?: Build.MODEL
}

// optString would turn a JSON null into "null".
private fun JSONObject.stringOrNull(name: String): String? = if (isNull(name)) null else getString(name)

private inline fun <reified E : Enum<E>> JSONObject.enum(name: String): E {
    val value = getString(name)
    return enumValues<E>().find { it.name.equals(value, ignoreCase = true) }
        ?: throw JSONException("$name has no value $value")
}
