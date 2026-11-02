package com.device.media

import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat

/**
 * Одноразовая точка входа: запрашивает захват экрана и запускает постоянный сервис.
 * Если сервис уже работает, сразу закрывается. Всем USB занимается сам сервис.
 */
class MainActivity : AppCompatActivity() {

    private lateinit var projectionManager: MediaProjectionManager
    private val TAG = "DeviceMedia"

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
