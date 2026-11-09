package com.device.adb

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.graphics.Path
import android.os.SystemClock
import android.util.Log
import com.device.media.HidCommand

/**
 * Перевод HID-команд ADB-канала в жесты accessibility-сервиса.
 *
 * Форматы репортов — те же, что строит Rust-ядро (см. hid.rs):
 * тач 14 байт [tip, id, x, y, w, h, pressure u16LE, count],
 * мышь 7 байт [btn, dx, dy s16LE, wheel_v, wheel_h],
 * клавиатура 8 байт [mod, _, 6 клавиш].
 * Координаты тача 0..32767 маппятся на пиксели текущего дисплея.
 *
 * Ограничения v1: мультитач исполняется попальцево (жесты идут друг за другом,
 * а не одновременно); из клавиш — только Esc→назад и GUI→домой, остальное
 * возвращается строкой для STATUS.
 */
object HidInput {
    private const val TAG = "AdbDaemon"
    private const val COORD_MAX = 32767

    @Volatile
    private var svc: AccessibilityService? = null

    fun bind(s: AccessibilityService) {
        svc = s
    }

    fun unbind() {
        svc = null
    }

    fun isReady() = svc != null

    private data class Stroke(
        val startX: Float, val startY: Float,
        var lastX: Float, var lastY: Float,
        val t0: Long,
    )

    private val contacts = HashMap<Int, Stroke>()
    private var mouseX = -1f
    private var mouseY = -1f
    private var mouseDown = false
    private var mouseDownX = 0f
    private var mouseDownY = 0f

    /** Разбирает команду. Возвращает текст для STATUS, если ввод не поддержан. */
    fun handle(cmd: HidCommand): String? {
        if (cmd.request != 57) return "hid request ${cmd.request}"
        return when (cmd.hidId) {
            1 -> touch(cmd.payload)
            3 -> mouse(cmd.payload)
            2 -> keyboard(cmd.payload)
            else -> "hid id ${cmd.hidId}"
        }
    }

    // ----- тач -----

    private fun touch(r: ByteArray): String? {
        if (r.size < 14) return "короткий тач-репорт"
        val s = svc ?: return "accessibility выключен"
        val tip = r[1].toInt() != 0
        val id = r[2].toInt() and 0xFF
        val (w, h) = screenSize(s) ?: return "нет дисплея"
        val x = u16(r, 3) / COORD_MAX.toFloat() * w
        val y = u16(r, 5) / COORD_MAX.toFloat() * h
        if (tip) {
            val st = contacts[id]
            if (st == null) {
                contacts[id] = Stroke(x, y, x, y, SystemClock.uptimeMillis())
            } else {
                st.lastX = x
                st.lastY = y
            }
        } else {
            val st = contacts.remove(id) ?: return null
            st.lastX = x
            st.lastY = y
            dispatchStroke(s, st)
        }
        return null
    }

    private fun dispatchStroke(s: AccessibilityService, st: Stroke) {
        val path = Path()
        path.moveTo(st.startX, st.startY)
        path.lineTo(st.lastX, st.lastY)
        val dur = (SystemClock.uptimeMillis() - st.t0).coerceIn(50, 1000)
        val stroke = GestureDescription.StrokeDescription(path, 0, dur)
        val ok = s.dispatchGesture(GestureDescription.Builder().addStroke(stroke).build(), null, null)
        if (!ok) Log.w(TAG, "жест отклонён системой")
    }

    // ----- мышь -----

    private fun mouse(r: ByteArray): String? {
        if (r.size < 7) return "короткий mouse-репорт"
        val s = svc ?: return "accessibility выключен"
        val (w, h) = screenSize(s) ?: return "нет дисплея"
        if (mouseX < 0) {
            mouseX = w / 2
            mouseY = h / 2
        }
        val dx = s16(r, 1).toFloat()
        val dy = s16(r, 3).toFloat()
        mouseX = (mouseX + dx).coerceIn(0f, w - 1)
        mouseY = (mouseY + dy).coerceIn(0f, h - 1)
        val btn = r[0].toInt() and 0x07
        if (btn and 0x01 != 0 && !mouseDown) {
            mouseDown = true
            mouseDownX = mouseX
            mouseDownY = mouseY
        } else if (btn and 0x01 == 0 && mouseDown) {
            mouseDown = false
            dispatchStroke(s, Stroke(mouseDownX, mouseDownY, mouseX, mouseY, SystemClock.uptimeMillis() - 60))
        }
        val wv = r[5].toInt()
        val wh = r[6].toInt()
        if (wv != 0 || wh != 0) {
            // Колесо — свайп в точке курсора (вверх колеса = контент вверх = палец вверх).
            val sx = mouseX
            val sy = mouseY
            val ex = (sx - wh * 40f).coerceIn(0f, w - 1)
            val ey = (sy - wv * 40f).coerceIn(0f, h - 1)
            dispatchStroke(s, Stroke(sx, sy, ex, ey, SystemClock.uptimeMillis() - 60))
        }
        return null
    }

    // ----- клавиатура -----

    private fun keyboard(r: ByteArray): String? {
        val s = svc ?: return "accessibility выключен"
        val down = r.drop(2).any { it != 0.toByte() } || r[0] != 0.toByte()
        if (!down) return null // отпускания системных клавиш не требуют действий
        val keys = r.drop(2).map { it.toInt() and 0xFF }.filter { it != 0 }
        for (k in keys) {
            when (k) {
                0x29 -> s.performGlobalAction(AccessibilityService.GLOBAL_ACTION_BACK)
                0x3A, 0x3B, 0x3C -> {} // F1..F3 — пропускаем молча
                else -> return "клавиша $k"
            }
        }
        if (r[0].toInt() and 0x08 != 0) s.performGlobalAction(AccessibilityService.GLOBAL_ACTION_HOME) // GUI
        return null
    }

    private fun screenSize(s: AccessibilityService): Pair<Float, Float>? {
        // Метрики уже в текущей ориентации; ядро отдаёт координаты по корпусу,
        // система сама поворачивает их вместе с экраном.
        val dm = s.resources.displayMetrics
        if (dm.widthPixels <= 0 || dm.heightPixels <= 0) return null
        return Pair(dm.widthPixels.toFloat(), dm.heightPixels.toFloat())
    }

    private fun u16(r: ByteArray, o: Int) = ((r[o].toInt() and 0xFF) or ((r[o + 1].toInt() and 0xFF) shl 8))
    private fun s16(r: ByteArray, o: Int): Int {
        val v = u16(r, o)
        return if (v >= 0x8000) v - 0x10000 else v
    }
}
