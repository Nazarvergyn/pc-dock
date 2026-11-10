package com.device.server

import android.content.Context
import android.content.ContextWrapper
import android.hardware.display.DisplayManager

/**
 * Контекст и `DisplayManager` для процесса без приложения (uid shell). Подход как у scrcpy:
 * создаём `ActivityThread` вручную, берём системный контекст и подставляем пакет `com.android.shell`,
 * иначе служба дисплеев не знает, от чьего имени создаётся виртуальный дисплей.
 */
object Hidden {
    const val PACKAGE = "com.android.shell"

    /** Системный контекст с именем пакета shell. */
    fun context(): Context {
        val atClass = Class.forName("android.app.ActivityThread")
        val ctor = atClass.getDeclaredConstructor().apply { isAccessible = true }
        val thread = ctor.newInstance()
        atClass.getDeclaredField("sCurrentActivityThread").apply { isAccessible = true }.set(null, thread)
        atClass.getDeclaredField("mSystemThread").apply { isAccessible = true }.setBoolean(thread, true)
        val system = atClass.getDeclaredMethod("getSystemContext").invoke(thread) as Context
        return ShellContext(system)
    }

    fun displayManager(ctx: Context): DisplayManager {
        val ctor = DisplayManager::class.java.getDeclaredConstructor(Context::class.java).apply { isAccessible = true }
        return ctor.newInstance(ctx)
    }

    private class ShellContext(base: Context) : ContextWrapper(base) {
        override fun getPackageName(): String = PACKAGE
        override fun getOpPackageName(): String = PACKAGE
        override fun getApplicationContext(): Context = this
    }
}
