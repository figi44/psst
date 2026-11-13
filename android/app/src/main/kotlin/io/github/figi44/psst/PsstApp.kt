package io.github.figi44.psst

import android.app.Application

class PsstApp : Application() {
    override fun onCreate() {
        super.onCreate()
        Psst.start(this)
    }
}
