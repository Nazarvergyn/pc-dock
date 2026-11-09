package com.device.server

/** Размер, плотность и поворот основного дисплея. */
data class DisplayState(val width: Int, val height: Int, val dpi: Int, val rotation: Int)

/**
 * Состояние основного дисплея через закрытый `DisplayManagerGlobal.getDisplayInfo` (рефлексия,
 * `Method` кэшируется). Размеры уже в текущей ориентации.
 */
class ScreenInfo {
    private val global: Any
    private val getInfo: java.lang.reflect.Method

    init {
        val cls = Class.forName("android.hardware.display.DisplayManagerGlobal")
        global = cls.getDeclaredMethod("getInstance").invoke(null)!!
        getInfo = cls.getMethod("getDisplayInfo", Int::class.javaPrimitiveType)
    }

    fun current(): DisplayState {
        val info = getInfo.invoke(global, 0)!!
        val c = info.javaClass
        return DisplayState(
            width = c.getField("logicalWidth").getInt(info),
            height = c.getField("logicalHeight").getInt(info),
            dpi = c.getField("logicalDensityDpi").getInt(info),
            rotation = c.getField("rotation").getInt(info),
        )
    }
}
