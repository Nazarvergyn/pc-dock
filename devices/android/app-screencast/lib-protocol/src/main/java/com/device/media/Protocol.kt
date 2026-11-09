package com.device.media

import java.nio.ByteBuffer

/**
 * Протокол PC Dock v2 (см. devices/android/PROTOCOL.md). Все числа big-endian, строки UTF-8.
 * Формат пакета: "PD", тип (1 байт), флаги (1 байт), длина (u32), данные.
 */
object Proto {
    const val VERSION = 2
    const val HEADER_LEN = 8
    const val MAX_PAYLOAD = 16 * 1024 * 1024
    const val MAGIC_P: Byte = 0x50
    const val MAGIC_D: Byte = 0x44

    const val T_HELLO = 0x01
    const val T_PING = 0x02
    const val T_PONG = 0x03
    const val T_DISPLAY_INFO = 0x10
    const val T_VIDEO_FRAME = 0x11
    const val T_AUDIO_CONFIG = 0x20
    const val T_AUDIO_FRAME = 0x21
    const val T_REQUEST_FRAME = 0x40
    const val T_SET_STREAM = 0x41
    const val T_WAKE = 0x42
    const val T_STATUS = 0x50
    const val T_ERROR = 0x7F

    const val CAP_VIDEO_JPEG = 1 shl 0
    const val CAP_AUDIO_PCM = 1 shl 2
    const val CAP_ROTATION = 1 shl 4

    const val CODEC_VIDEO_JPEG = 1
    const val CODEC_AUDIO_PCM_S16LE = 1

    const val ERR_UNSUPPORTED_VERSION = 1
    const val ERR_BAD_PACKET = 2

    const val STATUS_PROJECTION_STOPPED = 1
    const val STATUS_AUDIO_UNAVAILABLE = 2
    const val STATUS_INPUT_UNSUPPORTED = 3
}

class Packet(val type: Int, val flags: Int, val payload: ByteArray) {
    fun encode(): ByteArray {
        val buf = ByteBuffer.allocate(Proto.HEADER_LEN + payload.size)
        buf.put(Proto.MAGIC_P).put(Proto.MAGIC_D).put(type.toByte()).put(flags.toByte())
        buf.putInt(payload.size).put(payload)
        return buf.array()
    }
}

/** Потоковый разборщик с перестройкой после мусора (PROTOCOL.md §3). Не потокобезопасен. */
class PacketReader {
    private var buf = ByteArray(1 shl 16)
    private var len = 0
    private var resync = false
    var dropped = 0L
        private set

    fun push(src: ByteArray, n: Int) {
        if (len + n > buf.size) buf = buf.copyOf(maxOf(len + n, buf.size * 2))
        System.arraycopy(src, 0, buf, len, n)
        len += n
    }

    fun clear() {
        len = 0
        resync = false
    }

    private fun consume(n: Int) {
        System.arraycopy(buf, n, buf, 0, len - n)
        len -= n
    }

    /** Отбрасывает байты, которые не удалось разобрать. */
    private fun drop(n: Int) {
        consume(n)
        dropped += n
    }

    private fun skipToCandidate() {
        resync = true
        var pos = -1
        for (i in 1 until len - 1) {
            if (buf[i] == Proto.MAGIC_P && buf[i + 1] == Proto.MAGIC_D) {
                pos = i
                break
            }
        }
        if (pos >= 0) {
            drop(pos)
        } else {
            val keep = if (buf[len - 1] == Proto.MAGIC_P) 1 else 0
            drop(len - keep)
        }
    }

    fun next(): Packet? {
        while (true) {
            if (len < Proto.HEADER_LEN) return null
            if (buf[0] != Proto.MAGIC_P || buf[1] != Proto.MAGIC_D) {
                skipToCandidate()
                continue
            }
            val payloadLen = ByteBuffer.wrap(buf, 4, 4).int
            if (payloadLen < 0 || payloadLen > Proto.MAX_PAYLOAD) {
                resync = true
                drop(1)
                continue
            }
            val total = Proto.HEADER_LEN + payloadLen
            if (len < total) return null
            if (resync) {
                if (len >= total + 2 && (buf[total] != Proto.MAGIC_P || buf[total + 1] != Proto.MAGIC_D)) {
                    drop(1)
                    continue
                }
                resync = false
            }
            val packet =
                Packet(buf[2].toInt() and 0xFF, buf[3].toInt() and 0xFF, buf.copyOfRange(Proto.HEADER_LEN, total))
            consume(total)
            return packet
        }
    }
}

class HostHello(val versionMax: Int, val versionMin: Int, val capabilities: Int, val role: Int, val name: String)

class SetStream(
    val video: Boolean,
    val audio: Boolean,
    val maxWidth: Int,
    val maxHeight: Int,
    val jpegQuality: Int,
    val maxFps: Int,
)

object Messages {
    fun hello(capabilities: Int, name: String): Packet {
        val nameBytes = name.toByteArray(Charsets.UTF_8).let { if (it.size > 255) it.copyOf(255) else it }
        val b = ByteBuffer.allocate(10 + nameBytes.size)
        b.putShort(Proto.VERSION.toShort()).putShort(Proto.VERSION.toShort()).putInt(capabilities)
        b.put(1) // роль: телефон
        b.put(nameBytes.size.toByte()).put(nameBytes)
        return Packet(Proto.T_HELLO, 0, b.array())
    }

    fun parseHello(payload: ByteArray): HostHello? = try {
        val b = ByteBuffer.wrap(payload)
        val vMax = b.short.toInt() and 0xFFFF
        val vMin = b.short.toInt() and 0xFFFF
        val caps = b.int
        val role = b.get().toInt()
        val n = b.get().toInt() and 0xFF
        val name = ByteArray(n).also { b.get(it) }.toString(Charsets.UTF_8)
        HostHello(vMax, vMin, caps, role, name)
    } catch (e: Exception) {
        null
    }

    fun displayInfo(streamW: Int, streamH: Int, screenW: Int, screenH: Int, dpi: Int, rotation: Int): Packet {
        val b = ByteBuffer.allocate(12)
        for (v in intArrayOf(streamW, streamH, screenW, screenH, dpi)) b.putShort(v.toShort())
        b.put(rotation.toByte()).put(0)
        return Packet(Proto.T_DISPLAY_INFO, 0, b.array())
    }

    fun videoFrame(jpeg: ByteArray, ptsUs: Long): Packet {
        val b = ByteBuffer.allocate(9 + jpeg.size)
        b.put(Proto.CODEC_VIDEO_JPEG.toByte()).putLong(ptsUs).put(jpeg)
        return Packet(Proto.T_VIDEO_FRAME, 1, b.array())
    }

    fun pong(nonce: ByteArray): Packet = Packet(Proto.T_PONG, 0, nonce)

    fun audioConfig(sampleRate: Int, channels: Int): Packet {
        val b = ByteBuffer.allocate(6)
        b.put(Proto.CODEC_AUDIO_PCM_S16LE.toByte()).putInt(sampleRate).put(channels.toByte())
        return Packet(Proto.T_AUDIO_CONFIG, 0, b.array())
    }

    fun audioFrame(pcm: ByteArray, ptsUs: Long): Packet {
        val b = ByteBuffer.allocate(8 + pcm.size)
        b.putLong(ptsUs).put(pcm)
        return Packet(Proto.T_AUDIO_FRAME, 0, b.array())
    }

    fun status(code: Int, detail: String): Packet {
        val d = detail.toByteArray(Charsets.UTF_8)
        val b = ByteBuffer.allocate(2 + d.size)
        b.putShort(code.toShort()).put(d)
        return Packet(Proto.T_STATUS, 0, b.array())
    }

    fun error(code: Int, message: String): Packet {
        val d = message.toByteArray(Charsets.UTF_8)
        val b = ByteBuffer.allocate(2 + d.size)
        b.putShort(code.toShort()).put(d)
        return Packet(Proto.T_ERROR, 0, b.array())
    }

    fun parseSetStream(payload: ByteArray): SetStream? = try {
        val b = ByteBuffer.wrap(payload)
        val flags = b.get().toInt()
        SetStream(
            video = flags and 1 != 0,
            audio = flags and 2 != 0,
            maxWidth = b.short.toInt() and 0xFFFF,
            maxHeight = b.short.toInt() and 0xFFFF,
            jpegQuality = b.get().toInt() and 0xFF,
            maxFps = b.get().toInt() and 0xFF,
        )
    } catch (e: Exception) {
        null
    }

}
