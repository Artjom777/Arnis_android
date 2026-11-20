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


def report_error(anchor: str, file_path: str):
    print(f"ERROR: Missing anchor '{anchor}' in {file_path}", file=sys.stderr)
    sys.exit(1)


def copy_overlay(target_dir: Path, overlay_dir: Path):
    print(f"--> Copying overlay from {overlay_dir} to {target_dir}")
    if not overlay_dir.exists():
        print(f"ERROR: Overlay directory not found: {overlay_dir}", file=sys.stderr)
        sys.exit(1)

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
        lib_block = """\n[lib]
name = "arnis_lib"
crate-type = ["staticlib", "cdylib", "rlib"]
"""
        # Insert after [package] block
        m = re.search(r"(\[package\][\s\S]*?)(?=\n\[)", content)
        if m:
            content = content[:m.end()] + lib_block + content[m.end():]
        else:
            report_error("[package] section", str(cargo_path))

    # 2. Add [[bin]] section if not present
    if "[[bin]]" not in content:
        bin_block = """\n[[bin]]
name = "arnis"
path = "src/main.rs"
"""
        # Insert after [lib]
        m = re.search(r"(\[lib\][\s\S]*?)(?=\n\[)", content)
        if m:
            content = content[:m.end()] + bin_block + content[m.end():]
        else:
            report_error("[lib] section", str(cargo_path))

    # 3. Patch [features]: remove desktop-only dependencies from gui feature
    m_feat = re.search(r'gui\s*=\s*\[(.*?)\]', content, re.DOTALL)
    if m_feat:
        features_str = m_feat.group(1)
        # Parse features
        feats = [x.strip().strip('"').strip("'") for x in features_str.split(",") if x.strip()]
        # Remove desktop-only features
        feats = [x for x in feats if x not in ("rfd", "trash", "webkit2gtk")]
        # Ensure needed features are present
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
    content = re.sub(
        r'reqwest\s*=\s*\{[^}]+\}',
        'reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls-webpki-roots", "gzip"] }',
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
    if "[target.'cfg(not(target_os = \"android\"))'.dependencies]" not in content:
        content += desktop_deps

    # 8. Add patch sections for rusty-leveldb
    patch_block = """
[patch.crates-io]
rusty-leveldb = { path = "third_party/rusty-leveldb" }

[patch."https://github.com/louis-e/leveldb-rs"]
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

    # Remove desktop window dimensions and Chromium browser arguments
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
    lib_mods = ["pub mod android_support;"]
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
    #[cfg(target_os = "android")]
    crate::android_support::protect_process_memory();

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
    gui_anchor = "let gui_mode = std::env::args().len() == 1;"
    if gui_anchor not in main_content:
        report_error("gui_mode check in main()", str(main_path))

    # Replace the gui_mode block with arnis_lib::run(); return;
    gui_block_match = re.search(r"let gui_mode\s*=[^;]+;[\s\S]*?if gui_mode\s*\{", main_content)
    if not gui_block_match:
        report_error("if gui_mode { block", str(main_path))

    idx = gui_block_match.end()
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

    # 1. Add android_support re-exports at top
    header = """#[cfg(target_os = "android")]
pub use crate::android_support::*;
#[cfg(not(target_os = "android"))]
pub fn collect_memory(_force: bool) {}
#[cfg(not(target_os = "android"))]
pub fn protect_process_memory() {}
"""
    if "pub use crate::android_support::*;" not in content:
        content = header + "\n" + content

    # 2. Patch default_bedrock_output_directory / get_bedrock_output_directory
    anchors_found = 0
    if "pub fn default_bedrock_output_directory() -> PathBuf {" in content:
        content = content.replace(
            "pub fn default_bedrock_output_directory() -> PathBuf {",
            "pub fn default_bedrock_output_directory() -> PathBuf {\n    #[cfg(target_os = \"android\")]\n    return resolve_android_download_dir();"
        )
        anchors_found += 1

    if "pub fn get_bedrock_output_directory() -> PathBuf {" in content:
        content = content.replace(
            "pub fn get_bedrock_output_directory() -> PathBuf {",
            "pub fn get_bedrock_output_directory() -> PathBuf {\n    #[cfg(target_os = \"android\")]\n    return resolve_android_download_dir();"
        )
        anchors_found += 1

    if anchors_found == 0:
        report_error("default_bedrock_output_directory / get_bedrock_output_directory", str(wu_path))

    # 3. Patch build_bedrock_output
    bbox_anchor = "pub fn build_bedrock_output("
    if bbox_anchor not in content:
        report_error("pub fn build_bedrock_output(", str(wu_path))

    m_bbo = re.search(r"pub fn build_bedrock_output\([^)]*\)\s*->\s*\(PathBuf,\s*String\)\s*\{", content)
    if not m_bbo:
        report_error("build_bedrock_output signature", str(wu_path))

    insert_pos = m_bbo.end()
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
    content = content[:insert_pos] + fs_override + content[insert_pos:]

    with open(wu_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/world_utils.rs patched successfully.")


def patch_bedrock(target_dir: Path):
    br_path = target_dir / "src" / "world_editor" / "bedrock.rs"
    print(f"--> Patching {br_path}")
    if not br_path.exists():
        report_error("src/world_editor/bedrock.rs", str(br_path))

    with open(br_path, "r", encoding="utf-8") as f:
        content = f.read()

    # 1. MonotonicDiskEnv rename on Android FUSE
    rename_anchor = "fn rename(&self, from: &Path, to: &Path) -> rusty_leveldb::Result<()> {"
    if rename_anchor not in content:
        report_error("MonotonicDiskEnv::rename", str(br_path))
    content = content.replace(
        rename_anchor,
        rename_anchor + """
        #[cfg(target_os = "android")]
        if to.exists() {
            let _ = fs::remove_file(to);
        }"""
    )

    # 2. MonotonicDiskEnv lock on Android FUSE
    lock_match = re.search(r"fn lock\(&self,\s*p:\s*&Path\)\s*->\s*rusty_leveldb::Result<FileLock>\s*\{([\s\S]*?)\n    \}", content)
    if not lock_match:
        report_error("MonotonicDiskEnv::lock", str(br_path))

    lock_body = lock_match.group(1)
    if "let res =" not in lock_body:
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

    # 3. MonotonicDiskEnv unlock on Android
    unlock_match = re.search(r"fn unlock\(&self,\s*l:\s*FileLock\)\s*->\s*rusty_leveldb::Result<\(\)>\s*\{([\s\S]*?)\n    \}", content)
    if unlock_match:
        unlock_body = unlock_match.group(1).strip()
        new_unlock = """fn unlock(&self, l: FileLock) -> rusty_leveldb::Result<()> {
        let res = """ + unlock_body + """;
        #[cfg(target_os = "android")]
        if res.is_err() {
            return Ok(());
        }
        res
    }"""
        content = content[:unlock_match.start()] + new_unlock + content[unlock_match.end():]

    # 4. place_archive Android FUSE overwrite
    place_anchor = "fn place_archive(part: &Path, archive: &Path) -> Result<PathBuf, BedrockSaveError> {"
    if place_anchor in content:
        content = content.replace(
            place_anchor,
            place_anchor + """
    #[cfg(target_os = "android")]
    if archive.exists() {
        let _ = fs::remove_file(archive);
    }"""
        )

    # 5. private_staging_root for Android cache
    staging_anchor = "fn private_staging_root() -> Option<PathBuf> {"
    if staging_anchor in content:
        content = content.replace(
            staging_anchor,
            staging_anchor + """
    #[cfg(target_os = "android")]
    return Some(crate::world_utils::resolve_android_download_dir().join(".arnis_cache/bedrock-staging"));"""
        )

    # 6. Large file support in zip options
    content = content.replace(
        "SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)",
        "SimpleFileOptions::default().compression_method(CompressionMethod::Deflated).large_file(true)"
    )
    content = content.replace(
        "FileOptions::default().compression_method(CompressionMethod::Deflated)",
        "FileOptions::default().compression_method(CompressionMethod::Deflated).large_file(true)"
    )

    with open(br_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/world_editor/bedrock.rs patched successfully.")


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
    m_pick = re.search(r"fn gui_pick_save_directory\([^)]*\)\s*->\s*Result<String,\s*String>\s*\{", content)
    if m_pick:
        ins = m_pick.end()
        pick_override = """
    #[cfg(target_os = "android")]
    {
        let _ = start_path;
        return Ok(crate::world_utils::resolve_android_download_dir().display().to_string());
    }
"""
        content = content[:ins] + pick_override + content[ins:]
    else:
        report_error("fn gui_pick_save_directory", str(gui_path))

    # 3. Bypass trash on Android in trash_world / gui_trash_one_world
    m_trash = re.search(r"fn trash_world\([^)]*\)\s*->\s*Result<\(\),\s*String>\s*\{", content)
    if m_trash:
        ins = m_trash.end()
        trash_override = """
    #[cfg(target_os = "android")]
    {
        if world.is_dir() {
            let _ = fs::remove_dir_all(world);
        }
        return Ok(());
    }
"""
        content = content[:ins] + trash_override + content[ins:]

    # 4. In run_gui(), call protect_process_memory() and set TMPDIR
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
        content = content[:ins] + init_code + content[ins:]
    else:
        report_error("pub fn run_gui", str(gui_path))

    # 5. Disable desktop LogBuilder logging on Android
    log_pattern = re.compile(
        r'tauri::Builder::default\(\)\s*\.plugin\(\s*LogBuilder::default\(\)[\s\S]*?\.build\(\),\s*\)\s*\.plugin\(tauri_plugin_shell::init\(\)\)'
    )
    if log_pattern.search(content):
        replacement = """tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())"""
        content = log_pattern.sub(replacement, content)

    with open(gui_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/gui.rs patched successfully.")


def patch_elevation_and_canopy_caches(target_dir: Path):
    elev_path = target_dir / "src" / "elevation" / "cache.rs"
    if elev_path.exists():
        print(f"--> Patching {elev_path}")
        with open(elev_path, "r", encoding="utf-8") as f:
            content = f.read()

        if "pub fn user_cache_dir() -> Option<PathBuf> {" in content:
            content = content.replace(
                "pub fn user_cache_dir() -> Option<PathBuf> {",
                "pub fn user_cache_dir() -> Option<PathBuf> {\n    #[cfg(target_os = \"android\")]\n    return Some(crate::world_utils::resolve_android_download_dir().join(\".arnis_cache\"));"
            )
        elif "pub fn get_cache_dir(" in content:
            m = re.search(r"pub fn get_cache_dir\([^)]*\)\s*->\s*PathBuf\s*\{", content)
            if m:
                ins = m.end()
                content = content[:ins] + '\n    #[cfg(target_os = "android")]\n    return crate::world_utils::resolve_android_download_dir().join(".arnis_cache/elevation").join(provider_name);\n' + content[ins:]

        with open(elev_path, "w", encoding="utf-8") as f:
            f.write(content)
        print("    src/elevation/cache.rs patched successfully.")

    canopy_path = target_dir / "src" / "canopy" / "mod.rs"
    if canopy_path.exists():
        print(f"--> Patching {canopy_path}")
        with open(canopy_path, "r", encoding="utf-8") as f:
            content = f.read()

        if "fn cache_dir() -> PathBuf {" in content:
            content = content.replace(
                "fn cache_dir() -> PathBuf {",
                "fn cache_dir() -> PathBuf {\n    #[cfg(target_os = \"android\")]\n    return crate::world_utils::resolve_android_download_dir().join(\".arnis_cache/canopy\");"
            )

        with open(canopy_path, "w", encoding="utf-8") as f:
            f.write(content)
        print("    src/canopy/mod.rs patched successfully.")


def patch_osm_parser(target_dir: Path):
    osm_path = target_dir / "src" / "osm_parser.rs"
    print(f"--> Patching {osm_path}")
    if not osm_path.exists():
        report_error("src/osm_parser.rs", str(osm_path))

    with open(osm_path, "r", encoding="utf-8") as f:
        content = f.read()

    anchor = "drop(ways_map);"
    if anchor not in content:
        report_error("drop(ways_map);", str(osm_path))

    content = content.replace(
        anchor,
        "drop(ways_map);\n    crate::world_utils::collect_memory(true);"
    )

    with open(osm_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/osm_parser.rs patched successfully.")


def patch_world_editor_common(target_dir: Path):
    wc_path = target_dir / "src" / "world_editor" / "common.rs"
    print(f"--> Patching {wc_path}")
    if not wc_path.exists():
        report_error("src/world_editor/common.rs", str(wc_path))

    with open(wc_path, "r", encoding="utf-8") as f:
        content = f.read()

    anchor = "impl WorldToModify {"
    if anchor not in content:
        report_error("impl WorldToModify {", str(wc_path))

    if "pub fn set_block(&mut self, x: i32" not in content:
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
        content = content.replace(anchor, anchor + helper)

    with open(wc_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/world_editor/common.rs patched successfully.")


def patch_args(target_dir: Path):
    args_path = target_dir / "src" / "args.rs"
    print(f"--> Patching {args_path}")
    if not args_path.exists():
        report_error("src/args.rs", str(args_path))

    with open(args_path, "r", encoding="utf-8") as f:
        content = f.read()

    anchor = "pub one_world: bool,"
    if anchor not in content:
        report_error("pub one_world: bool,", str(args_path))

    if "pub tiled_generation: bool," not in content:
        tiled_args = """pub one_world: bool,

    /// Generate the world in parts/tiles (subdivided by regions) and assemble into one world.
    #[arg(long, default_value_t = true)]
    pub tiled_generation: bool,

    /// Size of each tile in blocks for tiled generation (32..2048, default 1024 = 2x2 Minecraft regions).
    #[arg(long, default_value_t = 1024)]
    pub tile_size: i32,

    /// User-configured RAM limit in megabytes (0 = auto-detect based on system memory).
    #[arg(long, default_value_t = 0)]
    pub ram_limit_mb: u64,"""
        content = content.replace(anchor, tiled_args, 1)

    with open(args_path, "w", encoding="utf-8") as f:
        f.write(content)
    print("    src/args.rs patched successfully.")


def patch_caves_and_water_guards(target_dir: Path):
    # deepslate guard
    ds_path = target_dir / "src" / "caves" / "deepslate.rs"
    if ds_path.exists():
        print(f"--> Patching {ds_path}")
        with open(ds_path, "r", encoding="utf-8") as f:
            content = f.read()
        m = re.search(r"pub\(super\)\s+fn\s+apply_region\([^)]*\)\s*\{", content)
        if m:
            ins = m.end()
            content = content[:ins] + "\n    if iter_min_x > iter_max_x || iter_min_z > iter_max_z {\n        return;\n    }\n" + content[ins:]
            with open(ds_path, "w", encoding="utf-8") as f:
                f.write(content)
            print("    src/caves/deepslate.rs patched successfully.")

    # caves/mod.rs guard
    caves_path = target_dir / "src" / "caves" / "mod.rs"
    if caves_path.exists():
        print(f"--> Patching {caves_path}")
        with open(caves_path, "r", encoding="utf-8") as f:
            content = f.read()
        m = re.search(r"pub\s+fn\s+carve_region\([^)]*\)\s*\{", content)
        if m:
            ins = m.end()
            content = content[:ins] + "\n    if min_x > max_x || min_z > max_z {\n        return;\n    }\n" + content[ins:]
            with open(caves_path, "w", encoding="utf-8") as f:
                f.write(content)
            print("    src/caves/mod.rs patched successfully.")

    # water_depth.rs guard
    water_path = target_dir / "src" / "water_depth.rs"
    if water_path.exists():
        print(f"--> Patching {water_path}")
        with open(water_path, "r", encoding="utf-8") as f:
            content = f.read()
        m = re.search(r"pub\s+fn\s+carve_lc_water_region\([^)]*\)\s*\{", content)
        if m:
            ins = m.end()
            content = content[:ins] + "\n    if iter_min_x > iter_max_x || iter_min_z > iter_max_z {\n        return;\n    }\n" + content[ins:]
            with open(water_path, "w", encoding="utf-8") as f:
                f.write(content)
            print("    src/water_depth.rs patched successfully.")


def main():
    parser = argparse.ArgumentParser(description="Autonomous Arnis -> Android Converter")
    parser.add_argument("--target", required=True, help="Path to clean upstream Arnis repository")
    parser.add_argument("--overlay", default="./overlay", help="Path to overlay folder (default: ./overlay)")
    args = parser.parse_args()

    target_dir = Path(args.target).resolve()
    overlay_dir = Path(args.overlay).resolve()

    print(f"=== Starting Arnis Android Patcher ===")
    print(f"Target repository: {target_dir}")
    print(f"Overlay source:    {overlay_dir}")

    if not target_dir.exists():
        print(f"ERROR: Target directory does not exist: {target_dir}", file=sys.stderr)
        sys.exit(1)

    copy_overlay(target_dir, overlay_dir)
    patch_cargo_toml(target_dir)
    patch_tauri_conf(target_dir)
    patch_main_and_generate_lib(target_dir)
    patch_world_utils(target_dir)
    patch_bedrock(target_dir)
    patch_gui(target_dir)
    patch_elevation_and_canopy_caches(target_dir)
    patch_osm_parser(target_dir)
    patch_world_editor_common(target_dir)
    patch_args(target_dir)
    patch_caves_and_water_guards(target_dir)

    print("\n=== Arnis successfully converted to Android project! ===")


if __name__ == "__main__":
    main()
