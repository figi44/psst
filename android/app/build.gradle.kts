plugins {
    id("com.android.application") version "9.5.1"
}

val workspace: Directory = rootProject.layout.projectDirectory.dir("..")

android {
    namespace = "io.github.figi44.psst"
    compileSdk = 36

    defaultConfig {
        applicationId = "io.github.figi44.psst"
        minSdk = 31
        targetSdk = 36
        // Android installs an update only over a versionCode no higher than its own, so CI's
        // releases pass main's commit count.
        versionCode = providers.gradleProperty("psst.versionCode").map(String::toInt).getOrElse(1)
        versionName = providers.gradleProperty("psst.versionName")
            .orElse(
                providers.fileContents(workspace.file("Cargo.toml")).asText
                    .map { Regex("""(?m)^version = "(.+)"$""").find(it)!!.groupValues[1] },
            )
            .get()
    }

    // Without a keystore, as anywhere but CI, a release build comes out unsigned. Its key is
    // `psst`, under the store's password: PKCS12 allows no other.
    val keystore = providers.environmentVariable("PSST_KEYSTORE")
    if (keystore.isPresent) {
        val release = signingConfigs.create("release") {
            storeFile = file(keystore.get())
            storePassword = providers.environmentVariable("PSST_KEYSTORE_PASSWORD").get()
            keyAlias = "psst"
            keyPassword = storePassword
        }
        buildTypes.getByName("release").signingConfig = release
    }

    lint {
        warningsAsErrors = true
        // These flag a newer SDK or AGP release, so a build would start failing with no change of
        // ours; staying on SDK 36 is a decision.
        disable += setOf("OldTargetApi", "GradleDependency", "AndroidGradlePluginVersion")
    }
}

abstract class CargoNdk : Exec() {
    @get:Input
    abstract val abis: ListProperty<String>

    @get:Input
    abstract val platform: Property<Int>

    @get:Input
    abstract val profile: ListProperty<String>

    @get:OutputDirectory
    abstract val jniLibs: DirectoryProperty

    init {
        executable("cargo")
        argumentProviders.add {
            listOf("ndk", "--platform", platform.get().toString()) +
                abis.get().flatMap { listOf("-t", it) } +
                listOf("-o", jniLibs.get().asFile.path, "build", "--locked", "-p", "psst-android") +
                profile.get()
        }
        // cargo knows when the library is out of date, and finds out in well under a second.
        outputs.upToDateWhen { false }
    }
}

// Measured for arm64-v8a on a stand-in that links psst-client (tokio, WebSocket, libopus): plain
// release is 2.05 MB stripped, 13.7 s to build and 2.3 s to rebuild after a change. Opt-level s
// makes that 1.73 MB, 11.4 s and 0.9 s. Fat LTO and one codegen unit take it to 1.23 MB but cost
// 7.6 s a rebuild, so only release builds pay for them, in a profile of their own so that the two
// don't rebuild each other. libopus keeps -O3 (78 KB): it encodes every 20 ms of a recording.
val sizeProfile = listOf(
    "profile.release.strip=true",
    "profile.release.opt-level=\"s\"",
    "profile.release.package.opusic-sys.opt-level=3",
)
val releaseProfile = listOf(
    "profile.android-release.inherits=\"release\"",
    "profile.android-release.lto=\"fat\"",
    "profile.android-release.codegen-units=1",
)

androidComponents {
    onVariants { variant ->
        val name = variant.name.replaceFirstChar(Char::uppercase)
        val cargo = tasks.register<CargoNdk>("cargoNdk$name") {
            workingDir = workspace.asFile
            abis = providers.gradleProperty("psst.abis").map { it.split(',') }
            platform = variant.minSdk.apiLevel
            profile = if (variant.buildType == "release") {
                listOf("--profile", "android-release") + releaseProfile.flatMap { listOf("--config", it) }
            } else {
                listOf("--release")
            } + sizeProfile.flatMap { listOf("--config", it) }
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(cargo, CargoNdk::jniLibs)
    }
}
