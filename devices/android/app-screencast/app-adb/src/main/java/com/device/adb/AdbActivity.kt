package com.device.adb

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity

/**
 * Одноразовая точка входа ADB-приложения: запрос захвата, запуск [AdbService].
 * Стартует с ПК (`adb shell am start`) или вручную; иконки в лаунчере нет.
 */
class AdbActivity : AppCompatActivity() {
    private val tag = "AdbDaemon"

    private val requestProjection = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == Activity.RESULT_OK && result.data != null) {
            Log.d(tag, "MediaProjection granted, запускаю сервис")
            val intent = Intent(this, AdbService::class.java).apply {
                putExtra("resultCode", result.resultCode)
                putExtra("media_data", result.data)
            }
            startForegroundService(intent)
        } else {
            Log.e(tag, "Захват экрана отклонён")
        }
        finish()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (AdbService.isRunning) {
            Log.d(tag, "Сервис уже работает, закрываюсь")
            finish()
            return
        }
        val pm = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        requestProjection.launch(pm.createScreenCaptureIntent())
    }
}
