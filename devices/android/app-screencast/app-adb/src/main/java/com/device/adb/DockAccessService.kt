package com.device.adb

import android.accessibilityservice.AccessibilityService
import android.util.Log
import android.view.accessibility.AccessibilityEvent

/**
 * Ввод ADB-режима: выполняет жесты, которые разбирает [HidInput].
 * Включается один раз в настройках (Специальные возможности); рута и отладки не требует.
 */
class DockAccessService : AccessibilityService() {
    private val tag = "AdbDaemon"

    override fun onServiceConnected() {
        Log.d(tag, "Accessibility подключён: жесты доступны")
        HidInput.bind(this)
    }

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        Log.d(tag, "Accessibility отключён")
        HidInput.unbind()
        return super.onUnbind(intent)
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}
    override fun onInterrupt() {}
}
