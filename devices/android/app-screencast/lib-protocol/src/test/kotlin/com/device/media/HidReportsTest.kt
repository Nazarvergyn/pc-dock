package com.device.media

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotNull
import kotlin.test.assertNull
import kotlin.test.assertTrue

class HidReportsTest {
    /** Отчёт тача как строит его ядро (hid.rs, `touch_report_layout`): x=3277, y=9830, 400×400, сила 300. */
    private fun touchReport(tip: Boolean, id: Int, x: Int, y: Int, w: Int, h: Int, p: Int): ByteArray {
        val r = ByteArray(14)
        r[0] = 1
        r[1] = if (tip) 1 else 0
        r[2] = id.toByte()
        for ((i, v) in listOf(x, y, w, h, p).withIndex()) {
            r[3 + i * 2] = (v and 0xFF).toByte()
            r[4 + i * 2] = (v shr 8).toByte()
        }
        r[13] = 1
        return r
    }

    @Test
    fun touchIsDecodedIntoFractions() {
        val t = assertNotNull(HidReports.touch(touchReport(true, 3, 3277, 9830, 400, 400, 300)))
        assertTrue(t.tip)
        assertEquals(3, t.id)
        assertEquals(3277 / 32767f, t.x, 1e-6f)
        assertEquals(9830 / 32767f, t.y, 1e-6f)
        assertEquals(400 / 32767f, t.width, 1e-6f)
        assertEquals(300 / 1023f, t.pressure, 1e-6f)
    }

    @Test
    fun shortReportsAreRejected() {
        assertNull(HidReports.touch(ByteArray(13)))
        assertNull(HidReports.mouse(ByteArray(6)))
        assertNull(HidReports.keyboard(ByteArray(7)))
    }

    @Test
    fun mouseHandlesSignedMotionAndWheel() {
        // кнопки 0b101, dx=-2 (0xFFFE), dy=300 (0x012C), колесо В=-1, Г=3
        val r = byteArrayOf(0b101, 0xFE.toByte(), 0xFF.toByte(), 0x2C, 0x01, 0xFF.toByte(), 3)
        val m = assertNotNull(HidReports.mouse(r))
        assertEquals(0b101, m.buttons)
        assertEquals(-2, m.dx)
        assertEquals(300, m.dy)
        assertEquals(-1, m.wheelV)
        assertEquals(3, m.wheelH)
    }

    @Test
    fun keyboardSkipsEmptySlots() {
        val k = assertNotNull(HidReports.keyboard(byteArrayOf(0x02, 0, 0x04, 0, 0x29, 0, 0, 0)))
        assertEquals(0x02, k.modifiers)
        assertEquals(listOf(0x04, 0x29), k.keys)
    }

    /** Ядро (`to_device_coords`) пересчитывает точку экрана в координаты корпуса; здесь обратное. */
    private fun coreToDevice(rotation: Int, x: Float, y: Float): Pair<Float, Float> = when (rotation) {
        1 -> Pair(1f - y, x)
        2 -> Pair(1f - x, 1f - y)
        3 -> Pair(y, 1f - x)
        else -> Pair(x, y)
    }

    @Test
    fun bodyToDisplayInvertsTheCoreMapping() {
        for (rotation in 0..3) {
            for ((x, y) in listOf(0.1f to 0.2f, 0.9f to 0.3f, 0.5f to 0.5f, 0f to 1f)) {
                val (dx, dy) = coreToDevice(rotation, x, y)
                val (bx, by) = HidReports.bodyToDisplay(dx, dy, rotation)
                assertEquals(x, bx, 1e-5f, "rot=$rotation x")
                assertEquals(y, by, 1e-5f, "rot=$rotation y")
            }
        }
    }
}
