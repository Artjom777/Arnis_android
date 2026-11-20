# Arnis Android Patcher

Autonomous patcher-converter that transforms a pristine upstream [Arnis](https://github.com/louis-e/arnis) repository into a fully working Android project (`arm64-v8a`) with automated APK builds via GitHub Actions.

## Structure

- `patcher.py`: Standalone Python 3 script using standard library to inject Android support and patches into clean upstream Arnis without fragile line-based diffs.
- `overlay/`: Android assets, launcher icons, build configuration scripts, patched LevelDB implementation (`rusty-leveldb`), and Android runtime support (`android_support.rs`).
- `.github/workflows/build_patched_apk.yml`: GitHub Actions workflow that clones upstream Arnis on demand, applies `patcher.py`, and compiles a signed `arm64-v8a` APK.

## Manual Usage

```bash
# 1. Clone clean upstream Arnis
git clone --depth 1 --branch v3.5.0 https://github.com/louis-e/arnis.git ./arnis

# 2. Apply patcher
python3 patcher.py --target ./arnis --overlay ./overlay

# 3. Build with Tauri
cd ./arnis
cargo tauri android init
python3 scripts/configure_android.py
cargo tauri android build --apk --target aarch64
```
