package com.device.server

import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import java.io.ByteArrayOutputStream

/**
 * Захват основного дисплея без диалога MediaProjection: зеркальный виртуальный дисплей через закрытый
 * `DisplayManager.createVirtualDisplay(имя, ширина, высота, номерДисплея, поверхность)` (как у scrcpy).
 * Кадр забирается из `ImageReader`, пакуется в JPEG и уходит в `onFrame`. При повороте экрана
 * дисплей и читатель пересоздаются, `onGeometry` сообщает новые размеры потока.
 */
class Capture(
    private val screen: ScreenInfo,
    private val onFrame: (jpeg: ByteArray, ptsUs: Long) -> Unit,
    private val onGeometry: () -> Unit,
) {
    private val tag = "PcDockServer"

    private val thread = HandlerThread("capture").apply { start() }
    private val handler = Handler(thread.looper)

    /** Параметры потока от ПК; 0 — по умолчанию. */
    @Volatile
    var maxLongSide = 0

    @Volatile
    var jpegQuality = 0

    @Volatile
    var maxFps = 0

    private var manager: DisplayManager? = null
    private var reader: ImageReader? = null
    private var display: VirtualDisplay? = null
    private var shown: DisplayState? = null
    private var running = false
    private var lastFrameAt = 0L
    private var lastJpeg: ByteArray? = null
    private var lastPts = 0L

    /** Размер потока для дисплея `d`: сторона не больше заданной, размеры чётные. */
    fun streamSize(d: DisplayState): Pair<Int, Int> {
        val limit = if (maxLongSide > 0) maxLongSide else DEFAULT_LONG_SIDE
        val long = maxOf(d.width, d.height).toFloat()
        val k = if (long > limit) limit / long else 1f
        val w = ((d.width * k).toInt() and 1.inv()).coerceAtLeast(2)
        val h = ((d.height * k).toInt() and 1.inv()).coerceAtLeast(2)
        return Pair(w, h)
    }

    fun start() {
        handler.post {
            if (running) return@post
            running = true
            try {
                create()
                handler.postDelayed(watch, WATCH_MS)
            } catch (e: Throwable) {
                Log.e(tag, "Захват не запустился", e)
                running = false
            }
        }
    }

    fun stop() {
        handler.post {
            running = false
            handler.removeCallbacks(watch)
            release()
        }
    }

    /** Повторно отдать последний кадр (ПК мог пропустить его). */
    fun resend() {
        handler.post { lastJpeg?.let { onFrame(it, lastPts) } }
    }

    fun shutdown() {
        stop()
        thread.quitSafely()
    }

    private val watch = object : Runnable {
        override fun run() {
            if (!running) return
            try {
                val now = screen.current()
                if (now != shown) {
                    Log.i(tag, "Экран изменился: $shown -> $now, пересоздаю захват")
                    release()
                    create()
                    onGeometry()
                }
            } catch (e: Throwable) {
                Log.e(tag, "Сбой пересоздания захвата", e)
            }
            if (running) handler.postDelayed(this, WATCH_MS)
        }
    }

    private fun create() {
        val d = screen.current()
        val (w, h) = streamSize(d)
        val dm = manager ?: Hidden.displayManager(Hidden.context()).also { manager = it }
        val r = ImageReader.newInstance(w, h, PixelFormat.RGBA_8888, 2)
        r.setOnImageAvailableListener({ onImage(it, w, h) }, handler)
        val create = DisplayManager::class.java.getDeclaredMethod(
            "createVirtualDisplay", String::class.java, Int::class.javaPrimitiveType, Int::class.javaPrimitiveType,
            Int::class.javaPrimitiveType, Surface::class.java,
        )
        display = create.invoke(dm, "pcdock", w, h, 0, r.surface) as VirtualDisplay
        reader = r
        shown = d
        Log.i(tag, "Захват: ${d.width}x${d.height} rot=${d.rotation} -> поток ${w}x$h")
    }

    private fun release() {
        try {
            display?.release()
        } catch (_: Throwable) {
        }
        display = null
        try {
            reader?.setOnImageAvailableListener(null, null)
            reader?.close()
        } catch (_: Throwable) {
        }
        reader = null
        shown = null
    }

    private fun onImage(r: ImageReader, w: Int, h: Int) {
        val image = try {
            r.acquireLatestImage()
        } catch (_: Throwable) {
            null
        } ?: return
        try {
            val now = SystemClock.elapsedRealtime()
            val fps = maxFps
            if (fps > 0 && now - lastFrameAt < 1000L / fps) return
            lastFrameAt = now
            val plane = image.planes[0]
            val rowPixels = plane.rowStride / plane.pixelStride
            val full = Bitmap.createBitmap(rowPixels, h, Bitmap.Config.ARGB_8888)
            full.copyPixelsFromBuffer(plane.buffer)
            val bmp = if (rowPixels != w) Bitmap.createBitmap(full, 0, 0, w, h).also { full.recycle() } else full
            val out = ByteArrayOutputStream(w * h / 4)
            bmp.compress(Bitmap.CompressFormat.JPEG, if (jpegQuality > 0) jpegQuality else DEFAULT_QUALITY, out)
            bmp.recycle()
            val jpeg = out.toByteArray()
            lastJpeg = jpeg
            lastPts = now * 1000
            onFrame(jpeg, lastPts)
        } finally {
            image.close()
        }
    }

    companion object {
        const val DEFAULT_LONG_SIDE = 1280
        const val DEFAULT_QUALITY = 70
        private const val WATCH_MS = 500L
    }
}
