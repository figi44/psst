package io.github.figi44.psst

import android.Manifest
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Bundle
import android.provider.Settings
import android.text.Editable
import android.text.TextWatcher
import android.view.KeyEvent
import android.view.View
import android.view.WindowInsets
import android.view.inputmethod.InputMethodManager
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import java.text.NumberFormat

/**
 * The launcher activity: a checklist that connects psst to its server and to the phone. What it
 * shows follows [Psst.state] and what the system says about the microphone and the keyboards.
 */
class SetupActivity : Activity() {
    private lateinit var imm: InputMethodManager
    private lateinit var server: EditText
    private lateinit var serverStatus: TextView
    private lateinit var connect: View
    private lateinit var pairGroup: View
    private lateinit var code: EditText
    private lateinit var micStatus: TextView
    private lateinit var micAllow: Button
    private lateinit var keyboardStatus: TextView
    private lateinit var keyboardEnable: View
    private lateinit var keyboardSwitch: View
    private lateinit var tryStatus: TextView
    private lateinit var tryField: EditText
    private lateinit var serverStep: Step
    private lateinit var micStep: Step
    private lateinit var keyboardStep: Step
    private lateinit var tryStep: Step

    private var micGranted = false

    /** Android no longer shows the permission dialog, so only the app's settings page can grant it. */
    private var micBlocked = false
    private var keyboardOn = false
    private var keyboardInUse = false

    /** Pair was pressed: the core says `connecting` until the code is redeemed or refused. */
    private var pairing = false

    /** The server as the field last showed it. Text that differs is the user's, and stays. */
    private var shownServer: String? = null

    private val onState: (State) -> Unit = { state ->
        if (state.link != Link.CONNECTING) pairing = false
        render()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.setup_activity)
        imm = getSystemService(InputMethodManager::class.java)

        findViewById<View>(android.R.id.content).setOnApplyWindowInsetsListener { content, insets ->
            val bars = insets.getInsets(WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout())
            val keyboard = insets.getInsets(WindowInsets.Type.ime())
            // Around the scroll view, so that it shrinks above the keyboard and scrolls the focused
            // field back into sight. Being a scroll container, it also stops Android panning instead.
            content.setPadding(bars.left, bars.top, bars.right, maxOf(bars.bottom, keyboard.bottom))
            WindowInsets.CONSUMED
        }

        server = findViewById(R.id.setup_server)
        serverStatus = findViewById(R.id.setup_server_status)
        connect = findViewById(R.id.setup_connect)
        pairGroup = findViewById(R.id.setup_pair_group)
        code = findViewById(R.id.setup_code)
        micStatus = findViewById(R.id.setup_mic_status)
        micAllow = findViewById(R.id.setup_mic_allow)
        keyboardStatus = findViewById(R.id.setup_keyboard_status)
        keyboardEnable = findViewById(R.id.setup_keyboard_enable)
        keyboardSwitch = findViewById(R.id.setup_keyboard_switch)
        tryStatus = findViewById(R.id.setup_try_status)
        tryField = findViewById(R.id.setup_try)
        serverStep = step(R.id.setup_server_mark, R.id.setup_server_title, 1)
        micStep = step(R.id.setup_mic_mark, R.id.setup_mic_title, 2)
        keyboardStep = step(R.id.setup_keyboard_mark, R.id.setup_keyboard_title, 3)
        tryStep = step(R.id.setup_try_mark, R.id.setup_try_title, 4)

        server.onSubmit(::connect)
        connect.setOnClickListener { connect() }
        code.onSubmit(::pair)
        findViewById<View>(R.id.setup_pair).setOnClickListener { pair() }
        micAllow.setOnClickListener { allowMicrophone() }
        keyboardEnable.setOnClickListener { open(Settings.ACTION_INPUT_METHOD_SETTINGS) }
        keyboardSwitch.setOnClickListener { imm.showInputMethodPicker() }
        tryField.addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) = Unit

            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) = Unit

            override fun afterTextChanged(s: Editable?) = renderTry()
        })


        micBlocked = savedInstanceState?.getBoolean(MIC_BLOCKED) == true
        readSystem()
        if (savedInstanceState == null) followIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        followIntent(intent)
    }

    override fun onStart() {
        super.onStart()
        Psst.addListener(onState)
        Psst.warm()
    }

    override fun onResume() {
        super.onResume()
        readSystem()
        render()
    }

    // The keyboard picker is a dialog over this window: no onResume when it closes.
    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) {
            readSystem()
            render()
        }
    }

    override fun onStop() {
        Psst.removeListener(onState)
        super.onStop()
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        outState.putBoolean(MIC_BLOCKED, micBlocked)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        if (requestCode != MICROPHONE || grantResults.isEmpty()) return
        // Denied with no rationale to show: Android answers every further request itself.
        micBlocked = grantResults[0] != PackageManager.PERMISSION_GRANTED &&
            !shouldShowRequestPermissionRationale(Manifest.permission.RECORD_AUDIO)
        readSystem()
        render()
    }

    private fun readSystem() {
        micGranted = checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
        if (micGranted) micBlocked = false
        val ime = imm.enabledInputMethodList.find { it.packageName == packageName }
        keyboardOn = ime != null
        keyboardInUse = ime != null &&
            ime.id == Settings.Secure.getString(contentResolver, Settings.Secure.DEFAULT_INPUT_METHOD)
    }

    private fun render() {
        val state = Psst.state
        syncServer(state.server)
        renderServer(state)
        renderMicrophone()
        renderKeyboard()
        renderTry()
    }

    private fun syncServer(current: String?) {
        val text = server.text.toString()
        if (text != shownServer.orEmpty() && text != current.orEmpty()) return
        if (text != current.orEmpty()) {
            server.setText(current)
            server.setSelection(server.length())
        }
        shownServer = current
    }

    private fun renderServer(state: State) {
        val (text, color) = when (state.link) {
            Link.UNCONFIGURED -> getString(R.string.setup_server_unconfigured) to R.color.muted
            Link.CONNECTING ->
                getString(if (pairing) R.string.setup_pair_checking else R.string.setup_server_connecting) to R.color.muted
            Link.ONLINE -> {
                val version = state.serverVersion
                val text = if (version == null) {
                    getString(R.string.setup_server_online)
                } else {
                    getString(R.string.setup_server_online_version, version)
                }
                text to R.color.ok
            }
            Link.OFFLINE, Link.NEEDS_PAIRING ->
                (state.linkMessage ?: getString(R.string.setup_server_offline)) to R.color.warn
            Link.UPDATE_APP, Link.UPDATE_SERVER ->
                (state.linkMessage ?: getString(R.string.setup_server_offline)) to R.color.danger
        }
        serverStatus.show(text, color)
        val showCode = state.link == Link.NEEDS_PAIRING || (pairing && state.link == Link.CONNECTING)
        pairGroup.visibility = if (showCode) View.VISIBLE else View.GONE
        if (state.link == Link.ONLINE && code.length() > 0) code.text.clear()
        connect.isActivated = state.link != Link.ONLINE
        serverStep.show(state.link == Link.ONLINE)
    }

    private fun renderMicrophone() {
        when {
            micGranted -> micStatus.show(getString(R.string.setup_mic_granted), R.color.muted)
            micBlocked -> micStatus.show(getString(R.string.setup_mic_blocked), R.color.warn)
            else -> micStatus.show(getString(R.string.setup_mic_why), R.color.muted)
        }
        micAllow.visibility = if (micGranted) View.GONE else View.VISIBLE
        micAllow.setText(if (micBlocked) R.string.setup_mic_settings else R.string.setup_mic_allow)
        micStep.show(micGranted)
    }

    private fun renderKeyboard() {
        val status = when {
            !keyboardOn -> R.string.setup_keyboard_off
            keyboardInUse -> R.string.setup_keyboard_in_use
            else -> R.string.setup_keyboard_on
        }
        keyboardStatus.show(getString(status), R.color.muted)
        keyboardEnable.visibility = if (keyboardOn) View.GONE else View.VISIBLE
        keyboardSwitch.visibility = if (keyboardOn) View.VISIBLE else View.GONE
        keyboardStep.show(keyboardOn)
    }

    private fun renderTry() {
        val status = if (keyboardInUse) R.string.setup_try_speak else R.string.setup_try_switch
        tryStatus.show(getString(status), R.color.muted)
        tryStep.show(tryField.length() > 0)
    }

    private fun connect() {
        val address = server.text.toString()
        if (address.isBlank()) return ask(server)
        // The core answers with the address in its canonical form, which then replaces this.
        shownServer = address
        Psst.configure(address)
        done(server)
    }

    private fun pair() {
        val text = code.text.toString()
        if (text.isBlank()) return ask(code)
        pairing = true
        Psst.pair(text.trim())
        done(code)
    }

    private fun allowMicrophone() {
        if (micBlocked) {
            open(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", packageName, null))
        } else {
            requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), MICROPHONE)
        }
    }

    private fun followIntent(intent: Intent) {
        if (intent.getBooleanExtra(ASK_MICROPHONE, false) && !micGranted) allowMicrophone()
    }

    private fun open(action: String, data: Uri? = null) {
        try {
            startActivity(Intent(action, data))
        } catch (e: ActivityNotFoundException) {
            startActivity(Intent(Settings.ACTION_SETTINGS))
        }
    }

    private fun ask(field: EditText) {
        field.requestFocus()
        field.post { imm.showSoftInput(field, 0) }
    }

    private fun done(field: EditText) {
        imm.hideSoftInputFromWindow(field.windowToken, 0)
        field.clearFocus()
    }

    // Setting the same text again would have TalkBack read out a live region again.
    private fun TextView.show(text: String, color: Int) {
        if (this.text.toString() != text) this.text = text
        setTextColor(getColor(color))
    }

    private fun step(mark: Int, title: Int, number: Int): Step {
        val step = Step(findViewById(mark), findViewById(title))
        step.mark.text = NumberFormat.getIntegerInstance().format(number)
        return step
    }

    private inner class Step(val mark: TextView, val title: TextView) {
        fun show(done: Boolean) {
            mark.isActivated = done
            title.stateDescription = getString(if (done) R.string.setup_done else R.string.setup_not_done)
        }
    }

    companion object {
        /** A boolean extra: ask for the microphone at once, as when the keyboard sends the user here. */
        const val ASK_MICROPHONE = "io.github.figi44.psst.ASK_MICROPHONE"

        private const val MICROPHONE = 1
        private const val MIC_BLOCKED = "mic_blocked"
    }
}

/** The IME action and the Enter key both [submit]. */
private fun EditText.onSubmit(submit: () -> Unit) = setOnEditorActionListener { _, _, event ->
    if (event == null || event.action == KeyEvent.ACTION_DOWN) submit()
    true
}
