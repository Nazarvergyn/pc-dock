package com.device.server

import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.Looper
import android.os.Process
import android.util.Log
import com.device.media.HidCommand
import com.device.media.HostLink
import com.device.media.Messages
import com.device.media.Packet
import com.device.media.Proto
import com.device.media.TcpLink
import java.util.concurrent.LinkedBlockingQueue

/**
 * Сервер экрана и ввода для ADB-режима (решение 32). Запуск с ПК:
 * `adb shell CLASSPATH=/data/local/tmp/pcdock-server.apk app_process / com.device.server.Main [имя-сокета]`.
 *
 * Слушает abstract unix-сокет (по умолчанию `pcdock`); ПК пробрасывает его через
 * `adb forward tcp:N localabstract:pcdock`. Абстрактный сокет открыт всем приложениям, поэтому
 * владельца соединения проверяем по uid (shell или свой), чужие соединения закрываем.
 */
object Main {
    private const val TAG = "PcDockServer"
    private const val NAME = "pcdock-server/1.0"
    private const val SHELL_UID = 2000

    @JvmStatic
    fun main(args: Array<String>) {
        val socketName = args.firstOrNull() ?: "pcdock"
        // ПК держит поток этого процесса открытым: закрылся (выход, обрыв) — завершаемся, чтобы сервер не висел.
        Thread({
            try {
                while (System.`in`.read() >= 0) {
                    // игнорируем содержимое
                }
            } catch (_: Exception) {
            }
            Log.i(TAG, "stdin закрыт, завершаюсь")
            System.exit(0)
        }, "stdin-watch").apply { isDaemon = true }.start()
        Looper.prepareMainLooper()
        val server = Server()
        Thread({ server.listen(socketName) }, "accept").start()
        Looper.loop()
    }

    class Server : HostLink.Listener {
        private val screen = ScreenInfo()
        private val injector = Injector(screen)
        private val capture = Capture(
            screen,
            onFrame = { jpeg, pts -> link?.offerVideo(Messages.videoFrame(jpeg, pts).encode()) },
            onGeometry = { events.add { sendDisplayInfo() } },
        )
        private var videoOn = false

        // Всё, что трогает ввод и состояние сессии, выполняется одним потоком.
        private val events = LinkedBlockingQueue<() -> Unit>()

        @Volatile
        private var link: HostLink? = null

        init {
            Thread({
                while (true) {
                    try {
                        events.take()()
                    } catch (e: Throwable) {
                        Log.e(TAG, "Сбой обработчика", e)
                    }
                }
            }, "events").apply { isDaemon = true }.start()
        }

        fun listen(name: String) {
            val server = LocalServerSocket(name)
            Log.i(TAG, "Слушаю localabstract:$name")
            while (true) {
                val socket = server.accept()
                if (!allowed(socket)) {
                    Log.w(TAG, "Чужое соединение отклонено")
                    socket.close()
                    continue
                }
                attach(socket)
            }
        }

        private fun allowed(socket: LocalSocket): Boolean {
            val uid = socket.peerCredentials.uid
            return uid == SHELL_UID || uid == Process.myUid() || uid == 0
        }

        private fun attach(socket: LocalSocket) {
            link?.close("новое соединение")
            val l = TcpLink(socket.inputStream, socket.outputStream, socket, this)
            link = l
            Log.i(TAG, "ПК подключён, жду HELLO")
            l.start()
        }

        override fun onPacket(packet: Packet) {
            events.add {
                when (packet.type) {
                    Proto.T_HELLO -> onHello()
                    Proto.T_PING -> link?.sendControl(Messages.pong(packet.payload).encode())
                    Proto.T_SET_STREAM -> Messages.parseSetStream(packet.payload)?.let { onSetStream(it) }
                    Proto.T_REQUEST_FRAME -> capture.resend()
                    Proto.T_WAKE -> injector.wake()
                    // Звук и побудка — отдельные шаги.
                    else -> {}
                }
            }
        }

        override fun onHid(cmd: HidCommand) {
            events.add {
                if (cmd.request != 57) return@add
                val unsupported = injector.handle(cmd.hidId, cmd.payload)
                if (unsupported != null) {
                    Log.w(TAG, "Ввод не поддержан: $unsupported")
                    link?.sendControl(
                        Messages.status(Proto.STATUS_INPUT_UNSUPPORTED, "input unsupported: $unsupported").encode(),
                    )
                }
            }
        }

        override fun onClosed(reason: String) {
            events.add {
                Log.i(TAG, "Канал закрыт: $reason")
                injector.releaseAll()
                capture.stop()
                videoOn = false
                link = null
            }
        }

        private fun onSetStream(st: com.device.media.SetStream) {
            capture.maxLongSide = maxOf(st.maxWidth, st.maxHeight)
            capture.jpegQuality = st.jpegQuality
            capture.maxFps = st.maxFps
            if (st.video && !videoOn) {
                videoOn = true
                capture.start()
            } else if (!st.video && videoOn) {
                videoOn = false
                capture.stop()
            }
            sendDisplayInfo()
        }

        private fun onHello() {
            val l = link ?: return
            l.sendControl(Messages.hello(Proto.CAP_VIDEO_JPEG or Proto.CAP_ROTATION, NAME).encode())
            sendDisplayInfo()
        }

        private fun sendDisplayInfo() {
            val l = link ?: return
            val d = screen.current()
            val (w, h) = capture.streamSize(d)
            l.sendControl(Messages.displayInfo(w, h, d.width, d.height, d.dpi, d.rotation).encode())
        }
    }
}
