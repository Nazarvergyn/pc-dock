plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// Сервер без Activity и иконки: APK кладётся в /data/local/tmp и запускается
// `app_process` с правами shell (см. DECISIONS.md, решение 32).
android {
    namespace = "com.device.server"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.device.media.server"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = (findProperty("pcdockVersion") as String?) ?: "0.1.0-alpha.2"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            if (!System.getenv("PCDOCK_KEYSTORE").isNullOrEmpty()) signingConfig = signingConfigs.getByName("release")
        }
    }


    // Подпись релиза: ключ берётся из переменных окружения (в репозитории ключей нет). Без них release собирается
    // без подписи, а debug подписывается отладочным ключом.
    val keystore = System.getenv("PCDOCK_KEYSTORE")?.takeIf { it.isNotEmpty() }
    if (keystore != null) {
        signingConfigs {
            create("release") {
                storeFile = file(keystore)
                storePassword = System.getenv("PCDOCK_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("PCDOCK_KEY_ALIAS")
                keyPassword = System.getenv("PCDOCK_KEY_PASSWORD")
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    implementation(project(":lib-protocol"))
    implementation(project(":lib-dock"))
}
