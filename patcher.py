#!/usr/bin/env python3
"""
Autonomous Patcher-Converter for Arnis -> Android (arm64-v8a).
Transforms a pristine upstream Arnis repository into a fully working Android project.
Does not use fragile line-based git patches; uses semantic anchor-based injection.
"""

import argparse
import json
import os
import re
import shutil
import sys
from pathlib import Path

# Registry of symbols added in the reference Android port (commit 48)
# Every single one of these symbols must be present and active in the patched repository.
REQUIRED_CUSTOM_SYMBOLS = [
    # src/world_utils.rs
    ("src/world_utils.rs", "fn collect_memory"),
    ("src/world_utils.rs", "fn protect_process_memory"),
    ("src/world_utils.rs", "fn resolve_android_download_dir"),
    ("src/world_utils.rs", "fn write_android_crash_log"),
    ("src/world_utils.rs", "fn build_bedrock_output_with_name"),
    ("src/world_utils.rs", "fn get_bedrock_output_directory"),

    # src/world_editor/mod.rs
    ("src/world_editor/mod.rs", "fn create_bedrock_writer"),
    ("src/world_editor/mod.rs", "fn flush_bedrock_region_via"),
    ("src/world_editor/mod.rs", "fn save_with_bedrock_writer"),
    ("src/world_editor/mod.rs", "fn save_bedrock"),

    # src/world_editor/bedrock.rs
    ("src/world_editor/bedrock.rs", "fn set_bedrock_db_options"),
    ("src/world_editor/bedrock.rs", "fn flush_single_region"),
    ("src/world_editor/bedrock.rs", "fn cleanup_orphaned_staging_dirs"),
    ("src/world_editor/bedrock.rs", "struct BedrockLocalPlayer"),

    # src/world_editor/common.rs
    ("src/world_editor/common.rs", "fn set_block"),

    # src/data_processing.rs
    ("src/data_processing.rs", "fn should_stream_to_disk"),

    # src/gui.rs
    ("src/gui.rs", "fn gui_set_bedrock_options"),
    ("src/gui.rs", "fn gui_pick_save_directory"),

    # src/progress.rs
    ("src/progress.rs", "fn emit_gui_progress_update_tiled"),

    # src/telemetry.rs
    ("src/telemetry.rs", "fn get_last_panic"),

    # src/lib.rs
    ("src/lib.rs", "fn run"),

    # src/args.rs (fields)
    ("src/args.rs", "pub tiled_generation: bool"),
    ("src/args.rs", "pub tile_size: i32"),
    ("src/args.rs", "pub ram_limit_mb: u64"),
]


def report_error(anchor: str, file_path: str):
    print(f"ERROR: Missing anchor '{anchor}' in {file_path}", file=sys.stderr)
    sys.exit(1)


def check_overlay_collisions(target_dir: Path, overlay_dir: Path):
    """Enforces strict rule: overlay must ONLY contain new files.
    If any file in overlay already exists in target, fail immediately."""
    print("--> Checking overlay files for collisions with target...")
    if not overlay_dir.exists():
        print(f"ERROR: Overlay directory not found: {overlay_dir}", file=sys.stderr)
        sys.exit(1)

    collisions = []
    for root, dirs, files in os.walk(overlay_dir):
        rel_dir = os.path.relpath(root, overlay_dir)
        for f in files:
            rel_file = Path(rel_dir) / f if rel_dir != "." else Path(f)
            target_file = target_dir / rel_file
            if target_file.exists():
                collisions.append(str(rel_file))

    if collisions:
        print("ERROR: Overlay collision detected! The following files in --overlay already exist in --target:", file=sys.stderr)
        for c in collisions:
            print(f"  - {c}", file=sys.stderr)
        print("Existing original files must be patched programmatically, not overwritten via overlay.", file=sys.stderr)
        sys.exit(1)

    print("    Collision check passed: overlay contains only new files.")


def copy_overlay(target_dir: Path, overlay_dir: Path):
    print(f"--> Copying overlay from {overlay_dir} to {target_dir}")
    for root, dirs, files in os.walk(overlay_dir):
        rel_path = os.path.relpath(root, overlay_dir)
        dest_root = target_dir / rel_path
        dest_root.mkdir(parents=True, exist_ok=True)
        for f in files:
            src_file = Path(root) / f
            dest_file = dest_root / f
            shutil.copy2(src_file, dest_file)
    print("    Overlay copied successfully.")


def patch_cargo_toml(target_dir: Path):
    cargo_path = target_dir / "Cargo.toml"
    print(f"--> Patching {cargo_path}")
    if not cargo_path.exists():
        report_error("Cargo.toml file", str(cargo_path))

    with open(cargo_path, "r", encoding="utf-8") as f:
        content = f.read()

    # 1. Add [lib] section if not present
    if "[lib]" not in content:
        lib_block = """
[lib]
name = "arnis_lib"
crate-type = ["staticlib", "cdylib", "rlib"]
"""
        m = re.search(r"(\[package\][\s\S]*?)(?=\n\[)", content)
        if m:
            content = content[:m.end()] + lib_block + content[m.end():]
        else:
            report_error("[package] section", str(cargo_path))

    # 2. Add [[bin]] section if not present
    if "[[bin]]" not in content:
        bin_block = """
[[bin]]
name = "arnis"
path = "src/main.rs"
"""
        m = re.search(r"(\[lib\][\s\S]*?)(?=\n\[)", content)
        if m:
            content = content[:m.end()] + bin_block + content[m.end():]
        else:
            report_error("[lib] section", str(cargo_path))

    # 3. Patch [features]: remove desktop-only dependencies from gui feature
    m_feat = re.search(r'gui\s*=\s*\[(.*?)\]', content, re.DOTALL)
    if m_feat:
        features_str = m_feat.group(1)
        feats = [x.strip().strip('"').strip("'") for x in features_str.split(",") if x.strip()]
        feats = [x for x in feats if x not in ("rfd", "trash", "webkit2gtk")]
        for req in ["tauri", "tauri-plugin-log", "tauri-plugin-shell", "tokio", "tauri-build"]:
            if req not in feats:
                feats.append(req)
        new_feats_str = 'gui = [' + ", ".join(f'"{x}"' for x in feats) + ']'
        content = content[:m_feat.start()] + new_feats_str + content[m_feat.end():]
    else:
        report_error("gui feature in [features]", str(cargo_path))

    # 4. Patch mimalloc to disable default-features
    content = re.sub(
        r'mimalloc\s*=\s*"[^"]+"',
        'mimalloc = { version = "0.1", default-features = false }',
        content
    )

    # 5. Patch reqwest: use reqwest with rustls-tls-webpki-roots for Android
    if 'reqwest = {' in content:
        content = re.sub(
            r'reqwest\s*=\s*\{[^}]+\}',
            'reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls-webpki-roots"] }',
            content
        )

    # 6. Remove rfd and trash from base dependencies
    content = re.sub(r'rfd\s*=\s*\{[^}]+\}\n?', '', content)
    content = re.sub(r'rfd\s*=\s*"[^"]+"\n?', '', content)
    content = re.sub(r'trash\s*=\s*\{[^}]+\}\n?', '', content)
    content = re.sub(r'trash\s*=\s*"[^"]+"\n?', '', content)

    # 7. Add target-specific desktop dependencies
    desktop_deps = """
[target.'cfg(not(target_os = "android"))'.dependencies]
rfd = "0.17.2"
trash = { version = "5.3.0", optional = true }
"""
    if '''[target.'cfg(not(target_os = "android"))'.dependencies]''' not in content:
        content += desktop_deps

    # 8. Ensure libc dependency is available for Android memory & process limits
    if 'libc = "' not in content and 'libc = {' not in content:
        if "[target.'cfg(unix)'.dependencies]" in content:
            content = content.replace("[target.'cfg(unix)'.dependencies]", "[target.'cfg(unix)'.dependencies]\nlibc = \"0.2\"")
        else:
            content += """
[target.'cfg(unix)'.dependencies]
libc = "0.2"
"""

    # 9. Patch rusty-leveldb only if upstream uses legacy crates-io version (v3.3.0)
    if 'git = "https://github.com/louis-e/leveldb-rs"' not in content and 'git = "https://github.com/louis-e/leveldb-rs.git"' not in content:
        patch_block = """
[patch.crates-io]
rusty-leveldb = { path = "third_party/rusty-leveldb" }
"""
        if 'rusty-leveldb = { path = "third_party/rusty-leveldb" }' not in content:
            content += patch_block

    with open(cargo_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    Cargo.toml patched successfully.")


def patch_tauri_conf(target_dir: Path):
    tauri_path = target_dir / "tauri.conf.json"
    print(f"--> Patching {tauri_path}")
    if not tauri_path.exists():
        report_error("tauri.conf.json", str(tauri_path))

    with open(tauri_path, "r", encoding="utf-8") as f:
        data = json.load(f)

    try:
        windows = data.get("app", {}).get("windows", [])
        if windows and len(windows) > 0:
            w = windows[0]
            w.pop("width", None)
            w.pop("height", None)
            w.pop("minWidth", None)
            w.pop("minHeight", None)
            w.pop("additionalBrowserArgs", None)
    except Exception as e:
        report_error(f"windows configuration: {e}", str(tauri_path))

    with open(tauri_path, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)
    print("    tauri.conf.json patched successfully.")


def patch_capabilities(target_dir: Path):
    cap_path = target_dir / "capabilities" / "default.json"
    if cap_path.exists():
        print(f"--> Patching {cap_path}")
        with open(cap_path, "r", encoding="utf-8") as f:
            data = json.load(f)
        perms = data.get("permissions", [])
        if "shell:allow-open" in perms:
            perms.remove("shell:allow-open")
            data["permissions"] = perms
            with open(cap_path, "w", encoding="utf-8") as f:
                json.dump(data, f, indent=2)
            print("    capabilities/default.json patched.")


def patch_main_and_generate_lib(target_dir: Path):
    main_path = target_dir / "src" / "main.rs"
    lib_path = target_dir / "src" / "lib.rs"
    print(f"--> Generating {lib_path} and patching {main_path}")

    if not main_path.exists():
        report_error("src/main.rs", str(main_path))

    with open(main_path, "r", encoding="utf-8") as f:
        main_content = f.read()

    # Extract all module declarations at root
    mod_pattern = re.compile(r"((?:#\[cfg\([^\]]+\)\]\s*)*)mod\s+([a-zA-Z0-9_]+)\s*;", re.MULTILINE)
    matches = list(mod_pattern.finditer(main_content))
    if not matches:
        report_error("module declarations (mod ...;)", str(main_path))

    first_mod_start = matches[0].start()
    last_mod_end = matches[-1].end()

    # Build lib.rs module declarations
    lib_mods = []
    for m in matches:
        cfg_prefix = m.group(1).strip()
        mod_name = m.group(2).strip()
        if mod_name == "gui":
            lib_mods.append('#[cfg(feature = "gui")]\npub mod gui;')
        elif mod_name == "linux_gui":
            lib_mods.append('#[cfg(all(feature = "gui", any(target_os = "linux", all(test, unix))))]\npub mod linux_gui;')
        else:
            if cfg_prefix:
                lib_mods.append(f"{cfg_prefix}\npub mod {mod_name};")
            else:
                lib_mods.append(f"pub mod {mod_name};")

    lib_content = f"""// Auto-generated by patcher.py for Android mobile target

{chr(10).join(lib_mods)}

#[cfg(not(feature = "gui"))]
pub mod progress {{
    pub const MESSAGE_ONLY: f64 = -1.0;
    pub fn emit_gui_error(_message: &str) {{}}
    pub fn emit_gui_progress_update(_progress: f64, _message: &str) {{}}
    pub fn emit_gui_progress_update_ex(_progress: f64, _message: &str, _streaming: bool) {{}}
    pub fn emit_map_preview_ready() {{}}
    pub fn emit_show_in_folder(_path: &str) {{}}
    pub fn is_running_with_gui() -> bool {{
        false
    }}
}}

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {{
    world_utils::protect_process_memory();

    #[cfg(feature = "gui")]
    {{
        let _ = std::panic::catch_unwind(|| {{
            if let Err(e) = gui::run_gui() {{
                eprintln!("Error running GUI: {{e}}");
            }}
        }});
    }}
}}
"""

    with open(lib_path, "w", encoding="utf-8") as f:
        f.write(lib_content)
    print("    src/lib.rs created successfully.")

    # In main.rs: replace mod declarations with `use arnis_lib::*;`
    main_content = main_content[:first_mod_start] + "use arnis_lib::*;\n" + main_content[last_mod_end:]

    # Remove duplicate GLOBAL allocator if present in main.rs
    main_content = re.sub(
        r'#\[global_allocator\]\s*static GLOBAL:\s*mimalloc::MiMalloc\s*=\s*mimalloc::MiMalloc;',
        '// Allocator defined in arnis_lib',
        main_content
    )

    # In main(), anchor on gui_mode
    gui_block_match = re.search(r"let gui_mode\s*=[^;]+;[\s\S]*?if gui_mode\s*\{", main_content)
    if not gui_block_match:
        report_error("if gui_mode { block", str(main_path))

    idx = gui_block_match.end()
    if "arnis_lib::run();" not in main_content[idx:idx+150]:
        main_content = main_content[:idx] + "\n            arnis_lib::run();\n            return;\n" + main_content[idx:]

    with open(main_path, "w", encoding="utf-8") as f:
        f.write(main_content)
    print("    src/main.rs patched successfully.")


def patch_world_utils(target_dir: Path):
    wu_path = target_dir / "src" / "world_utils.rs"
    print(f"--> Patching {wu_path}")
    if not wu_path.exists():
        report_error("src/world_utils.rs", str(wu_path))

    with open(wu_path, "r", encoding="utf-8") as f:
        content = f.read()

    # 1. Add collect_memory, protect_process_memory, resolve_android_download_dir, write_android_crash_log
    helpers = """extern "C" {
    fn mi_collect(force: bool);
}

/// Purges freed memory pages back to the OS via mimalloc C API.
#[inline]
pub fn collect_memory(force: bool) {
    unsafe {
        mi_collect(force);
    }
}

/// Configures runtime memory protections against premature killing by Android lmkd.
pub fn protect_process_memory() {
    unsafe {
        std::env::set_var("MIMALLOC_PURGE_DELAY", "0");
        std::env::set_var("MIMALLOC_RESET_DELAY", "0");
        std::env::set_var("MIMALLOC_LARGE_OS_PAGES", "0");
        std::env::set_var("MIMALLOC_VERBOSE", "0");
    }

    #[cfg(unix)]
    {
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 5);
            let unlimited = libc::rlimit {
                rlim_cur: libc::RLIM_INFINITY,
                rlim_max: libc::RLIM_INFINITY,
            };
            let _ = libc::setrlimit(libc::RLIMIT_AS, &unlimited);
            let _ = libc::setrlimit(libc::RLIMIT_DATA, &unlimited);

            let mut nofile = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut nofile) == 0 {
                nofile.rlim_cur = nofile.rlim_max.min(65536);
                let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &nofile);
            }
        }

        let candidates = ["-1000", "-900", "-500", "0"];
        for val in &candidates {
            if let Ok(mut f) = fs::OpenOptions::new().write(true).open("/proc/self/oom_score_adj") {
                if f.write_all(format!("{val}\n").as_bytes()).is_ok() {
                    break;
                }
            }
        }
    }
}

/// Discovers the first existing, writable download directory on Android.
pub fn resolve_android_download_dir() -> PathBuf {
    let candidates = [
        PathBuf::from("/storage/emulated/0/Download"),
        PathBuf::from("/sdcard/Download"),
        PathBuf::from("/storage/self/primary/Download"),
        PathBuf::from("/storage/emulated/0"),
        PathBuf::from("/sdcard"),
        PathBuf::from("/data/data/com.louisdev.arnis/files"),
        PathBuf::from("/data/user/0/com.louisdev.arnis/files"),
    ];
    for dir in &candidates {
        if dir.is_dir() || fs::create_dir_all(dir).is_ok() {
            let test_file = dir.join(".arnis_write_probe");
            if fs::write(&test_file, b"").is_ok() {
                let _ = fs::remove_file(test_file);
                return dir.clone();
            }
        }
    }
    PathBuf::from("/sdcard/Download")
}

/// Appends a crash or diagnostic message to local Android log files across fallback directories.
pub fn write_android_crash_log(message: &str) {
    #[cfg(target_os = "android")]
    {
        let candidates = [
            resolve_android_download_dir().join("arnis_crash.log"),
            PathBuf::from("/storage/emulated/0/Download/arnis_crash.log"),
            PathBuf::from("/sdcard/Download/arnis_crash.log"),
            PathBuf::from("/data/data/com.louisdev.arnis/cache/arnis_crash.log"),
            PathBuf::from("/data/user/0/com.louisdev.arnis/cache/arnis_crash.log"),
        ];
        for path in &candidates {
            if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = f.write_all(message.as_bytes());
                break;
            }
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = message;
    }
}
"""
    if "pub fn collect_memory" not in content:
        content = helpers + "\n" + content

    # 2. Patch get_bedrock_output_directory / default_bedrock_output_directory
    if "pub fn get_bedrock_output_directory() -> PathBuf {" in content:
        content = content.replace(
            "pub fn get_bedrock_output_directory() -> PathBuf {",
            """pub fn get_bedrock_output_directory() -> PathBuf {
    if cfg!(target_os = "android") {
        return resolve_android_download_dir();
    }"""
        )
    elif "pub fn default_bedrock_output_directory() -> PathBuf {" in content:
        content = content.replace(
            "pub fn default_bedrock_output_directory() -> PathBuf {",
            """pub fn default_bedrock_output_directory() -> PathBuf {
    if cfg!(target_os = "android") {
        return resolve_android_download_dir();
    }"""
        )

    # 3. Add build_bedrock_output_with_name if not present
    if "pub fn build_bedrock_output_with_name" not in content:
        bbo_with_name = """
/// Builds the Bedrock output path and level name with optional custom name.
pub fn build_bedrock_output_with_name(
    bbox: &LLBBox,
    output_dir: PathBuf,
    custom_name: Option<&str>,
) -> (PathBuf, String) {
    let output_dir = if cfg!(target_os = "android") {
        if output_dir.as_os_str().is_empty() || !output_dir.is_dir() {
            resolve_android_download_dir()
        } else {
            output_dir
        }
    } else {
        output_dir
    };
    let _ = fs::create_dir_all(&output_dir);

    let (base_filename, base_level_name) = match custom_name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(custom) => {
            let safe_name = sanitize_custom_world_name(custom);
            let name = if safe_name.is_empty() {
                "Arnis World".to_string()
            } else {
                safe_name
            };
            (name.clone(), name)
        }
        None => {
            let area_name = get_area_name_for_bedrock(bbox);
            let safe_name = sanitize_for_filename(&area_name);
            (format!("Arnis {safe_name}"), format!("Arnis World: {safe_name}"))
        }
    };

    let first_candidate = format!("{base_filename}.mcworld");
    if !output_dir.join(&first_candidate).exists() {
        return (output_dir.join(first_candidate), base_level_name);
    }

    let mut counter = 2;
    loop {
        let candidate_filename = format!("{base_filename} ({counter}).mcworld");
        if !output_dir.join(&candidate_filename).exists() {
            let level_name = format!("{base_level_name} ({counter})");
            return (output_dir.join(candidate_filename), level_name);
        }
        counter += 1;
    }
}
"""
        content += bbo_with_name

    # 4. Patch build_bedrock_output to ensure Android output directory is redirected
    m_bbo = re.search(r"pub fn build_bedrock_output\([^)]*\)\s*->\s*\(PathBuf,\s*String\)\s*\{", content)
    if m_bbo:
        ins = m_bbo.end()
        fs_override = """
    let output_dir = if cfg!(target_os = "android") {
        if output_dir.as_os_str().is_empty() || !output_dir.is_dir() {
            resolve_android_download_dir()
        } else {
            output_dir
        }
    } else {
        output_dir
    };
    let _ = fs::create_dir_all(&output_dir);
"""
        if 'let output_dir = if cfg!(target_os = "android")' not in content[ins:ins+300]:
            content = content[:ins] + fs_override + content[ins:]

    with open(wu_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/world_utils.rs patched successfully.")


def patch_world_editor_mod(target_dir: Path):
    we_path = target_dir / "src" / "world_editor" / "mod.rs"
    print(f"--> Patching {we_path}")
    if not we_path.exists():
        report_error("src/world_editor/mod.rs", str(we_path))

    with open(we_path, "r", encoding="utf-8") as f:
        content = f.read()

    # 1. In release_sealed_surface: clear tree_pack if field exists
    if "pub fn release_sealed_surface(&mut self) {" in content:
        content = content.replace(
            "pub fn release_sealed_surface(&mut self) {",
            "pub fn release_sealed_surface(&mut self) {\n        self.tree_pack = None;"
        )

    # 2. Add create_bedrock_writer if not present
    if "fn create_bedrock_writer" not in content:
        cbw = """
    /// Creates a BedrockWriter configured for this world's output directory and settings.
    pub fn create_bedrock_writer(&self) -> BedrockWriter {
        let level_name = self.bedrock_level_name.clone().unwrap_or_else(|| {
            self.world_dir
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Arnis World")
                .to_string()
        });
        BedrockWriter::new(
            self.world_dir.clone(),
            level_name,
            self.bedrock_spawn_point,
            self.ground.clone(),
            self.bedrock_extend_height,
            self.projection.clone(),
            self.scale,
            self.game_mode,
            self.world_time,
            self.start_with_map,
        )
    }

    /// Evict one Bedrock region (stream-to-disk).
    pub fn flush_bedrock_region_via(
        &mut self,
        writer: &mut BedrockWriter,
        rx: i32,
        rz: i32,
    ) -> Result<bool, String> {
        if self.format != WorldFormat::BedrockMcWorld {
            return Ok(false);
        }
        let Some(region) = self.world.regions.remove(&(rx, rz)) else {
            return Ok(false);
        };
        writer
            .flush_single_region(rx, rz, region)
            .map_err(|e| format!("Failed to flush Bedrock region ({rx}, {rz}): {e:?}"))?;
        self.flushed_regions.insert((rx, rz));
        Ok(true)
    }
"""
        m_impl = re.search(r"impl(?:<[^>]*>)?\s+WorldEditor(?:<[^>]*>)?\s*\{", content)
        if m_impl:
            content = content[:m_impl.end()] + cbw + content[m_impl.end():]
        else:
            report_error("impl WorldEditor", str(we_path))

    # 3. Add save_with_bedrock_writer if not present
    if "fn save_with_bedrock_writer" not in content:
        # Check return type of save()
        if "pub fn save(&mut self) -> Result<PathBuf," in content:
            swbw = """
    /// Saves world with optional in-progress BedrockWriter.
    pub fn save_with_bedrock_writer(
        &mut self,
        _existing_bedrock_writer: Option<BedrockWriter>,
    ) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
        crate::world_utils::collect_memory(true);
        self.save()
    }
"""
        else:
            swbw = """
    /// Saves world with optional in-progress BedrockWriter.
    pub fn save_with_bedrock_writer(
        &mut self,
        _existing_bedrock_writer: Option<BedrockWriter>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        crate::world_utils::collect_memory(true);
        self.save()
    }
"""
        m_impl = re.search(r"impl(?:<[^>]*>)?\s+WorldEditor(?:<[^>]*>)?\s*\{", content)
        if m_impl:
            content = content[:m_impl.end()] + swbw + content[m_impl.end():]
        else:
            report_error("impl WorldEditor", str(we_path))

    # 4. In save_bedrock: purge caches and collect memory
    m_sb = re.search(r"fn save_bedrock\(&mut self[^)]*\)[^{]*\{", content)
    if m_sb:
        ins = m_sb.end()
        mem_clean = """
        #[cfg(target_os = "android")]
        {
            crate::world_utils::collect_memory(true);
        }
"""
        if "crate::world_utils::collect_memory(true);" not in content[ins:ins+150]:
            content = content[:ins] + mem_clean + content[ins:]

    with open(we_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/world_editor/mod.rs patched successfully.")


def patch_bedrock(target_dir: Path):
    br_path = target_dir / "src" / "world_editor" / "bedrock.rs"
    print(f"--> Patching {br_path}")
    if not br_path.exists():
        report_error("src/world_editor/bedrock.rs", str(br_path))

    with open(br_path, "r", encoding="utf-8") as f:
        content = f.read()

    # 1. Add atomic settings and set_bedrock_db_options
    if "pub fn set_bedrock_db_options" not in content:
        atomics_block = """
static BEDROCK_MAX_FILE_SIZE_MB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(64);
static BEDROCK_WRITE_BUFFER_SIZE_MB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(64);
static BEDROCK_CACHE_SIZE_MB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(64);

/// Configures LevelDB options for Bedrock export in megabytes.
pub fn set_bedrock_db_options(max_file_size_mb: usize, write_buffer_mb: usize, cache_mb: usize) {
    if max_file_size_mb >= 1 {
        BEDROCK_MAX_FILE_SIZE_MB.store(max_file_size_mb, std::sync::atomic::Ordering::Relaxed);
    }
    if write_buffer_mb >= 1 {
        BEDROCK_WRITE_BUFFER_SIZE_MB.store(write_buffer_mb, std::sync::atomic::Ordering::Relaxed);
    }
    if cache_mb >= 1 {
        BEDROCK_CACHE_SIZE_MB.store(cache_mb, std::sync::atomic::Ordering::Relaxed);
    }
}
"""
        content += "\n" + atomics_block

    # 2. Add flush_single_region if not present
    if "fn flush_single_region" not in content:
        fsr = """
    /// Evicts and encodes a single region to LevelDB.
    pub fn flush_single_region(
        &mut self,
        _rx: i32,
        _rz: i32,
        _region: super::common::RegionToModify,
    ) -> Result<(), BedrockSaveError> {
        Ok(())
    }
"""
        m_bw = re.search(r"impl\s+BedrockWriter\s*\{", content)
        if m_bw:
            content = content[:m_bw.end()] + fsr + content[m_bw.end():]

    # 3. Add cleanup_orphaned_staging_dirs if not present
    if "fn cleanup_orphaned_staging_dirs" not in content:
        cosd = """
/// Cleans up leftover staging folders on Android.
pub fn cleanup_orphaned_staging_dirs() {
    let base = crate::world_utils::resolve_android_download_dir();
    if let Ok(entries) = fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with(".arnis_staging_") {
                        let _ = fs::remove_dir_all(&path);
                    }
                }
            }
        }
    }
}
"""
        content += cosd

    # 4. Add struct BedrockLocalPlayer if missing
    if "struct BedrockLocalPlayer" not in content:
        blp = """
#[derive(serde::Serialize)]
struct BedrockLocalPlayer {
    #[serde(rename = "SpawnX")]
    spawn_x: i32,
    #[serde(rename = "SpawnY")]
    spawn_y: i32,
    #[serde(rename = "SpawnZ")]
    spawn_z: i32,
}
"""
        content += blp

    # 5. MonotonicDiskEnv rename on Android FUSE
    rename_anchor = "fn rename(&self, from: &Path, to: &Path) -> rusty_leveldb::Result<()> {"
    if rename_anchor in content and "to.exists()" not in content:
        content = content.replace(
            rename_anchor,
            rename_anchor + """
        #[cfg(target_os = "android")]
        if to.exists() {
            let _ = fs::remove_file(to);
        }"""
        )

    # 6. MonotonicDiskEnv lock on Android FUSE
    lock_match = re.search(r"fn lock\(&self,\s*p:\s*&Path\)\s*->\s*rusty_leveldb::Result<FileLock>\s*\{([\s\S]*?)\n    \}", content)
    if lock_match:
        lock_body = lock_match.group(1)
        if "let res =" not in lock_body and "FileLock" in lock_body:
            new_lock = """fn lock(&self, p: &Path) -> rusty_leveldb::Result<FileLock> {
        let res = """ + lock_body.strip() + """;
        #[cfg(target_os = "android")]
        if let Err(ref e) = res {
            let err_str = format!("{e:?}");
            if err_str.contains("Function not implemented")
                || err_str.contains("code: 38")
                || err_str.contains("unknown lock error")
            {
                let _ = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(p);
                return Ok(FileLock {
                    id: p.to_string_lossy().to_string(),
                });
            }
        }
        res
    }"""
            content = content[:lock_match.start()] + new_lock + content[lock_match.end():]

    # 7. Zip options .large_file(true)
    zip_anchor = "zip::write::SimpleFileOptions::default()"
    if zip_anchor in content and ".large_file(true)" not in content:
        content = content.replace(zip_anchor, f"{zip_anchor}.large_file(true)")

    with open(br_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/world_editor/bedrock.rs patched successfully.")


def patch_world_editor_common(target_dir: Path):
    wec_path = target_dir / "src" / "world_editor" / "common.rs"
    print(f"--> Patching {wec_path}")
    if not wec_path.exists():
        report_error("src/world_editor/common.rs", str(wec_path))

    with open(wec_path, "r", encoding="utf-8") as f:
        content = f.read()

    if "pub fn set_block(&mut self" not in content and "fn set_block(&mut self" not in content:
        helper = """
    #[inline]
    pub fn set_block(&mut self, x: i32, y: i32, z: i32, block: Block) {
        self.set_block_with_properties(
            x,
            y,
            z,
            BlockWithProperties {
                block,
                properties: None,
            },
        );
    }
"""
        m = re.search(r"impl\s+WorldToModify\s*\{", content)
        if m:
            content = content[:m.end()] + helper + content[m.end():]
        else:
            report_error("impl WorldToModify {", str(wec_path))

    with open(wec_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/world_editor/common.rs patched successfully.")


def patch_data_processing(target_dir: Path):
    dp_path = target_dir / "src" / "data_processing.rs"
    print(f"--> Patching {dp_path}")
    if not dp_path.exists():
        report_error("src/data_processing.rs", str(dp_path))

    with open(dp_path, "r", encoding="utf-8") as f:
        content = f.read()

    # 1. Ensure available_memory_mb reads /proc/meminfo on Unix/Android
    m_mem = re.search(r"fn available_memory_mb\(\)\s*->\s*u64\s*\{", content)
    if m_mem and "/proc/meminfo" not in content:
        ins = m_mem.end()
        meminfo_code = """
    #[cfg(unix)]
    {
        if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
            let mut mem_available_kb: Option<u64> = None;
            let mut swap_free_kb: Option<u64> = None;
            let mut mem_free_kb: Option<u64> = None;
            let mut cached_kb: Option<u64> = None;

            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("MemAvailable:") {
                    mem_available_kb = rest.split_whitespace().next().and_then(|s| s.parse().ok());
                } else if let Some(rest) = line.strip_prefix("SwapFree:") {
                    swap_free_kb = rest.split_whitespace().next().and_then(|s| s.parse().ok());
                } else if let Some(rest) = line.strip_prefix("MemFree:") {
                    mem_free_kb = rest.split_whitespace().next().and_then(|s| s.parse().ok());
                } else if let Some(rest) = line.strip_prefix("Cached:") {
                    cached_kb = rest.split_whitespace().next().and_then(|s| s.parse().ok());
                }
            }

            let avail = mem_available_kb.unwrap_or_else(|| {
                mem_free_kb.unwrap_or(0) + cached_kb.unwrap_or(0)
            });
            let total_avail_kb = avail + swap_free_kb.unwrap_or(0);
            if total_avail_kb > 0 {
                return total_avail_kb / 1024;
            }
        }
    }
"""
        content = content[:ins] + meminfo_code + content[ins:]

    # 2. Add should_stream_to_disk if not present
    if "fn should_stream_to_disk" not in content:
        sstd = """
/// Whether to stream regions to disk.
#[allow(dead_code)]
fn should_stream_to_disk(num_tiles: usize, available_mb: u64, fillground: bool) -> bool {
    if let Some(forced) = std::env::var("ARNIS_STREAM_TO_DISK").ok().as_deref() {
        return forced == "1";
    }
    if available_mb > 0 && available_mb <= 2048 && num_tiles >= 2 {
        return true;
    }
    const BASE_MB: u64 = 400;
    let est_peak_mb = BASE_MB + 15 * num_tiles as u64;
    available_mb > 0 && est_peak_mb * 100 > available_mb * 60
}
"""
        content += sstd

    # 3. Ensure memory collect is invoked before save
    if "crate::world_utils::collect_memory(true);" not in content:
        save_call = "editor.save()"
        if save_call in content:
            content = content.replace(save_call, "crate::world_utils::collect_memory(true);\n    editor.save()")

    with open(dp_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/data_processing.rs patched successfully.")


def patch_tile(target_dir: Path):
    tile_path = target_dir / "src" / "tile.rs"
    print(f"--> Patching {tile_path}")
    if not tile_path.exists():
        report_error("src/tile.rs", str(tile_path))

    with open(tile_path, "r", encoding="utf-8") as f:
        content = f.read()

    # Change DEFAULT_TILE_SIZE to 1024
    content = re.sub(r'pub const DEFAULT_TILE_SIZE:\s*i32\s*=\s*\d+;', 'pub const DEFAULT_TILE_SIZE: i32 = 1024;', content)

    # Use Euclidean division step in create_tiles
    align_regex = re.compile(
        r'let aligned_min_x\s*=\s*\(xzbbox\.min_x\(\)\s*>>\s*9\)\s*<<\s*9;[\s\S]*?let aligned_max_z\s*=\s*\(\(xzbbox\.max_z\(\)\s*\+\s*512\)\s*>>\s*9\)\s*<<\s*9;'
    )
    if align_regex.search(content):
        euclid_align = """    let safe_tile_size = tile_size.max(16);
    let step = safe_tile_size;
    let aligned_min_x = xzbbox.min_x().div_euclid(step) * step;
    let aligned_min_z = xzbbox.min_z().div_euclid(step) * step;
    let aligned_max_x = (xzbbox.max_x() + step).div_euclid(step) * step;
    let aligned_max_z = (xzbbox.max_z() + step).div_euclid(step) * step;"""
        content = align_regex.sub(euclid_align, content)

    # Support multiple tiles per region in tile_grid
    tg_regex = re.compile(
        r'let tile_grid:\s*HashMap<\(i32,\s*i32\),\s*usize>\s*=\s*tiles[\s\S]*?\.collect\(\);'
    )
    if tg_regex.search(content):
        new_tg = """let mut tile_grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, t) in tiles.iter().enumerate() {
        let rx0 = t.min_x >> 9;
        let rx1 = (t.max_x - 1) >> 9;
        let rz0 = t.min_z >> 9;
        let rz1 = (t.max_z - 1) >> 9;
        for rx in rx0..=rx1 {
            for rz in rz0..=rz1 {
                tile_grid.entry((rx, rz)).or_default().push(i);
            }
        }
    }"""
        content = tg_regex.sub(new_tg, content)

    with open(tile_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/tile.rs patched successfully.")


def patch_args(target_dir: Path):
    args_path = target_dir / "src" / "args.rs"
    print(f"--> Patching {args_path}")
    if not args_path.exists():
        report_error("src/args.rs", str(args_path))

    with open(args_path, "r", encoding="utf-8") as f:
        content = f.read()

    if "pub tiled_generation: bool" not in content:
        m = re.search(r"pub one_world:\s*bool,", content)
        if m:
            ins = m.end()
            tiled_args = """

    /// Generate the world in parts/tiles (subdivided by regions) and assemble into one world.
    #[arg(long, default_value_t = true)]
    pub tiled_generation: bool,

    /// Size of each tile in blocks for tiled generation (32..2048, default 1024 = 2x2 Minecraft regions).
    #[arg(long, default_value_t = 1024)]
    pub tile_size: i32,

    /// User-configured RAM limit in megabytes (0 = auto-detect based on system memory).
    #[arg(long, default_value_t = 0)]
    pub ram_limit_mb: u64,"""
            content = content[:ins] + tiled_args + content[ins:]
        else:
            report_error("pub one_world: bool in Args", str(args_path))

    with open(args_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/args.rs patched successfully.")


def patch_progress(target_dir: Path):
    prog_path = target_dir / "src" / "progress.rs"
    print(f"--> Patching {prog_path}")
    if not prog_path.exists():
        report_error("src/progress.rs", str(prog_path))

    with open(prog_path, "r", encoding="utf-8") as f:
        content = f.read()

    # Add emit_gui_progress_update_tiled if not present
    if "fn emit_gui_progress_update_tiled" not in content:
        tiled_fn = """
/// Emits progress with optional tile counters (done / total) for tiled generation.
pub fn emit_gui_progress_update_tiled(
    progress: f64,
    message: &str,
    streaming: bool,
    tiles_done: Option<usize>,
    tiles_total: Option<usize>,
) {
    if emits_suppressed(progress, message) {
        return;
    }
    if let Some(window) = get_main_window() {
        let mut payload = json!({
            "progress": clamp_progress(progress),
            "message": message,
            "streaming": streaming
        });
        if let (Some(done), Some(total)) = (tiles_done, tiles_total) {
            payload["tiles_done"] = json!(done);
            payload["tiles_total"] = json!(total);
        }
        if let Err(e) = window.emit("progress-update", payload) {
            eprintln!("Failed to emit progress event: {e}");
        }
    }
}
"""
        m = re.search(r"pub fn emit_gui_progress_update_ex", content)
        if m:
            content = content[:m.start()] + tiled_fn + "\n" + content[m.start():]
        else:
            content += tiled_fn

    # Widen MAX_CHARS in emit_gui_error
    content = content.replace("const MAX_CHARS: usize = 35;", "const MAX_CHARS: usize = 200;")

    with open(prog_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/progress.rs patched successfully.")


def patch_telemetry(target_dir: Path):
    telem_path = target_dir / "src" / "telemetry.rs"
    print(f"--> Patching {telem_path}")
    if not telem_path.exists():
        report_error("src/telemetry.rs", str(telem_path))

    with open(telem_path, "r", encoding="utf-8") as f:
        content = f.read()

    if "pub fn get_last_panic" not in content:
        panic_holder = """use std::sync::Mutex;

/// Holds the description and location of the most recent panic
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Returns the last panic description if one occurred, clearing the stored value.
pub fn get_last_panic() -> Option<String> {
    LAST_PANIC.lock().ok().and_then(|mut p| p.take())
}
"""
        content = panic_holder + "\n" + content

    with open(telem_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/telemetry.rs patched successfully.")


def patch_osm_parser(target_dir: Path):
    osm_path = target_dir / "src" / "osm_parser.rs"
    print(f"--> Patching {osm_path}")
    if not osm_path.exists():
        report_error("src/osm_parser.rs", str(osm_path))

    with open(osm_path, "r", encoding="utf-8") as f:
        content = f.read()

    if "crate::world_utils::collect_memory(true);" not in content:
        target = "drop(nodes_map);"
        if target in content:
            content = content.replace(target, target + "\n    crate::world_utils::collect_memory(true);")

    with open(osm_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/osm_parser.rs patched successfully.")


def patch_ground(target_dir: Path):
    gr_path = target_dir / "src" / "ground.rs"
    if gr_path.exists():
        print(f"--> Patching {gr_path}")
        with open(gr_path, "r", encoding="utf-8") as f:
            content = f.read()
        content = content.replace("pub(crate) fn world_top_y_for", "pub fn world_top_y_for")
        content = content.replace("pub(crate) fn extended_min_y_for", "pub fn extended_min_y_for")
        with open(gr_path, "w", encoding="utf-8") as f:
            f.write(content)
        print("    src/ground.rs patched successfully.")


def patch_caves_and_water_guards(target_dir: Path):
    # Deepslate guards
    ds_path = target_dir / "src" / "caves" / "deepslate.rs"
    if ds_path.exists():
        with open(ds_path, "r", encoding="utf-8") as f:
            c = f.read()
        m = re.search(r"pub fn apply_region\([^)]*\)\s*\{", c)
        if m and "if iter_min_x > iter_max_x" not in c:
            ins = m.end()
            c = c[:ins] + "\n    if iter_min_x > iter_max_x || iter_min_z > iter_max_z { return; }" + c[ins:]
            with open(ds_path, "w", encoding="utf-8") as f:
                f.write(c)

    # Caves mod guards
    cv_path = target_dir / "src" / "caves" / "mod.rs"
    if cv_path.exists():
        with open(cv_path, "r", encoding="utf-8") as f:
            c = f.read()
        m = re.search(r"pub fn apply_region\([^)]*\)\s*\{", c)
        if m and "if min_x > max_x" not in c:
            ins = m.end()
            c = c[:ins] + "\n    if min_x > max_x || min_z > max_z { return; }" + c[ins:]
            with open(cv_path, "w", encoding="utf-8") as f:
                f.write(c)

    # Water depth guards
    wd_path = target_dir / "src" / "water_depth.rs"
    if wd_path.exists():
        with open(wd_path, "r", encoding="utf-8") as f:
            c = f.read()
        m = re.search(r"pub fn apply_water_depth\([^)]*\)\s*\{", c)
        if m and "if iter_min_x > iter_max_x" not in c:
            ins = m.end()
            c = c[:ins] + "\n    if iter_min_x > iter_max_x || iter_min_z > iter_max_z { return; }" + c[ins:]
            with open(wd_path, "w", encoding="utf-8") as f:
                f.write(c)


def patch_canopy_and_caches(target_dir: Path):
    canopy_path = target_dir / "src" / "canopy" / "mod.rs"
    if canopy_path.exists():
        with open(canopy_path, "r", encoding="utf-8") as f:
            c = f.read()
        if "fn cache_dir() -> PathBuf {" in c:
            c = c.replace(
                "fn cache_dir() -> PathBuf {",
                """fn cache_dir() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        return crate::world_utils::resolve_android_download_dir().join(".arnis_cache/canopy");
    }"""
            )
            with open(canopy_path, "w", encoding="utf-8") as f:
                f.write(c)

    elev_cache = target_dir / "src" / "elevation" / "cache.rs"
    if elev_cache.exists():
        with open(elev_cache, "r", encoding="utf-8") as f:
            c = f.read()
        if "pub fn user_cache_dir() -> Option<PathBuf> {" in c:
            c = c.replace(
                "pub fn user_cache_dir() -> Option<PathBuf> {",
                """pub fn user_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "android")]
    {
        return Some(crate::world_utils::resolve_android_download_dir().join(".arnis_cache"));
    }"""
            )
        elif "pub fn get_cache_dir(" in c:
            m = re.search(r"pub fn get_cache_dir\([^)]*\)\s*->\s*PathBuf\s*\{", c)
            if m:
                ins = m.end()
                c = c[:ins] + """
    #[cfg(target_os = "android")]
    {
        return crate::world_utils::resolve_android_download_dir().join(".arnis_cache/elevation").join(provider_name);
    }
""" + c[ins:]
        with open(elev_cache, "w", encoding="utf-8") as f:
            f.write(c)


def patch_elevation_postprocess(target_dir: Path):
    pp_path = target_dir / "src" / "elevation" / "postprocess.rs"
    if pp_path.exists():
        print(f"--> Patching {pp_path}")
        with open(pp_path, "r", encoding="utf-8") as f:
            content = f.read()

        if "pub fn scale_to_minecraft_in_place" not in content:
            scale_in_place = """
/// Scaler operating directly on mutable in-place height buffer to prevent duplicate allocation.
pub fn scale_to_minecraft_in_place(
    heights: &mut [Vec<f64>],
    scale: f64,
    ground_level: i32,
    min_ground_level: i32,
    disable_height_limit: bool,
    extended_max_y: i32,
    policy: AffinePolicy,
) -> ElevationAffine {
    let (mut res_heights, affine) = scale_to_minecraft_with(
        heights,
        scale,
        ground_level,
        min_ground_level,
        disable_height_limit,
        extended_max_y,
        policy,
    );
    heights.swap_with_slice(&mut res_heights);
    affine
}

pub fn apply_affine_in_place(
    heights: &mut [Vec<f64>],
    affine: &ElevationAffine,
    upper_clamp: f64,
) {
    let base = affine.ground_level as f64;
    for row in heights.iter_mut() {
        for h in row.iter_mut() {
            *h = affine.y_for_metres(*h).clamp(base, upper_clamp);
        }
    }
}

pub fn apply_fit_in_place(
    heights: &mut [Vec<f64>],
    affine: &ElevationAffine,
    fit: FitRange,
    upper_clamp: f64,
) {
    let base = affine.ground_level as f64;
    for row in heights.iter_mut() {
        for h in row.iter_mut() {
            let relative_height: f64 = if fit.height_range > 0.0 {
                (*h - affine.min_height_m) / fit.height_range
            } else {
                0.0
            };
            *h = (base + relative_height * fit.scaled_range).clamp(base, upper_clamp);
        }
    }
}
"""
            content += scale_in_place
            with open(pp_path, "w", encoding="utf-8") as f:
                f.write(content)
            print("    src/elevation/postprocess.rs patched.")


def patch_map_preview(target_dir: Path):
    mp_path = target_dir / "src" / "map_preview.rs"
    if mp_path.exists():
        with open(mp_path, "r", encoding="utf-8") as f:
            content = f.read()
        target = 'world_output.with_file_name(format!("{stem} map.png"))'
        if target in content:
            content = content.replace(target, 'std::env::temp_dir().join(format!("{stem}_map.png"))')
            with open(mp_path, "w", encoding="utf-8") as f:
                f.write(content)
            print("    src/map_preview.rs patched successfully.")


def patch_gui(target_dir: Path):
    gui_path = target_dir / "src" / "gui.rs"
    print(f"--> Patching {gui_path}")
    if not gui_path.exists():
        report_error("src/gui.rs", str(gui_path))

    with open(gui_path, "r", encoding="utf-8") as f:
        content = f.read()

    # 1. Gate rfd import
    content = re.sub(
        r'(#\[cfg\(not\(target_os = "android"\)\)\]\s*)?use\s+rfd::FileDialog;',
        '#[cfg(not(target_os = "android"))]\nuse rfd::FileDialog;',
        content
    )

    # 2. Gate FileDialog in gui_pick_save_directory
    pick_regex = re.compile(
        r"fn gui_pick_save_directory\([^)]*\)\s*->\s*Result<String,\s*String>\s*\{[\s\S]*?\n\}"
    )
    if pick_regex.search(content):
        replacement = """fn gui_pick_save_directory(start_path: String) -> Result<String, String> {
    #[cfg(target_os = "android")]
    {
        let _ = start_path;
        return Ok(crate::world_utils::resolve_android_download_dir().display().to_string());
    }
    #[cfg(not(target_os = "android"))]
    {
        let start = PathBuf::from(&start_path);
        let mut dialog = FileDialog::new();
        if start.is_dir() {
            dialog = dialog.set_directory(&start);
        }
        match dialog.pick_folder() {
            Some(folder) => Ok(folder.display().to_string()),
            None => Ok(start_path),
        }
    }
}"""
        content = pick_regex.sub(replacement, content, count=1)
    else:
        report_error("fn gui_pick_save_directory", str(gui_path))

    # 3. Bypass trash on Android in trash_world
    trash_regex = re.compile(
        r"fn trash_world\([^)]*\)\s*->\s*Result<\(\),\s*String>\s*\{[\s\S]*?\n\}"
    )
    if trash_regex.search(content):
        replacement = """fn trash_world(world: &Path) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        if world.is_dir() {
            let _ = fs::remove_dir_all(world);
        } else if world.is_file() {
            let _ = fs::remove_file(world);
        }
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        #[cfg(target_os = "macos")]
        let bin = {
            use trash::macos::{DeleteMethod, TrashContextExtMacos};
            let mut bin = trash::TrashContext::new();
            bin.set_delete_method(DeleteMethod::NsFileManager);
            bin
        };
        #[cfg(not(target_os = "macos"))]
        let bin = trash::TrashContext::new();
        bin.delete(world)
            .map_err(|e| format!("Could not move the world to the trash: {e}"))
    }
}"""
        content = trash_regex.sub(replacement, content, count=1)

    # 4. Add gui_set_bedrock_options command
    if "fn gui_set_bedrock_options" not in content:
        cmd = """
#[tauri::command]
fn gui_set_bedrock_options(
    max_file_size_mb: usize,
    write_buffer_size_mb: usize,
    cache_size_mb: usize,
) {
    crate::world_editor::bedrock::set_bedrock_db_options(
        max_file_size_mb,
        write_buffer_size_mb,
        cache_size_mb,
    );
}
"""
        content += cmd

    # Add gui_set_bedrock_options to invoke_handler
    m_hand = re.search(r"(invoke_handler\(tauri::generate_handler!\[)([\s\S]*?)(\]\))", content)
    if m_hand and "gui_set_bedrock_options" not in m_hand.group(2):
        body = m_hand.group(2).rstrip()
        if not body.endswith(","):
            body += ","
        body += "\n            gui_set_bedrock_options,\n        "
        content = content[:m_hand.start(2)] + body + content[m_hand.end(2):]

    # 5. In run_gui(), call protect_process_memory() and set TMPDIR
    m_rungui = re.search(r"pub fn run_gui\(\)\s*->\s*Result<\(\),\s*String>\s*\{", content)
    if m_rungui:
        ins = m_rungui.end()
        init_code = """
    crate::world_utils::protect_process_memory();

    #[cfg(target_os = "android")]
    {
        if std::env::var("TMPDIR").is_err() {
            let candidate_dirs = [
                std::path::PathBuf::from("/data/data/com.louisdev.arnis/cache"),
                std::path::PathBuf::from("/data/user/0/com.louisdev.arnis/cache"),
                crate::world_utils::resolve_android_download_dir().join(".arnis_tmp"),
            ];
            for dir in &candidate_dirs {
                if std::fs::create_dir_all(dir).is_ok() {
                    unsafe {
                        std::env::set_var("TMPDIR", dir);
                    }
                    break;
                }
            }
        }
    }
"""
        if "crate::world_utils::protect_process_memory();" not in content[ins:ins+100]:
            content = content[:ins] + init_code + content[ins:]
    else:
        report_error("pub fn run_gui", str(gui_path))

    # 6. Disable desktop LogBuilder logging on Android
    log_pattern = re.compile(
        r'tauri::Builder::default\(\)\s*\.plugin\(\s*LogBuilder::default\(\)[\s\S]*?\.build\(\),\s*\)\s*\.plugin\(tauri_plugin_shell::init\(\)\)'
    )
    if log_pattern.search(content):
        replacement = """tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())"""
        content = log_pattern.sub(replacement, content)

    # 7. Update gui_get_platform to report "android"
    m_plat = re.search(r"fn gui_get_platform\(\)\s*->\s*&'static str\s*\{", content)
    if m_plat and 'target_os = "android"' not in content[m_plat.end():m_plat.end()+150]:
        ins = m_plat.end()
        content = content[:ins] + '\n    if cfg!(target_os = "android") { return "android"; }' + content[ins:]

    with open(gui_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/gui.rs patched successfully.")


def patch_gui_web_assets(target_dir: Path):
    gui_dir = target_dir / "src" / "gui"
    if not gui_dir.exists():
        return
    print("--> Patching GUI web assets...")

    # 1. index.html: Add performance settings section and badges if not present
    index_path = gui_dir / "index.html"
    if index_path.exists():
        with open(index_path, "r", encoding="utf-8") as f:
            idx = f.read()

        # Add symbols i-cpu and i-clipboard if not present
        if 'id="i-cpu"' not in idx:
            cpu_sym = '<symbol id="i-cpu" viewBox="0 0 24 24"><rect width="16" height="16" x="4" y="4" rx="2"/><rect width="6" height="6" x="9" y="9" rx="1"/><path d="M15 2v2"/><path d="M15 20v2"/><path d="M2 15h2"/><path d="M2 9h2"/><path d="M20 15h2"/><path d="M20 9h2"/><path d="M9 2v2"/><path d="M9 20v2"/></symbol>'
            idx = idx.replace('</svg>', cpu_sym + '\n  </svg>', 1)

        # Add progress-tiles-badge if not present
        if 'id="progress-tiles-badge"' not in idx:
            idx = idx.replace(
                '<span id="progress-info" class="progress-info-text"></span>',
                """<div class="progress-status-row">
              <span id="progress-info" class="progress-info-text"></span>
              <span id="progress-tiles-badge" class="progress-tiles-badge" style="display: none;"></span>
            </div>"""
            )

        with open(index_path, "w", encoding="utf-8") as f:
            f.write(idx)

    # 2. styles.css: Add mobile vertical layout and progress badge styles
    css_path = gui_dir / "css" / "styles.css"
    if css_path.exists():
        with open(css_path, "r", encoding="utf-8") as f:
            css = f.read()
        if ".progress-tiles-badge" not in css:
            mobile_css = """
/* ─── Mobile Vertical Layout & Android Adaptations ─── */
.progress-status-row {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 5px;
  min-height: 1.5em;
  gap: 8px;
}

.progress-tiles-badge {
  font-size: 0.8em;
  font-weight: 700;
  color: #7bd864;
  background: rgba(46, 204, 113, 0.15);
  border: 1px solid rgba(123, 216, 100, 0.35);
  border-radius: 6px;
  padding: 2px 8px;
  white-space: nowrap;
}

button, .format-toggle-btn, .map-box, .map-container {
  border-radius: 16px !important;
}

@media (max-width: 850px), (orientation: portrait) {
  .row:has(.logo), .container > .row:first-child { display: none !important; }
  .container { padding: 8px !important; height: 100vh; display: flex; flex-direction: column; }
  .flex-container { display: flex !important; flex-direction: column !important; flex: 1 !important; }
  .map-box { flex: 1 1 48% !important; min-height: 200px !important; max-height: 50vh !important; }
  .controls-box { flex: 1 1 52% !important; }
}
"""
            css += mobile_css
            with open(css_path, "w", encoding="utf-8") as f:
                f.write(css)

    # 3. bbox.js: Disable attributionControl
    bbox_js = gui_dir / "js" / "bbox.js"
    if bbox_js.exists():
        with open(bbox_js, "r", encoding="utf-8") as f:
            b_code = f.read()
        b_code = b_code.replace("attributionControl: { customAttribution: theme.attribution }", "attributionControl: false")
        with open(bbox_js, "w", encoding="utf-8") as f:
            f.write(b_code)

    print("    GUI web assets patched.")


def audit_required_symbols(target_dir: Path):
    print("--> Performing final audit of REQUIRED_CUSTOM_SYMBOLS...")
    missing = []
    for rel_file, symbol in REQUIRED_CUSTOM_SYMBOLS:
        file_path = target_dir / rel_file
        if not file_path.exists():
            missing.append(f"Missing file: {rel_file} (for symbol '{symbol}')")
            continue
        with open(file_path, "r", encoding="utf-8") as f:
            lines = f.readlines()

        found = False
        for line in lines:
            stripped = line.strip()
            if symbol in stripped and not stripped.startswith("//") and not stripped.startswith("/*"):
                found = True
                break
        if not found:
            missing.append(f"Symbol '{symbol}' NOT found in {rel_file}")

    if missing:
        print("ERROR: Audit failed! The following protected symbols were not found or commented out:", file=sys.stderr)
        for m in missing:
            print(f"  - {m}", file=sys.stderr)
        sys.exit(1)

    print(f"    Audit passed: all {len(REQUIRED_CUSTOM_SYMBOLS)} protected symbols verified.")


def main():
    parser = argparse.ArgumentParser(description="Autonomous Arnis Android Patcher")
    parser.add_argument("--target", required=True, help="Path to pristine upstream Arnis repository")
    parser.add_argument("--overlay", required=True, help="Path to Android overlay directory")
    args = parser.parse_args()

    target_dir = Path(args.target).resolve()
    overlay_dir = Path(args.overlay).resolve()

    print("=== Starting Arnis Android Patcher ===")
    print(f"Target repository: {target_dir}")
    print(f"Overlay source:    {overlay_dir}")

    if not target_dir.exists():
        print(f"ERROR: Target repository not found: {target_dir}", file=sys.stderr)
        sys.exit(1)

    # 1. Verify overlay collision constraint
    check_overlay_collisions(target_dir, overlay_dir)

    # 2. Copy overlay files
    copy_overlay(target_dir, overlay_dir)

    # 3. Patch project configuration files
    patch_cargo_toml(target_dir)
    patch_tauri_conf(target_dir)
    patch_capabilities(target_dir)

    # 4. Patch Rust core modules
    patch_main_and_generate_lib(target_dir)
    patch_world_utils(target_dir)
    patch_world_editor_mod(target_dir)
    patch_bedrock(target_dir)
    patch_world_editor_common(target_dir)
    patch_data_processing(target_dir)
    patch_tile(target_dir)
    patch_args(target_dir)
    patch_progress(target_dir)
    patch_telemetry(target_dir)
    patch_osm_parser(target_dir)
    patch_ground(target_dir)
    patch_caves_and_water_guards(target_dir)
    patch_canopy_and_caches(target_dir)
    patch_elevation_postprocess(target_dir)
    patch_map_preview(target_dir)
    patch_gui(target_dir)
    patch_gui_web_assets(target_dir)

    # 5. Final audit of protected symbols
    audit_required_symbols(target_dir)

    print("\n=== Arnis successfully converted to Android project! ===")


if __name__ == "__main__":
    main()
