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
        Looper.prepareMainLooper()
        val server = Server()
        Thread({ server.listen(socketName) }, "accept").start()
        Looper.loop()
    }

    class Server : HostLink.Listener {
        private val screen = ScreenInfo()
        private val injector = Injector(screen)

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
                    // Видео, звук и остальное — отдельный шаг (захват экрана), пока молча принимаем.
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
                link = null
            }
        }

        private fun onHello() {
            val l = link ?: return
            l.sendControl(Messages.hello(Proto.CAP_ROTATION, NAME).encode())
            val d = screen.current()
            l.sendControl(
                Messages.displayInfo(d.width / 2 and 1.inv(), d.height / 2 and 1.inv(), d.width, d.height, d.dpi, d.rotation)
                    .encode(),
            )
        }
    }
}
