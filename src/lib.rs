pub mod args;
pub mod bedrock_block_map;
pub mod bench;
pub mod biome;
pub mod block_definitions;
pub mod block_palette;
pub mod bresenham;
pub mod canopy;
pub mod celestial;
pub mod climate;
pub mod clipping;
pub mod colors;
pub mod coordinate_system;
pub mod data_processing;
pub mod decals;
pub mod deterministic_rng;
pub mod element_processing;
pub mod elevation;
pub mod elevation_data;
pub mod floodfill;
pub mod floodfill_cache;
pub mod ground;
pub mod ground_generation;
pub mod land_cover;
pub mod landmarks;
pub mod luanti_block_map;
pub mod map_item;
pub mod map_item_palette;
pub mod map_preview;
pub mod map_renderer;
pub mod map_transformation;
pub mod models_3d;
pub mod net;
pub mod ore_generation;
pub mod osm_parser;
pub mod overture;
#[cfg(feature = "gui")]
pub mod preview_3d;
#[cfg(feature = "gui")]
pub mod progress;
pub mod projection;
pub mod retrieve_data;
pub mod structures;
#[cfg(feature = "gui")]
pub mod telemetry;
#[cfg(test)]
pub mod test_utilities;
pub mod tile;
pub mod trees;
pub mod version_check;
pub mod water_depth;
pub mod world_editor;
pub mod world_utils;
#[cfg(feature = "gui")]
pub mod gui;
#[cfg(not(feature = "gui"))]
pub mod progress {
    pub fn emit_gui_error(_message: &str) {}
    pub fn emit_gui_progress_update(_progress: f64, _message: &str) {}
    pub fn emit_gui_progress_update_ex(_progress: f64, _message: &str, _streaming: bool) {}
    pub fn emit_map_preview_ready() {}
    pub fn emit_show_in_folder(_path: &str) {}
    pub fn is_running_with_gui() -> bool { false }
}

#[cfg(not(target_os = "android"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(feature = "gui")]
    {
        let _ = std::panic::catch_unwind(|| {
            if let Err(e) = gui::run_gui() {
                eprintln!("GUI error: {e}");
            }
        });
    }
}
