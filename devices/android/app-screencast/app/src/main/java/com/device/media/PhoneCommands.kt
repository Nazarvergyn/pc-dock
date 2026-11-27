package com.device.media

import android.app.ActivityManager
import android.app.AppOpsManager
import android.app.usage.UsageEvents
import android.app.usage.UsageStatsManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.BatteryManager
import android.os.Build
import android.os.PowerManager
import android.os.Process
import org.json.JSONArray
import org.json.JSONObject

class CommandResultData(val ok: Boolean, val body: String)

/**
 * Команды от ПК (T_COMMAND, PROTOCOL.md §9): то, что возможно без ADB обычному приложению.
 * Всё, что Android запрещает приложению без особых прав, отвечает ошибкой с причиной, а не молчит.
 */
object PhoneCommands {
    fun run(ctx: Context, text: String): CommandResultData = try {
        val j = JSONObject(text)
        val data = when (val op = j.getString("op")) {
            "Info" -> info(ctx)
            "LaunchApp" -> launch(ctx, j.getString("package"), j.optString("activity").ifEmpty { null })
            "ForceStop" -> forceStop(ctx, j.getString("package"))
            "ListApps" -> listApps(ctx, j.optBoolean("all", false))
            "AppInfo" -> appInfo(ctx, j.getJSONArray("packages"))
            "ForegroundApp" -> foreground(ctx)
            "GetClipboard" -> JSONObject().put("text", getClipboard(ctx))
            "SetClipboard" -> setClipboard(ctx, j.getString("text"))
            "Button" -> button(ctx, j.getString("name"))
            "ScreenState" -> screenState(ctx)
            "NetworkInfo" -> network(ctx)
            else -> throw IllegalArgumentException("компаньон не умеет $op")
        }
        CommandResultData(true, data.toString())
    } catch (e: Throwable) {
        CommandResultData(false, e.message ?: e.javaClass.simpleName)
    }

    /** Время как у ADB-пути: `гггг-мм-дд чч:мм:сс` по часам телефона. */
    private fun fmt(ms: Long): String =
        java.text.SimpleDateFormat("yyyy-MM-dd HH:mm:ss", java.util.Locale.US).format(java.util.Date(ms))

    private fun info(ctx: Context): JSONObject {
        val bm = ctx.getSystemService(Context.BATTERY_SERVICE) as BatteryManager
        val dm = ctx.resources.displayMetrics
        // Полный размер экрана, а не область без панелей (как `wm size` по ADB).
        val full = if (Build.VERSION.SDK_INT >= 30) {
            (ctx.getSystemService(Context.WINDOW_SERVICE) as android.view.WindowManager).maximumWindowMetrics.bounds
        } else {
            null
        }
        return JSONObject()
            .put("model", Build.MODEL).put("manufacturer", Build.MANUFACTURER)
            .put("android", Build.VERSION.RELEASE).put("sdk", Build.VERSION.SDK_INT)
            .put("screen_w", full?.width() ?: dm.widthPixels).put("screen_h", full?.height() ?: dm.heightPixels).put("dpi", dm.densityDpi)
            .put("battery", JSONObject().put("level", bm.getIntProperty(BatteryManager.BATTERY_PROPERTY_CAPACITY)))
    }

    private fun launch(ctx: Context, pkg: String, activity: String?): JSONObject {
        val intent = if (activity != null) {
            Intent(Intent.ACTION_MAIN).setClassName(pkg, if (activity.startsWith(".")) pkg + activity else activity)
        } else {
            ctx.packageManager.getLaunchIntentForPackage(pkg) ?: throw IllegalArgumentException("нет значка запуска у $pkg")
        }
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED)
        // С Android 10 запуск из фона разрешён только при показанном окне или особом праве; ошибку отдаём как есть.
        ctx.startActivity(intent)
        return JSONObject().put("package", pkg)
    }

    private fun forceStop(ctx: Context, pkg: String): JSONObject {
        // Без системных прав можно только убить фоновые процессы; приложение на экране так не закрыть.
        (ctx.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager).killBackgroundProcesses(pkg)
        return JSONObject().put("note", "убиты фоновые процессы (полная остановка требует ADB)")
    }

    private fun listApps(ctx: Context, all: Boolean): JSONObject {
        val pm = ctx.packageManager
        val names = if (all) {
            pm.getInstalledApplications(0).map { it.packageName }
        } else {
            pm.queryIntentActivities(
                Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER), 0,
            ).map { it.activityInfo.packageName }
        }
        return JSONObject().put("packages", JSONArray(names.distinct().sorted()))
    }

    private fun appInfo(ctx: Context, pkgs: JSONArray): JSONObject {
        val apps = JSONArray()
        for (i in 0 until pkgs.length()) {
            val name = pkgs.getString(i)
            val o = JSONObject().put("package", name)
            try {
                val pi = ctx.packageManager.getPackageInfo(name, 0)
                val code = if (Build.VERSION.SDK_INT >= 28) pi.longVersionCode else @Suppress("DEPRECATION") pi.versionCode.toLong()
                o.put("installed", true).put("version_code", code).put("version_name", pi.versionName)
                    .put("enabled", pi.applicationInfo?.enabled ?: true)
                    .put("first_install", fmt(pi.firstInstallTime)).put("last_update", fmt(pi.lastUpdateTime))
                    .put("system", (pi.applicationInfo?.flags ?: 0) and ApplicationInfo.FLAG_SYSTEM != 0)
            } catch (e: PackageManager.NameNotFoundException) {
                o.put("installed", false)
            }
            apps.put(o)
        }
        return JSONObject().put("apps", apps)
    }

    private fun foreground(ctx: Context): JSONObject {
        val ops = ctx.getSystemService(Context.APP_OPS_SERVICE) as AppOpsManager
        @Suppress("DEPRECATION")
        val mode = ops.checkOpNoThrow(AppOpsManager.OPSTR_GET_USAGE_STATS, Process.myUid(), ctx.packageName)
        if (mode != AppOpsManager.MODE_ALLOWED) {
            throw SecurityException("нужно право «Доступ к данным об использовании» для PC Dock (Настройки → Приложения → Особый доступ)")
        }
        val usm = ctx.getSystemService(Context.USAGE_STATS_SERVICE) as UsageStatsManager
        val now = System.currentTimeMillis()
        val events = usm.queryEvents(now - 60_000, now)
        val e = UsageEvents.Event()
        var pkg: String? = null
        var cls: String? = null
        while (events.hasNextEvent()) {
            events.getNextEvent(e)
            if (e.eventType == UsageEvents.Event.ACTIVITY_RESUMED) {
                pkg = e.packageName
                cls = e.className
            }
        }
        return JSONObject().put("package", pkg ?: JSONObject.NULL).put("activity", cls ?: JSONObject.NULL)
    }

    private fun getClipboard(ctx: Context): String {
        // Android 10+: читать буфер может только приложение в фокусе; иначе придёт пусто.
        val cm = ctx.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        val clip = cm.primaryClip ?: return ""
        return if (clip.itemCount > 0) clip.getItemAt(0).coerceToText(ctx).toString() else ""
    }

    private fun setClipboard(ctx: Context, text: String): JSONObject {
        val cm = ctx.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        cm.setPrimaryClip(ClipData.newPlainText("pcdock", text))
        return JSONObject()
    }

    private fun button(ctx: Context, name: String): JSONObject {
        if (name != "home") throw UnsupportedOperationException("кнопка $name без ADB недоступна (нужна специальная возможность); есть home")
        ctx.startActivity(Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_HOME).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        return JSONObject()
    }

    private fun screenState(ctx: Context): JSONObject {
        val pm = ctx.getSystemService(Context.POWER_SERVICE) as PowerManager
        val km = ctx.getSystemService(Context.KEYGUARD_SERVICE) as android.app.KeyguardManager
        return JSONObject().put("on", pm.isInteractive).put("locked", km.isKeyguardLocked)
    }

    private fun network(ctx: Context): JSONObject {
        val cm = ctx.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val caps = cm.getNetworkCapabilities(cm.activeNetwork)
        val online = caps?.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED) == true
        val iface = cm.getLinkProperties(cm.activeNetwork)
        val ip = iface?.linkAddresses?.map { it.address }?.firstOrNull { !it.isLoopbackAddress && it.address.size == 4 }
        return JSONObject().put("online", online).put("local_ip", ip?.hostAddress ?: JSONObject.NULL)
            .put("interface", iface?.interfaceName ?: JSONObject.NULL)
    }
}
