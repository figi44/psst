package io.github.figi44.psst

object Native {
    init { System.loadLibrary("psst_android") }

    /** Starts the core once per process; later calls do nothing. `dataDir` is app-private. */
    external fun start(dataDir: String, deviceName: String)

    /** One JSON command (below). Never blocks. */
    external fun send(command: String)

    /** `count` samples of the recording in progress: 16 kHz mono. Any chunk size. Never blocks. */
    external fun audio(samples: ShortArray, count: Int)

    /** Blocks until the core has an event, and returns it as JSON (below). */
    external fun nextEvent(): String

    external fun version(): String
}
