package com.device.media

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.projection.MediaProjection
import android.os.Build
import android.os.SystemClock
import android.util.Log
import androidx.annotation.RequiresApi

/**
 * Захват звука приложений через ту же MediaProjection, что и видео (Android 10+).
 * Отдельного диалога не нужно: разрешение на проекцию уже дано.
 *
 * Формат фиксированный: PCM s16le, 48000 Гц, стерео. Кадр ~20мс (3840 байт).
 * Приложения могут запретить захват своего звука — тогда придёт тишина.
 */
@RequiresApi(Build.VERSION_CODES.Q)
class AudioCapture(
    private val projection: MediaProjection,
    private val listener: Listener,
) {
    interface Listener {
        fun onPcm(data: ByteArray, ptsUs: Long)
        fun onError(message: String)
    }

    companion object {
        const val SAMPLE_RATE = 48000
        const val CHANNELS = 2
        private const val FRAME_MS = 20
        private const val FRAME_BYTES = SAMPLE_RATE * CHANNELS * 2 * FRAME_MS / 1000
    }

    private val tag = "DeviceMedia"
    private var record: AudioRecord? = null
    private var thread: Thread? = null
    @Volatile
    private var running = false

    fun start(): Boolean {
        if (running) return true
        try {
            val capture = AudioPlaybackCaptureConfiguration.Builder(projection)
                .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
                .addMatchingUsage(AudioAttributes.USAGE_GAME)
                .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
                .build()
            val format = AudioFormat.Builder()
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .setSampleRate(SAMPLE_RATE)
                .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
                .build()
            val minBuf = AudioRecord.getMinBufferSize(
                SAMPLE_RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT
            )
            if (minBuf <= 0) {
                listener.onError("AudioRecord.getMinBufferSize вернул $minBuf")
                return false
            }
            val rec = AudioRecord.Builder()
                .setAudioPlaybackCaptureConfig(capture)
                .setAudioFormat(format)
                .setBufferSizeInBytes(maxOf(minBuf * 4, FRAME_BYTES * 4))
                .build()
            if (rec.state != AudioRecord.STATE_INITIALIZED) {
                listener.onError("AudioRecord не инициализирован")
                try {
                    rec.release()
                } catch (_: Exception) {
                }
                return false
            }
            rec.startRecording()
            if (rec.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
                listener.onError("AudioRecord не начал запись")
                try {
                    rec.stop()
                } catch (_: Exception) {
                }
                rec.release()
                return false
            }
            record = rec
            running = true
            thread = Thread({ readLoop() }, "audio-capture").also { it.start() }
            Log.d(tag, "Захват звука запущен: PCM $SAMPLE_RATE Гц стерео, кадр ${FRAME_BYTES}Б")
            return true
        } catch (e: SecurityException) {
            listener.onError("нет разрешения на захват звука: ${e.message}")
            return false
        } catch (e: Exception) {
            listener.onError("не удалось запустить захват звука: $e")
            return false
        }
    }

    private fun readLoop() {
        val buf = ByteArray(FRAME_BYTES)
        try {
            while (running) {
                val rec = record ?: break
                var off = 0
                while (off < buf.size && running) {
                    val n = rec.read(buf, off, buf.size - off)
                    if (n < 0) {
                        val msg = when (n) {
                            AudioRecord.ERROR_INVALID_OPERATION -> "invalid operation"
                            AudioRecord.ERROR_BAD_VALUE -> "bad value"
                            AudioRecord.ERROR_DEAD_OBJECT -> "dead object"
                            else -> "код $n"
                        }
                        listener.onError("AudioRecord.read вернул $msg")
                        return
                    }
                    if (n == 0) break
                    off += n
                }
                if (off > 0) {
                    val ptsUs = SystemClock.elapsedRealtimeNanos() / 1000
                    listener.onPcm(if (off == buf.size) buf.copyOf() else buf.copyOf(off), ptsUs)
                }
            }
        } catch (e: Exception) {
            if (running) listener.onError("сбой чтения звука: $e")
        } finally {
            try {
                record?.stop()
            } catch (_: Exception) {
            }
            try {
                record?.release()
            } catch (_: Exception) {
            }
            record = null
        }
    }

    fun stop() {
        running = false
        try {
            record?.stop()
        } catch (_: Exception) {
        }
        thread?.interrupt()
        thread = null
    }
}
