package com.device.media

import android.os.ParcelFileDescriptor
import android.util.Log
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.util.ArrayDeque
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Канал к ПК поверх USB-аксессуара: отдельные потоки чтения и записи.
 *
 * Запись в USB блокируется, пока ПК не читает (наблюдалось до 109 с), поэтому она идёт в своём потоке:
 * управляющие пакеты уходят в порядке очереди, а видео держится как «последний кадр» — старые отбрасываются.
 */
class Link(
    private val pfd: ParcelFileDescriptor,
    private val listener: HostLink.Listener,
) : HostLink {

    private val tag = "DeviceMedia"
    private val input = FileInputStream(pfd.fileDescriptor)
    private val output = FileOutputStream(pfd.fileDescriptor)
    private val lock = Object()
    private val control = ArrayDeque<ByteArray>()
    private var latestVideo: ByteArray? = null
    private val audio = ArrayDeque<ByteArray>()
    private var audioBytes = 0

    /** Очередь звука держим короткой (~200мс PCM 48кГц стерео 16бит = ~38КБ): старое отбрасываем. */
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

    private val reader = Thread({ readLoop() }, "usb-reader")
    private val writer = Thread({ writeLoop() }, "usb-writer")

    override fun start() {
        reader.start()
        writer.start()
    }

    override val isClosed: Boolean get() = closed.get()

    /** Управляющий пакет или любые байты, которые нельзя терять. */
    override fun sendControl(bytes: ByteArray) {
        synchronized(lock) {
            control.addLast(bytes)
            lock.notifyAll()
        }
    }

    /** Кадр экрана: заменяет предыдущий, ещё не отправленный. */
    override fun offerVideo(bytes: ByteArray) {
        synchronized(lock) {
            if (latestVideo != null) videoDropped++
            latestVideo = bytes
            lock.notifyAll()
        }
    }

    /** Звук: очередь FIFO, при переполнении самые старые пакеты отбрасываются (PROTOCOL.md §9). */
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
        Log.w(tag, "Канал USB закрыт: $reason")
        try {
            input.close()
        } catch (_: IOException) {
        }
        try {
            output.close()
        } catch (_: IOException) {
        }
        try {
            pfd.close()
        } catch (_: IOException) {
        }
        synchronized(lock) { lock.notifyAll() }
        listener.onClosed(reason)
    }

    private fun readLoop() {
        val packets = PacketReader()
        val buf = ByteArray(16 * 1024)
        try {
            while (!closed.get()) {
                val n = input.read(buf)
                if (n < 0) {
                    close("конец потока чтения")
                    return
                }
                bytesRead += n
                packets.push(buf, n)
                while (true) {
                    val p = packets.next() ?: break
                    listener.onPacket(p)
                }
            }
        } catch (e: IOException) {
            close("ошибка чтения: ${e.message}")
        } catch (e: Exception) {
            Log.e(tag, "Сбой потока чтения", e)
            close("сбой чтения: $e")
        }
    }

    private fun writeLoop() {
        try {
            while (true) {
                val item: ByteArray = synchronized(lock) {
                    while (!closed.get() && control.isEmpty() && latestVideo == null && audio.isEmpty()) lock.wait()
                    if (closed.get()) return
                    // Приоритет: управляющие пакеты (нельзя терять) > звук (маленький, срочный)
                    // > видео (большое, хранится только последнее). USB быстрый, голодания нет.
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
