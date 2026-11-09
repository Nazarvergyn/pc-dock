package com.device.media

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.provider.Settings
import android.util.Log

/**
 * Открыть приложение по команде хоста (LAUNCH_APP, PROTOCOL.md «Запуск приложения (возможность 7)»).
 *
 * Сервис захвата — фоновый с точки зрения Android: с Android 10 запуск активности из фона запрещён, и
 * foreground-сервис (MediaProjection) от этого не освобождает. Освобождает разрешение «Поверх других окон»
 * (SYSTEM_ALERT_WINDOW), которое даёт пользователь в настройках — без него ответ [BLOCKED_BAL].
 * Имя пакета проверяется так же, как на хосте; своё приложение не запускается (там запрос захвата экрана).
 */
object AppLauncher {
    private const val TAG = "DeviceMedia"

    const val OK = "ok"
    const val NOT_INSTALLED = "not_installed"
    const val NO_LAUNCHER_ACTIVITY = "no_launcher_activity"
    const val BLOCKED_BAL = "blocked_bal"
    const val INVALID_PACKAGE = "invalid_package"
    const val ERROR = "error"

    /** Как valid_package хоста: латинская буква, затем буквы, цифры, '_' и '.', не длиннее 255 */
    private val PACKAGE_RE = Regex("^[A-Za-z][A-Za-z0-9_.]{0,254}$")

    fun validPackage(pkg: String): Boolean = PACKAGE_RE.matches(pkg)

    /** Можно ли открывать приложения из фона: до Android 10 — всегда, дальше — с «Поверх других окон» */
    fun canStartFromBackground(context: Context): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.Q || Settings.canDrawOverlays(context)

    /** Открыть главный экран приложения; результат — код для STATUS 18 */
    fun launch(context: Context, pkg: String): String {
        if (!validPackage(pkg)) return INVALID_PACKAGE
        if (pkg == context.packageName) return ERROR
        val pm = context.packageManager
        try {
            @Suppress("DEPRECATION")
            pm.getPackageInfo(pkg, 0)
        } catch (e: PackageManager.NameNotFoundException) {
            return NOT_INSTALLED
        }
        val intent = pm.getLaunchIntentForPackage(pkg) ?: return NO_LAUNCHER_ACTIVITY
        if (!canStartFromBackground(context)) return BLOCKED_BAL
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED)
        return try {
            context.startActivity(intent)
            Log.d(TAG, "LAUNCH_APP: открыто $pkg")
            OK
        } catch (e: ActivityNotFoundException) {
            NO_LAUNCHER_ACTIVITY
        } catch (e: SecurityException) {
            Log.w(TAG, "LAUNCH_APP $pkg: $e")
            BLOCKED_BAL
        } catch (e: Exception) {
            Log.w(TAG, "LAUNCH_APP $pkg: $e")
            ERROR
        }
    }
}
