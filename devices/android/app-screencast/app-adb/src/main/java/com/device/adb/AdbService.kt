package com.device.adb

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.Image
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.os.SystemClock
import android.util.DisplayMetrics
import android.util.Log
import android.view.WindowManager
import androidx.core.app.NotificationCompat
import com.device.media.AudioCapture
import com.device.media.HostLink
import com.device.media.Messages
import com.device.media.Packet
import com.device.media.Proto
import com.device.media.TcpLink
import java.io.ByteArrayOutputStream

/**
 * Консольная часть ADB-режима: захват экрана, сокет для демона, ввод через accessibility.
 * Стартует с ПК (`adb shell am start`), иконки в лаунчере нет.
 * Протокол тот же v2; ввод приходит HID-командами и переводится в жесты [HidInput].
 */
class AdbService : Service(), HostLink.Listener {
    companion object {
        @Volatile
        var isRunning = false

        private const val CHANNEL_ID = "adb_channel"
        private const val TCP_PORT = 47631
        private const val DEFAULT_JPEG_QUALITY = 70
        private const val NAME = "pcdock-adb/1.0"
    }

    private val tag = "AdbDaemon"

    private var mediaProjection: MediaProjection? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var imageReader: ImageReader? = null
    private var captureThread: HandlerThread? = null
    private var handler: Handler? = null

    private var link: HostLink? = null
    private var sessionActive = false
    private var lastHostRx = 0L
    private var video = true
    private var audio = false
    private var jpegQuality = DEFAULT_JPEG_QUALITY
    private var maxFps = 0

    private var frameBitmap: Bitmap? = null
    private val jpegBuffer = ByteArrayOutputStream()
    private var lastJpeg: ByteArray? = null
    private var lastPtsUs = 0L
    private var lastConvertMs = 0L

    private var audioCapture: AudioCapture? = null
    private var audioActive = false
    private var prevMusicVolume = -1

    private var tcpStarted = false
    private var tcpServer: java.net.ServerSocket? = null

    private var running = false
    private var framesSent = 0L

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        createChannel()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(1, buildNotification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(1, buildNotification())
        }
        val resultCode = intent?.getIntExtra("resultCode", Activity.RESULT_CANCELED) ?: Activity.RESULT_CANCELED
        val data = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent?.getParcelableExtra("media_data", Intent::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent?.getParcelableExtra("media_data") as? Intent
        }
        if (running) {
            Log.d(tag, "Сервис уже работает")
            return START_NOT_STICKY
        }
        if (resultCode != Activity.RESULT_OK || data == null) {
            Log.e(tag, "Нет разрешения на захват")
            stopSelf()
            return START_NOT_STICKY
        }
        val pm = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        mediaProjection = pm.getMediaProjection(resultCode, data)
        startCapture()
        running = true
        isRunning = true
        startTcpListener()
        handler?.post(object : Runnable {
            override fun run() {
                try {
                    if (!running) return
                    logHeartbeat()
                } catch (e: Throwable) {
                    Log.e(tag, "Сбой heartbeat", e)
                } finally {
                    if (running) handler?.postDelayed(this, 5000)
                }
            }
        })
        return START_NOT_STICKY
    }

    // ---------- захват ----------

    private fun startCapture() {
        val projection = mediaProjection ?: return
        val thread = HandlerThread("adb-capture").also { it.start() }
        captureThread = thread
        val h = Handler(thread.looper)
        handler = h
        projection.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                Log.w(tag, "Проекция остановлена")
                handler?.post { stopSelf() }
            }
        }, h)
        val wm = getSystemService(Context.WINDOW_SERVICE) as WindowManager
        val m = DisplayMetrics()
        @Suppress("DEPRECATION")
        wm.defaultDisplay.getRealMetrics(m)
        @Suppress("DEPRECATION")
        val rotation = wm.defaultDisplay.rotation
        val w = (m.widthPixels / 2 and 1.inv()).coerceAtLeast(2)
        val hgt = (m.heightPixels / 2 and 1.inv()).coerceAtLeast(2)
        val reader = ImageReader.newInstance(w, hgt, PixelFormat.RGBA_8888, 2)
        reader.setOnImageAvailableListener({ r ->
            try {
                val image = r.acquireLatestImage() ?: return@setOnImageAvailableListener
                processImage(image)
            } catch (e: Throwable) {
                Log.e(tag, "Сбой кадра", e)
            }
        }, h)
        imageReader = reader
        val vd = projection.createVirtualDisplay(
            "AdbCapture", w, hgt, m.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, reader.surface, null, h
        )
        virtualDisplay = vd
        if (vd == null) {
            Log.e(tag, "VirtualDisplay не создан")
            stopSelf()
            return
        }
        Log.d(tag, "Захват: ${m.widthPixels}x${m.heightPixels} rot=$rotation -> ${w}x$hgt")
    }

    private fun processImage(image: Image) {
        try {
            val now = SystemClock.elapsedRealtime()
            if (maxFps > 0 && now - lastConvertMs < 1000L / maxFps) {
                image.close()
                return
            }
            lastConvertMs = now
            val width = image.width
            val height = image.height
            val plane = image.planes[0]
            val rowPadding = plane.rowStride - plane.pixelStride * width
            val paddedWidth = width + rowPadding / plane.pixelStride
            var bitmap = frameBitmap
            if (bitmap == null || bitmap.width != paddedWidth || bitmap.height != height) {
                bitmap?.recycle()
                bitmap = Bitmap.createBitmap(paddedWidth, height, Bitmap.Config.ARGB_8888)
                frameBitmap = bitmap
            }
            bitmap.copyPixelsFromBuffer(plane.buffer)
            val frame = if (rowPadding == 0) bitmap else Bitmap.createBitmap(bitmap, 0, 0, width, height)
            jpegBuffer.reset()
            frame.compress(Bitmap.CompressFormat.JPEG, jpegQuality.coerceIn(1, 100), jpegBuffer)
            if (frame !== bitmap) frame.recycle()
            val jpeg = jpegBuffer.toByteArray()
            lastJpeg = jpeg
            lastPtsUs = SystemClock.elapsedRealtimeNanos() / 1000
            if (sessionActive && video) {
                link?.offerVideo(Messages.videoFrame(jpeg, lastPtsUs).encode())
                framesSent++
            }
        } finally {
            image.close()
        }
    }

    // ---------- сокет ----------

    private fun startTcpListener() {
        if (tcpStarted) return
        tcpStarted = true
        Thread({
            try {
                val server = java.net.ServerSocket(TCP_PORT, 1, java.net.InetAddress.getByName("127.0.0.1"))
                tcpServer = server
                Log.d(tag, "Слушаю 127.0.0.1:$TCP_PORT")
                while (running) {
                    val socket = try {
                        server.accept()
                    } catch (e: Exception) {
                        if (running) Log.w(tag, "TCP-accept: $e")
                        break
                    }
                    handler?.post { attach(socket) }
                }
            } catch (e: Exception) {
                Log.w(tag, "TCP-слушатель не встал: $e")
            }
        }, "adb-accept").start()
    }

    private fun attach(socket: java.net.Socket) {
        if (link != null) {
            try {
                socket.close()
            } catch (_: Exception) {
            }
            return
        }
        val l = TcpLink(socket, this)
        link = l
        sessionActive = false
        Log.d(tag, "Демон подключён, жду HELLO")
        l.start()
    }

    // ---------- сессия ----------

    override fun onPacket(packet: Packet) {
        handler?.post {
            try {
                lastHostRx = SystemClock.elapsedRealtime()
                when (packet.type) {
                    Proto.T_HELLO -> onHello()
                    Proto.T_PING -> link?.sendControl(Messages.pong(packet.payload).encode())
                    Proto.T_SET_STREAM -> {
                        val s = Messages.parseSetStream(packet.payload) ?: return@post
                        video = s.video
                        audio = s.audio
                        jpegQuality = if (s.jpegQuality == 0) DEFAULT_JPEG_QUALITY else s.jpegQuality
                        maxFps = s.maxFps
                        sendDisplayInfo()
                        if (s.audio) startAudio() else stopAudio()
                        lastJpeg?.let { if (sessionActive && video) link?.offerVideo(Messages.videoFrame(it, lastPtsUs).encode()) }
                    }
                    Proto.T_REQUEST_FRAME -> {
                        lastJpeg?.let { if (sessionActive && video) link?.offerVideo(Messages.videoFrame(it, lastPtsUs).encode()) }
                    }
                    Proto.T_WAKE -> {} // ADB-режим: экран обычно включён; будить нечем без wake lock — пропускаем
                    else -> {}
                }
            } catch (e: Throwable) {
                Log.e(tag, "Сбой пакета", e)
            }
        }
    }

    override fun onClosed(reason: String) {
        handler?.post {
            Log.w(tag, "Канал закрыт: $reason")
            stopAudio()
            link = null
            sessionActive = false
        }
    }

    override fun onHid(cmd: com.device.media.HidCommand) {
        val unsupported = HidInput.handle(cmd)
        if (unsupported != null) {
            Log.w(tag, "Ввод не поддержан: $unsupported")
            link?.sendControl(Messages.status(Proto.STATUS_INPUT_UNSUPPORTED, "input unsupported: $unsupported").encode())
        }
    }

    private fun onHello() {
        stopAudio()
        sessionActive = true
        val l = link ?: return
        l.sendControl(Messages.hello(Proto.CAP_VIDEO_JPEG or Proto.CAP_ROTATION or Proto.CAP_AUDIO_PCM, NAME).encode())
        sendDisplayInfo()
        lastJpeg?.let { if (video) link?.offerVideo(Messages.videoFrame(it, lastPtsUs).encode()) }
    }

    private fun sendDisplayInfo() {
        val wm = getSystemService(Context.WINDOW_SERVICE) as WindowManager
        val m = DisplayMetrics()
        @Suppress("DEPRECATION")
        wm.defaultDisplay.getRealMetrics(m)
        @Suppress("DEPRECATION")
        val rotation = wm.defaultDisplay.rotation
        val w = (m.widthPixels / 2 and 1.inv()).coerceAtLeast(2)
        val h = (m.heightPixels / 2 and 1.inv()).coerceAtLeast(2)
        link?.sendControl(Messages.displayInfo(w, h, m.widthPixels, m.heightPixels, m.densityDpi, rotation).encode())
    }

    // ---------- звук ----------

    private fun startAudio() {
        if (audioActive) return
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            link?.sendControl(Messages.status(Proto.STATUS_AUDIO_UNAVAILABLE, "audio requires Android 10+").encode())
            return
        }
        val projection = mediaProjection ?: return
        val mgr = getSystemService(Context.AUDIO_SERVICE) as android.media.AudioManager
        try {
            prevMusicVolume = mgr.getStreamVolume(android.media.AudioManager.STREAM_MUSIC)
            mgr.setStreamVolume(android.media.AudioManager.STREAM_MUSIC, 0, 0)
        } catch (e: Exception) {
            Log.w(tag, "Не приглушил: $e")
        }
        val cap = AudioCapture(projection, object : AudioCapture.Listener {
            override fun onPcm(data: ByteArray, ptsUs: Long) {
                if (sessionActive && audio) link?.offerAudio(Messages.audioFrame(data, ptsUs).encode())
            }

            override fun onError(message: String) {
                handler?.post { stopAudio() }
            }
        })
        if (!cap.start()) {
            stopAudio()
            return
        }
        audioCapture = cap
        audioActive = true
        link?.sendControl(Messages.audioConfig(AudioCapture.SAMPLE_RATE, AudioCapture.CHANNELS).encode())
    }

    private fun stopAudio() {
        audioCapture?.stop()
        audioCapture = null
        audioActive = false
    }

    private fun logHeartbeat() {
        Log.d(tag, "hb: канал=${if (link != null) "есть" else "нет"} сессия=$sessionActive отправлено=$framesSent ввод=${if (HidInput.isReady()) "готов" else "accessibility выключен"}")
    }

    private fun buildNotification(): Notification {
        val stopIntent = PendingIntent.getService(
            this, 1, Intent(this, AdbService::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("PC Dock ADB")
            .setContentText("Экран передаётся демону")
            .setSmallIcon(android.R.drawable.ic_menu_gallery)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setOngoing(true)
            .build()
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(CHANNEL_ID, "PC Dock ADB", NotificationManager.IMPORTANCE_LOW)
            getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        }
    }

    override fun onDestroy() {
        running = false
        isRunning = false
        stopAudio()
        link?.close("service destroyed")
        try {
            tcpServer?.close()
        } catch (_: Exception) {
        }
        captureThread?.quitSafely()
        super.onDestroy()
    }
}
