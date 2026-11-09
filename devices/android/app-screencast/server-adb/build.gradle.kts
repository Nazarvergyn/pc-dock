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
        versionName = "1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
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
