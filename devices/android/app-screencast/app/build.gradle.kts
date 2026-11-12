plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.device.media"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.device.media"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = (findProperty("pcdockVersion") as String?) ?: "0.1.0-alpha.1"
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

    buildTypes {
        release {
            isMinifyEnabled = false
            if (!System.getenv("PCDOCK_KEYSTORE").isNullOrEmpty()) signingConfig = signingConfigs.getByName("release")
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
    implementation("androidx.core:core-ktx:1.10.1")
    implementation("androidx.appcompat:appcompat:1.6.1")
}
