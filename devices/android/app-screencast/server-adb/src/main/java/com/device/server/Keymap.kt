package com.device.server

import android.view.KeyEvent

/** HID usage (страница Keyboard) → код клавиши Android. 0 — не поддержана. */
object Keymap {
    fun androidKey(usage: Int): Int = when (usage) {
        in 0x04..0x1D -> KeyEvent.KEYCODE_A + (usage - 0x04)
        in 0x1E..0x26 -> KeyEvent.KEYCODE_1 + (usage - 0x1E)
        0x27 -> KeyEvent.KEYCODE_0
        0x28 -> KeyEvent.KEYCODE_ENTER
        0x29 -> KeyEvent.KEYCODE_BACK // Esc
        0x2A -> KeyEvent.KEYCODE_DEL
        0x2B -> KeyEvent.KEYCODE_TAB
        0x2C -> KeyEvent.KEYCODE_SPACE
        0x2D -> KeyEvent.KEYCODE_MINUS
        0x2E -> KeyEvent.KEYCODE_EQUALS
        0x2F -> KeyEvent.KEYCODE_LEFT_BRACKET
        0x30 -> KeyEvent.KEYCODE_RIGHT_BRACKET
        0x31 -> KeyEvent.KEYCODE_BACKSLASH
        0x33 -> KeyEvent.KEYCODE_SEMICOLON
        0x34 -> KeyEvent.KEYCODE_APOSTROPHE
        0x35 -> KeyEvent.KEYCODE_GRAVE
        0x36 -> KeyEvent.KEYCODE_COMMA
        0x37 -> KeyEvent.KEYCODE_PERIOD
        0x38 -> KeyEvent.KEYCODE_SLASH
        in 0x3A..0x45 -> KeyEvent.KEYCODE_F1 + (usage - 0x3A)
        0x4A -> KeyEvent.KEYCODE_MOVE_HOME
        0x4B -> KeyEvent.KEYCODE_PAGE_UP
        0x4C -> KeyEvent.KEYCODE_FORWARD_DEL
        0x4D -> KeyEvent.KEYCODE_MOVE_END
        0x4E -> KeyEvent.KEYCODE_PAGE_DOWN
        0x4F -> KeyEvent.KEYCODE_DPAD_RIGHT
        0x50 -> KeyEvent.KEYCODE_DPAD_LEFT
        0x51 -> KeyEvent.KEYCODE_DPAD_DOWN
        0x52 -> KeyEvent.KEYCODE_DPAD_UP
        0xE0 -> KeyEvent.KEYCODE_CTRL_LEFT
        0xE1 -> KeyEvent.KEYCODE_SHIFT_LEFT
        0xE2 -> KeyEvent.KEYCODE_ALT_LEFT
        0xE3 -> KeyEvent.KEYCODE_META_LEFT // GUI
        0xE4 -> KeyEvent.KEYCODE_CTRL_RIGHT
        0xE5 -> KeyEvent.KEYCODE_SHIFT_RIGHT
        0xE6 -> KeyEvent.KEYCODE_ALT_RIGHT
        0xE7 -> KeyEvent.KEYCODE_META_RIGHT
        else -> 0
    }
}
