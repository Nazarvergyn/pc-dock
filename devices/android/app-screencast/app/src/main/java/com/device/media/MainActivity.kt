package com.device.media

import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.util.Log
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat

/**
 * Одноразовая точка входа: запрашивает захват экрана и запускает постоянный сервис.
 * Если сервис уже работает, сразу закрывается. Всем USB занимается сам сервис.
 */
class MainActivity : AppCompatActivity() {

    private lateinit var projectionManager: MediaProjectionManager
    private val TAG = "DeviceMedia"

    /** Ушли в настройки «Доступа к истории использования» — по возвращении продолжить запуск */
    private var backFromUsageSettings = false
    private var usageAsked = false

    private val requestNotificationPermission = registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { isGranted ->
        Log.d(TAG, "POST_NOTIFICATIONS granted=$isGranted")
        requestProjection()
    }

    private val requestMediaProjection = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == RESULT_OK && result.data != null) {
            Log.d(TAG, "MediaProjection granted, запускаю сервис")
            val intent = Intent(this, ScreenCaptureService::class.java).apply {
                putExtra("resultCode", result.resultCode)
                putExtra("media_data", result.data)
            }
            startForegroundService(intent)
        } else {
            Log.e(TAG, "MediaProjection denied or cancelled: resultCode=${result.resultCode}")
            Toast.makeText(this, "Захват экрана отменён", Toast.LENGTH_SHORT).show()
        }
        finish()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Log.d(
            TAG, "MainActivity onCreate: intent.action=${intent?.action} " +
                    "device=${Build.MANUFACTURER} ${Build.MODEL} sdk=${Build.VERSION.SDK_INT}"
        )
        projectionManager = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        handleLaunch()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        Log.d(TAG, "MainActivity onNewIntent: action=${intent.action}")
        handleLaunch()
    }

    private fun handleLaunch() {
        // Открыли с иконки, а «Доступа к истории использования» нет — объяснить и предложить открыть настройки
        // (нужен хосту, чтобы знать приложение на переднем плане; при подключении по USB не спрашиваем)
        if (intent?.action == Intent.ACTION_MAIN && !usageAsked && !ForegroundWatch.hasUsageAccess(this)) {
            usageAsked = true
            AlertDialog.Builder(this)
                .setTitle("Доступ к истории использования")
                .setMessage(
                    "Компьютер (например, pynelo) запускает сценарии только в разрешённых приложениях и должен " +
                        "видеть, какое приложение открыто на экране. Для этого разрешите «Device Media» доступ к " +
                        "истории использования: в открывшемся списке выберите «Device Media» → «Разрешить». " +
                        "Приложение узнаёт только имя открытого приложения и ничего не сохраняет."
                )
                .setPositiveButton("Открыть настройки") { _, _ ->
                    backFromUsageSettings = true
                    try {
                        startActivity(Intent(Settings.ACTION_USAGE_ACCESS_SETTINGS))
                    } catch (e: Exception) {
                        Log.w(TAG, "Нет экрана доступа к истории использования: $e")
                        backFromUsageSettings = false
                        continueLaunch()
                    }
                }
                .setNegativeButton("Позже") { _, _ -> continueLaunch() }
                .setCancelable(false)
                .show()
            return
        }
        continueLaunch()
    }

    override fun onRestart() {
        super.onRestart()
        if (backFromUsageSettings) {
            backFromUsageSettings = false
            continueLaunch()
        }
    }

    private fun continueLaunch() {
        // Сервис жив и проекция жива — делать нечего. Если проекция умерла
        // (система её остановила), запрашиваем разрешение заново в живой сервис.
        if (ScreenCaptureService.isRunning && !ScreenCaptureService.needsProjection()) {
            Log.d(TAG, "Сервис уже работает, закрываюсь")
            finish()
            return
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(this, android.Manifest.permission.POST_NOTIFICATIONS)
            != PackageManager.PERMISSION_GRANTED
        ) {
            requestNotificationPermission.launch(android.Manifest.permission.POST_NOTIFICATIONS)
            return
        }
        requestProjection()
    }

    private fun requestProjection() {
        requestMediaProjection.launch(projectionManager.createScreenCaptureIntent())
    }

    override fun onDestroy() {
        Log.d(TAG, "MainActivity onDestroy")
        super.onDestroy()
    }
}
