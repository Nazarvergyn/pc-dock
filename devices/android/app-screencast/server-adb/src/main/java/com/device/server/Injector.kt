package com.device.server

import android.os.SystemClock
import android.util.Log
import android.view.InputDevice
import android.view.InputEvent
import android.view.KeyEvent
import android.view.MotionEvent
import com.device.media.HidReports

/**
 * Ввод через `InputManager.injectInputEvent` (закрытый API, доступен uid shell; Accessibility не нужен).
 * Мультитач, давление и площадь пятна уходят в `MotionEvent`. HID-репорты те же, что строит Rust-ядро.
 *
 * Метод InputManager вызывается через рефлексию; `Method` ищется один раз при создании.
 * Не потокобезопасен: вызывать из одного потока.
 */
class Injector(private val screen: ScreenInfo) {
    private val tag = "PcDockServer"

    private val manager: Any
    private val inject: java.lang.reflect.Method

    init {
        val cls = Class.forName("android.hardware.input.InputManager")
        manager = cls.getDeclaredMethod("getInstance").invoke(null)!!
        inject = cls.getMethod("injectInputEvent", InputEvent::class.java, Int::class.javaPrimitiveType)
    }

    private class Contact(val id: Int, var x: Float, var y: Float, var pressure: Float, var w: Float, var h: Float)

    /** Активные пальцы в порядке касания: индекс в списке = индекс указателя в MotionEvent. */
    private val contacts = ArrayList<Contact>()
    private var touchDownTime = 0L

    private var mouseX = -1f
    private var mouseY = -1f
    private var mouseButtons = 0
    private var mouseDownTime = 0L

    private var keyboardPressed = emptySet<Int>()

    /** Возвращает текст для STATUS, если репорт не поддержан. */
    fun handle(hidId: Int, payload: ByteArray): String? = try {
        when (hidId) {
            1 -> touch(payload)
            2 -> keyboard(payload)
            3 -> mouse(payload)
            else -> "hid id $hidId"
        }
    } catch (e: Throwable) {
        Log.e(tag, "Сбой инъекции", e)
        "injection failed: $e"
    }

    // ----- тач -----

    private fun touch(r: ByteArray): String? {
        val t = HidReports.touch(r) ?: return "короткий тач-репорт"
        val s = screen.current()
        val (px, py) = HidReports.bodyToDisplay(t.x, t.y, s.rotation)
        // Пятно поворачивается вместе с телефоном: на 90° и 270° ширина и высота меняются местами.
        val (fw, fh) = if (s.rotation % 2 == 1) Pair(t.height, t.width) else Pair(t.width, t.height)
        val x = px * (s.width - 1)
        val y = py * (s.height - 1)
        val known = contacts.indexOfFirst { it.id == t.id }
        if (t.tip) {
            if (known < 0) {
                val c = Contact(t.id, x, y, t.pressure, fw * s.width, fh * s.height)
                contacts.add(c)
                val now = SystemClock.uptimeMillis()
                if (contacts.size == 1) touchDownTime = now
                val action = if (contacts.size == 1) MotionEvent.ACTION_DOWN
                else MotionEvent.ACTION_POINTER_DOWN or ((contacts.size - 1) shl MotionEvent.ACTION_POINTER_INDEX_SHIFT)
                send(action, now)
            } else {
                contacts[known].apply {
                    this.x = x
                    this.y = y
                    pressure = t.pressure
                    w = fw * s.width
                    h = fh * s.height
                }
                send(MotionEvent.ACTION_MOVE, SystemClock.uptimeMillis())
            }
        } else if (known >= 0) {
            contacts[known].apply {
                this.x = x
                this.y = y
            }
            val action = if (contacts.size == 1) MotionEvent.ACTION_UP
            else MotionEvent.ACTION_POINTER_UP or (known shl MotionEvent.ACTION_POINTER_INDEX_SHIFT)
            send(action, SystemClock.uptimeMillis())
            contacts.removeAt(known)
        }
        return null
    }

    private fun send(action: Int, now: Long) {
        val n = contacts.size
        val props = Array(n) { MotionEvent.PointerProperties() }
        val coords = Array(n) { MotionEvent.PointerCoords() }
        contacts.forEachIndexed { i, c ->
            props[i].id = c.id
            props[i].toolType = MotionEvent.TOOL_TYPE_FINGER
            coords[i].x = c.x
            coords[i].y = c.y
            coords[i].pressure = c.pressure
            coords[i].size = (maxOf(c.w, c.h) / maxOf(screen.current().width, screen.current().height).toFloat())
                .coerceIn(0f, 1f)
            coords[i].touchMajor = maxOf(c.w, c.h)
            coords[i].touchMinor = minOf(c.w, c.h)
        }
        val e = MotionEvent.obtain(
            touchDownTime, now, action, n, props, coords, 0, 0, 1f, 1f, 0, 0,
            InputDevice.SOURCE_TOUCHSCREEN, 0,
        )
        try {
            inject.invoke(manager, e, 0) // 0 = INJECT_INPUT_EVENT_MODE_ASYNC
        } finally {
            e.recycle()
        }
    }

    // ----- мышь -----

    private fun mouse(r: ByteArray): String? {
        val m = HidReports.mouse(r) ?: return "короткий mouse-репорт"
        val s = screen.current()
        if (mouseX < 0) {
            mouseX = s.width / 2f
            mouseY = s.height / 2f
        }
        mouseX = (mouseX + m.dx).coerceIn(0f, (s.width - 1).toFloat())
        mouseY = (mouseY + m.dy).coerceIn(0f, (s.height - 1).toFloat())
        val now = SystemClock.uptimeMillis()
        val pressed = m.buttons
        val changed = pressed xor mouseButtons
        if (changed != 0 && pressed != 0 && mouseButtons == 0) mouseDownTime = now
        val action = when {
            mouseButtons == 0 && pressed != 0 -> MotionEvent.ACTION_DOWN
            mouseButtons != 0 && pressed == 0 -> MotionEvent.ACTION_UP
            pressed != 0 -> MotionEvent.ACTION_MOVE
            else -> MotionEvent.ACTION_HOVER_MOVE
        }
        mouseEvent(action, now, buttonState(pressed), 0f, 0f)
        mouseButtons = pressed
        if (m.wheelV != 0 || m.wheelH != 0) {
            mouseEvent(MotionEvent.ACTION_SCROLL, now, buttonState(pressed), m.wheelH.toFloat(), m.wheelV.toFloat())
        }
        return null
    }

    private fun buttonState(bits: Int): Int {
        var b = 0
        if (bits and 1 != 0) b = b or MotionEvent.BUTTON_PRIMARY
        if (bits and 2 != 0) b = b or MotionEvent.BUTTON_SECONDARY
        if (bits and 4 != 0) b = b or MotionEvent.BUTTON_TERTIARY
        return b
    }

    private fun mouseEvent(action: Int, now: Long, buttons: Int, hscroll: Float, vscroll: Float) {
        val props = arrayOf(MotionEvent.PointerProperties().apply {
            id = 0
            toolType = MotionEvent.TOOL_TYPE_MOUSE
        })
        val coords = arrayOf(MotionEvent.PointerCoords().apply {
            x = mouseX
            y = mouseY
            pressure = if (buttons != 0) 1f else 0f
            size = 1f
            setAxisValue(MotionEvent.AXIS_HSCROLL, hscroll)
            // Колесо вверх (положительное значение ядра) — прокрутка вверх, у Android это положительный VSCROLL.
            setAxisValue(MotionEvent.AXIS_VSCROLL, vscroll)
        })
        val e = MotionEvent.obtain(
            if (mouseDownTime != 0L) mouseDownTime else now, now, action, 1, props, coords, 0, buttons, 1f, 1f, 0, 0,
            InputDevice.SOURCE_MOUSE, 0,
        )
        try {
            inject.invoke(manager, e, 0)
        } finally {
            e.recycle()
        }
    }

    // ----- клавиатура -----

    private fun keyboard(r: ByteArray): String? {
        val k = HidReports.keyboard(r) ?: return "короткий keyboard-репорт"
        val now = SystemClock.uptimeMillis()
        val pressed = k.keys.toSet() + modifierUsages(k.modifiers)
        var unsupported: String? = null
        for (u in pressed - keyboardPressed) {
            if (!key(KeyEvent.ACTION_DOWN, u, now)) unsupported = "клавиша $u"
        }
        for (u in keyboardPressed - pressed) key(KeyEvent.ACTION_UP, u, now)
        keyboardPressed = pressed
        return unsupported
    }

    /** Биты модификаторов HID → usage-коды клавиш (0xE0..0xE7). */
    private fun modifierUsages(mods: Int) = (0..7).filter { mods and (1 shl it) != 0 }.map { 0xE0 + it }.toSet()

    private fun key(action: Int, usage: Int, now: Long): Boolean {
        val code = Keymap.androidKey(usage)
        if (code == 0) return false
        val e = KeyEvent(now, now, action, code, 0, 0, 0, 0, 0, InputDevice.SOURCE_KEYBOARD)
        inject.invoke(manager, e, 0)
        return true
    }

    /** Отпускает всё удержанное: вызывается при закрытии канала, чтобы не залипли кнопки. */
    fun releaseAll() {
        try {
            if (contacts.isNotEmpty()) {
                val now = SystemClock.uptimeMillis()
                while (contacts.isNotEmpty()) {
                    val action = if (contacts.size == 1) MotionEvent.ACTION_UP
                    else MotionEvent.ACTION_POINTER_UP or ((contacts.size - 1) shl MotionEvent.ACTION_POINTER_INDEX_SHIFT)
                    send(action, now)
                    contacts.removeAt(contacts.size - 1)
                }
            }
            if (mouseButtons != 0) {
                mouseEvent(MotionEvent.ACTION_UP, SystemClock.uptimeMillis(), 0, 0f, 0f)
                mouseButtons = 0
            }
            for (u in keyboardPressed) key(KeyEvent.ACTION_UP, u, SystemClock.uptimeMillis())
            keyboardPressed = emptySet()
        } catch (e: Throwable) {
            Log.w(tag, "releaseAll: $e")
        }
    }
}
