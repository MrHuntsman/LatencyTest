plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.latencytest.sensor"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.latencytest.sensor"
        minSdk = 29          // spec §4: minSdk 29 (LG V50 runs Android 11)
        targetSdk = 34
        versionCode = 1
        versionName = "0.1"
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

    buildFeatures {
        viewBinding = false
    }
}

dependencies {
    // Intentionally no AndroidX: Camera2 is used directly and the UI is a
    // plain Activity with a SurfaceView, so the build stays dependency-light.
    implementation(kotlin("stdlib"))
}
