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
    <application"""

        if "<uses-permission" not in content:
            content = content.replace("<application", permissions, 1)

        if 'android:requestLegacyExternalStorage="true"' not in content:
            content = content.replace('<application', '<application\n        android:requestLegacyExternalStorage="true"', 1)

        if 'android:largeHeap="true"' not in content:
            content = content.replace('<application', '<application\n        android:largeHeap="true"', 1)

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

if __name__ == "__main__":
    main()
