package com.device.media

import android.util.Log
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import java.net.Socket
import java.util.ArrayDeque
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Канал к ПК: USB-аксессуар ([Link]) или TCP-сокет через ADB-проброс ([TcpLink]).
 * Сервис держит один активный канал: режимы взаимоисключающие на одном кабеле.
 */
interface HostLink {
    interface Listener {
        /** Вызывается из потока чтения. */
        fun onPacket(packet: Packet)

        /** Вызывается один раз при закрытии канала, из любого потока. */
        fun onClosed(reason: String)

        /** HID-команда из ADB-канала (по USB её глотает ядро, сюда не приходит). */
        fun onHid(cmd: HidCommand) {}
    }

    val isClosed: Boolean
    val bytesWritten: Long
    val packetsWritten: Long
    val videoDropped: Long
    val audioDropped: Long
    val maxWriteMs: Long
    val bytesRead: Long

    fun start()
    fun sendControl(bytes: ByteArray)
    fun offerVideo(bytes: ByteArray)
    fun offerAudio(bytes: ByteArray)
    fun close(reason: String)
}

/**
 * HID-команда из ADB-канала: control-пакет демона (шапка 0xC0).
 * `request` 57 = событие HID, `hidId` 1 тач / 2 клавиатура / 3 мышь.
 */
data class HidCommand(val request: Int, val hidId: Int, val payload: ByteArray)

/**
 * Канал к ПК через ADB-проброс: ServerSocket на localhost, дальше те же пакеты v2.
 * HID идёт control-пакетами с шапкой 0xC0 (по USB их глотает ядро, здесь их видит приложение
 * и переводит во ввод через accessibility-мост).
 */
class TcpLink(
    private val socket: Socket,
    private val listener: HostLink.Listener,
) : HostLink {
    private val tag = "DeviceMedia"

    private val input = DataInputStream(socket.getInputStream())
    private val output = DataOutputStream(socket.getOutputStream())
    private val lock = Object()
    private val control = ArrayDeque<ByteArray>()
    private var latestVideo: ByteArray? = null
    private val audio = ArrayDeque<ByteArray>()
    private var audioBytes = 0
    private val audioMaxBytes = 64 * 1024
    private val closed = AtomicBoolean(false)

    @Volatile
    override var bytesWritten = 0L; private set

    @Volatile
    override var packetsWritten = 0L; private set

    @Volatile
    override var videoDropped = 0L; private set

    @Volatile
    override var audioDropped = 0L; private set

    @Volatile
    override var maxWriteMs = 0L; private set

    @Volatile
    override var bytesRead = 0L; private set

    private val reader = Thread({ readLoop() }, "tcp-reader")
    private val writer = Thread({ writeLoop() }, "tcp-writer")

    override fun start() {
        reader.start()
        writer.start()
    }

    override val isClosed: Boolean get() = closed.get()

    override fun sendControl(bytes: ByteArray) {
        synchronized(lock) {
            control.addLast(bytes)
            lock.notifyAll()
        }
    }

    override fun offerVideo(bytes: ByteArray) {
        synchronized(lock) {
            if (latestVideo != null) videoDropped++
            latestVideo = bytes
            lock.notifyAll()
        }
    }

    override fun offerAudio(bytes: ByteArray) {
        synchronized(lock) {
            audio.addLast(bytes)
            audioBytes += bytes.size
            while (audioBytes > audioMaxBytes && audio.isNotEmpty()) {
                val dropped = audio.removeFirst()
                audioBytes -= dropped.size
                audioDropped++
            }
            lock.notifyAll()
        }
    }

    override fun close(reason: String) {
        if (!closed.compareAndSet(false, true)) return
        Log.w(tag, "Канал TCP закрыт: $reason")
        try {
            socket.close()
        } catch (_: IOException) {
        }
        synchronized(lock) { lock.notifyAll() }
        listener.onClosed(reason)
    }

    private fun readLoop() {
        val packets = PacketReader()
        try {
            while (!closed.get()) {
                val first = input.read()
                if (first < 0) {
                    close("конец потока чтения")
                    return
                }
                when (first) {
                    Proto.MAGIC_P.toInt() and 0xFF -> {
                        // Пакет v2: первый байт уже прочитан, дочитываем остаток шапки и тело.
                        val rest = ByteArray(Proto.HEADER_LEN - 1)
                        input.readFully(rest)
                        val header = byteArrayOf(first.toByte()) + rest
                        val payloadLen = ((header[4].toInt() and 0xFF) shl 24) or
                                ((header[5].toInt() and 0xFF) shl 16) or
                                ((header[6].toInt() and 0xFF) shl 8) or
                                (header[7].toInt() and 0xFF)
                        if (payloadLen < 0 || payloadLen > Proto.MAX_PAYLOAD) {
                            Log.w(tag, "Плохая длина пакета: $payloadLen")
                            close("плохая длина пакета")
                            return
                        }
                        val payload = ByteArray(payloadLen)
                        if (payloadLen > 0) input.readFully(payload)
                        bytesRead += Proto.HEADER_LEN + payloadLen
                        val type = header[2].toInt() and 0xFF
                        val flags = header[3].toInt() and 0xFF
                        listener.onPacket(Packet(type, flags, payload))
                        packets.clear()
                    }
                    0xC0 -> {
                        // Control-пакет демона: request u8, value/index/len u16LE, данные.
                        val h = ByteArray(7)
                        input.readFully(h)
                        val request = h[0].toInt() and 0xFF
                        val value = (h[1].toInt() and 0xFF) or ((h[2].toInt() and 0xFF) shl 8)
                        val index = (h[3].toInt() and 0xFF) or ((h[4].toInt() and 0xFF) shl 8)
                        val len = (h[5].toInt() and 0xFF) or ((h[6].toInt() and 0xFF) shl 8)
                        val data = ByteArray(len)
                        if (len > 0) input.readFully(data)
                        bytesRead += 1 + 7 + len
                        listener.onHid(HidCommand(request, value, data))
                        packets.clear()
                    }
                    else -> {
                        // Мусор: отдаём в перестройщик v2, он сам найдёт следующую метку.
                        packets.push(byteArrayOf(first.toByte()), 1)
                        while (true) {
                            val p = packets.next() ?: break
                            listener.onPacket(p)
                        }
                    }
                }
            }
        } catch (e: IOException) {
            close("ошибка чтения: ${e.message}")
        } catch (e: Exception) {
            Log.e(tag, "Сбой потока чтения TCP", e)
            close("сбой чтения: $e")
        }
    }

    private fun writeLoop() {
        try {
            while (true) {
                val item: ByteArray = synchronized(lock) {
                    while (!closed.get() && control.isEmpty() && latestVideo == null && audio.isEmpty()) lock.wait()
                    if (closed.get()) return
                    if (control.isNotEmpty()) {
                        control.removeFirst()
                    } else if (audio.isNotEmpty()) {
                        val a = audio.removeFirst()
                        audioBytes -= a.size
                        a
                    } else {
                        val v = latestVideo!!
                        latestVideo = null
                        v
                    }
                }
                val t0 = System.nanoTime()
                output.write(item)
                output.flush()
                val ms = (System.nanoTime() - t0) / 1_000_000
                if (ms > maxWriteMs) maxWriteMs = ms
                bytesWritten += item.size
                packetsWritten++
            }
        } catch (e: IOException) {
            close("ошибка записи: ${e.message}")
        } catch (e: InterruptedException) {
            close("поток записи прерван")
        }
    }
}
