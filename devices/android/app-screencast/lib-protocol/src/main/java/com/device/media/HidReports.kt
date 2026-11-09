package com.device.media

/**
 * Разбор HID-репортов, которые строит Rust-ядро (hid.rs), для режимов без настоящего HID (ADB):
 * тач 14 байт [report_id=1, tip, id, x u16LE, y u16LE, w u16LE, h u16LE, pressure u16LE, count],
 * мышь 7 байт [btn, dx s16LE, dy s16LE, wheel_v s8, wheel_h s8],
 * клавиатура 8 байт [mod, _, 6 клавиш].
 * Здесь только чистая логика без классов Android: проверяется на JVM.
 */
object HidReports {
    const val COORD_MAX = 32767f
    const val PRESSURE_MAX = 1023f

    class Touch(
        val tip: Boolean,
        val id: Int,
        val x: Float,
        val y: Float,
        val width: Float,
        val height: Float,
        val pressure: Float,
    )

    class Mouse(val buttons: Int, val dx: Int, val dy: Int, val wheelV: Int, val wheelH: Int)

    class Keyboard(val modifiers: Int, val keys: List<Int>)

    /** Доли корпуса 0..1 (x, y), пятно и давление 0..1. */
    fun touch(r: ByteArray): Touch? {
        if (r.size < 14) return null
        return Touch(
            tip = r[1].toInt() != 0,
            id = r[2].toInt() and 0xFF,
            x = u16(r, 3) / COORD_MAX,
            y = u16(r, 5) / COORD_MAX,
            width = u16(r, 7) / COORD_MAX,
            height = u16(r, 9) / COORD_MAX,
            pressure = (u16(r, 11) / PRESSURE_MAX).coerceIn(0f, 1f),
        )
    }

    fun mouse(r: ByteArray): Mouse? {
        if (r.size < 7) return null
        return Mouse(r[0].toInt() and 0x07, s16(r, 1), s16(r, 3), r[5].toInt(), r[6].toInt())
    }

    fun keyboard(r: ByteArray): Keyboard? {
        if (r.size < 8) return null
        return Keyboard(r[0].toInt() and 0xFF, r.drop(2).map { it.toInt() and 0xFF }.filter { it != 0 })
    }

    /**
     * Ядро отдаёт координаты тача «по корпусу» (`to_device_coords`): Android сам повернул бы их вместе
     * с экраном. При прямой подаче событий в дисплей нужен обратный пересчёт: корпус → текущая ориентация
     * дисплея (`rotation` 0..3, как в DisplayInfo).
     */
    fun bodyToDisplay(x: Float, y: Float, rotation: Int): Pair<Float, Float> = when (rotation and 3) {
        1 -> Pair(y, 1f - x)
        2 -> Pair(1f - x, 1f - y)
        3 -> Pair(1f - y, x)
        else -> Pair(x, y)
    }

    private fun u16(r: ByteArray, o: Int) = (r[o].toInt() and 0xFF) or ((r[o + 1].toInt() and 0xFF) shl 8)

    private fun s16(r: ByteArray, o: Int): Int {
        val v = u16(r, o)
        return if (v >= 0x8000) v - 0x10000 else v
    }
}
