package com.device.server

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioRecord
import android.os.Build
import android.os.Looper
import android.os.Parcel
import android.os.SystemClock
import android.util.Log
import java.lang.ref.WeakReference

/**
 * Звук телефона без приложения и без диалога: `AudioRecord` с источником REMOTE_SUBMIX (то, что играет
 * телефон). Из процесса shell штатный конструктор не работает, поэтому объект собирается вручную через
 * закрытый `native_setup`, как у scrcpy; сигнатура зависит от версии Android, аргументы собираются по
 * списку параметров метода. PCM 16 бит, 48 кГц, стерео.
 */
class AudioCap(
    private val onConfig: (rate: Int, channels: Int) -> Unit,
    private val onAudio: (pcm: ByteArray, ptsUs: Long) -> Unit,
    private val onUnavailable: (why: String) -> Unit,
) {
    private val tag = "PcDockServer"

    @Volatile
    private var running = false
    private var thread: Thread? = null

    fun start() {
        if (running) return
        running = true
        thread = Thread({ loop() }, "audio").also { it.start() }
    }

    fun stop() {
        running = false
        thread?.interrupt()
        thread = null
    }

    private fun loop() {
        var record: AudioRecord? = null
        try {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) {
                onUnavailable("захват звука требует Android 11+")
                return
            }
            record = create()
            record.startRecording()
            onConfig(RATE, CHANNELS)
            val chunk = ByteArray(RATE / 50 * CHANNELS * 2) // 20 мс
            while (running) {
                val n = record.read(chunk, 0, chunk.size)
                if (n > 0) onAudio(chunk.copyOf(n), SystemClock.elapsedRealtime() * 1000)
                else if (n < 0) {
                    onUnavailable("чтение звука: $n")
                    break
                }
            }
        } catch (e: Throwable) {
            Log.e(tag, "Звук недоступен", e)
            onUnavailable("$e")
        } finally {
            try {
                record?.stop()
                record?.release()
            } catch (_: Throwable) {
            }
            running = false
        }
    }

    private fun field(name: String) = AudioRecord::class.java.getDeclaredField(name).apply { isAccessible = true }

    private fun create(): AudioRecord {
        val ctor = AudioRecord::class.java.getDeclaredConstructor(Long::class.javaPrimitiveType).apply { isAccessible = true }
        val record = ctor.newInstance(0L)
        field("mRecordingState").setInt(record, AudioRecord.RECORDSTATE_STOPPED)
        field("mInitializationLooper").set(record, Looper.myLooper() ?: Looper.getMainLooper())
        val attrs = AudioAttributes.Builder().apply {
            AudioAttributes.Builder::class.java.getMethod("setInternalCapturePreset", Int::class.javaPrimitiveType)
                .invoke(this, SOURCE_REMOTE_SUBMIX)
        }.build()
        field("mAudioAttributes").set(record, attrs)
        AudioRecord::class.java.getDeclaredMethod(
            "audioParamCheck", Int::class.javaPrimitiveType, Int::class.javaPrimitiveType, Int::class.javaPrimitiveType,
        ).apply { isAccessible = true }.invoke(record, SOURCE_REMOTE_SUBMIX, RATE, ENCODING)
        field("mChannelCount").setInt(record, CHANNELS)
        field("mChannelMask").setInt(record, CHANNEL_MASK)
        val bufSize = AudioRecord.getMinBufferSize(RATE, CHANNEL_MASK, ENCODING) * 8
        AudioRecord::class.java.getDeclaredMethod("audioBuffSizeCheck", Int::class.javaPrimitiveType)
            .apply { isAccessible = true }.invoke(record, bufSize)

        val session = intArrayOf(0) // AUDIO_SESSION_ID_GENERATE
        val setup = AudioRecord::class.java.declaredMethods.first { it.name == "native_setup" }.apply { isAccessible = true }
        val format = AudioFormat.Builder().setEncoding(ENCODING).setSampleRate(RATE).setChannelMask(CHANNEL_MASK).build()
        val args = ArrayList<Any?>()
        val ints = arrayListOf(CHANNEL_MASK, 0, format.encoding, bufSize)
        var arrays = 0
        for (t in setup.parameterTypes) {
            args += when {
                t == Any::class.java && args.isEmpty() -> WeakReference(record)
                t == Any::class.java -> attrs
                t == IntArray::class.java -> if (arrays++ == 0) intArrayOf(RATE) else session
                t == String::class.java -> Hidden.PACKAGE
                t == Parcel::class.java -> attributionParcel()
                t == Long::class.javaPrimitiveType -> 0L
                t == Int::class.javaPrimitiveType -> if (ints.isNotEmpty()) ints.removeAt(0) else 0
                else -> throw IllegalStateException("native_setup: неизвестный параметр $t")
            }
        }
        val result = setup.invoke(record, *args.toTypedArray()) as Int
        if (result != AudioRecord.SUCCESS) throw IllegalStateException("native_setup вернул $result")
        field("mSampleRate").setInt(record, RATE)
        field("mSessionId").setInt(record, session[0])
        field("mState").setInt(record, AudioRecord.STATE_INITIALIZED)
        return record
    }

    /** `AttributionSource` shell в виде Parcel (Android 12+). */
    private fun attributionParcel(): Parcel {
        val cls = Class.forName("android.content.AttributionSource")
        val b = Class.forName("android.content.AttributionSource\$Builder")
            .getConstructor(Int::class.javaPrimitiveType).newInstance(SHELL_UID)
        b.javaClass.getMethod("setPackageName", String::class.java).invoke(b, Hidden.PACKAGE)
        val source = b.javaClass.getMethod("build").invoke(b)
        val state = cls.getMethod("asScopedParcelState").invoke(source)
        return state!!.javaClass.getMethod("getParcel").invoke(state) as Parcel
    }

    companion object {
        const val RATE = 48000
        const val CHANNELS = 2
        private const val SOURCE_REMOTE_SUBMIX = 8
        private const val ENCODING = AudioFormat.ENCODING_PCM_16BIT
        private const val CHANNEL_MASK = AudioFormat.CHANNEL_IN_STEREO
        private const val SHELL_UID = 2000
    }
}
