import os

def main():
    # 1. Update AndroidManifest.xml
    manifest_path = "gen/android/app/src/main/AndroidManifest.xml"
    if os.path.exists(manifest_path):
        with open(manifest_path, "r", encoding="utf-8") as f:
            content = f.read()

        permissions = """    <uses-permission android:name="android.permission.MANAGE_EXTERNAL_STORAGE" />
    <uses-permission android:name="android.permission.READ_EXTERNAL_STORAGE" />
    <uses-permission android:name="android.permission.WRITE_EXTERNAL_STORAGE" />
    <uses-permission android:name="android.permission.WAKE_LOCK" />
    <uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
    <uses-permission android:name="android.permission.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS" />
    <application"""

        if "<uses-permission" not in content:
            content = content.replace("<application", permissions, 1)

        if 'android:requestLegacyExternalStorage="true"' not in content:
            content = content.replace('<application', '<application\n        android:requestLegacyExternalStorage="true"', 1)

        if 'android:largeHeap="true"' not in content:
            content = content.replace('<application', '<application\n        android:largeHeap="true"', 1)

        if 'android:hardwareAccelerated="true"' not in content:
            content = content.replace('<application', '<application\n        android:hardwareAccelerated="true"', 1)

        if 'android:keepScreenOn="true"' not in content and '<activity' in content:
            content = content.replace('<activity', '<activity\n            android:keepScreenOn="true"', 1)

        with open(manifest_path, "w", encoding="utf-8") as f:
            f.write(content)
        print("Updated AndroidManifest.xml")

    # 2. Update gen/android/app/build.gradle.kts
    gradle_path = "gen/android/app/build.gradle.kts"
    if os.path.exists(gradle_path):
        with open(gradle_path, "r", encoding="utf-8") as f:
            content = f.read()

        ndk_filter = """defaultConfig {
        ndk {
            abiFilters.clear()
            abiFilters.add("arm64-v8a")
        }"""
        if "arm64-v8a" not in content:
            content = content.replace("defaultConfig {", ndk_filter, 1)

        packaging_block = """android {
    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }"""
        if "useLegacyPackaging" not in content:
            content = content.replace("android {", packaging_block, 1)

        with open(gradle_path, "w", encoding="utf-8") as f:
            f.write(content)
        print("Updated build.gradle.kts")

    # 3. Update gen/android/gradle.properties to remove JVM memory constraints
    gradle_props = "gen/android/gradle.properties"
    if os.path.exists(gradle_props):
        with open(gradle_props, "r", encoding="utf-8") as f:
            props = f.read()
        if "org.gradle.jvmargs" not in props:
            props += "\norg.gradle.jvmargs=-Xmx8g -XX:+UseG1GC\n"
            with open(gradle_props, "w", encoding="utf-8") as f:
                f.write(props)
        print("Updated gradle.properties")

if __name__ == "__main__":
    main()
