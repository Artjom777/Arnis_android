//! Bedrock Edition .mcworld format world saving.
//!
//! This module handles saving worlds in the Bedrock Edition format,
//! producing .mcworld files that can be imported into Minecraft Bedrock.

use super::bedrock_entities;
use super::common::{BlockStorage, ChunkToModify, SectionToModify, WorldToModify};
use super::WorldMetadata;
use crate::bedrock_block_map::{
    is_waterlogged, potted_plant, to_bedrock_block_with_properties, BedrockBlock,
    BedrockBlockStateValue, BLOCK_STATE_VERSION,
};
use crate::block_definitions::{Block, AIR};
use crate::coordinate_system::cartesian::XZBBox;
use crate::coordinate_system::geographic::LLBBox;
use crate::ground::Ground;
use crate::progress::emit_gui_progress_update;
use crate::world_utils::WorldSizeOptions;

use bedrockrs_level::level::db_interface::key_level::KeyTypeTag;
use bedrockrs_level::level::db_interface::rusty::mcpe_options;
use bedrockrs_shared::world::dimension::Dimension;
use byteorder::{LittleEndian, WriteBytesExt};
use fastnbt::Value;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use rusty_leveldb::env::{Env, FileLock, Logger, RandomAccess};
use rusty_leveldb::{Options, PosixDiskEnv, DB};
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap as StdHashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Cursor, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;
use vek::Vec2;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;
use zip::ZipWriter;

/// Error type for Bedrock world saving operations
#[derive(Debug)]
pub enum BedrockSaveError {
    Io(std::io::Error),
    Zip(zip::result::ZipError),
    Serialization(serde_json::Error),
    Database(rusty_leveldb::Status),
    Nbt(String),
}

impl BedrockSaveError {
    /// The cause in a few words, for the GUI's one-line status.
    pub fn reason(&self) -> &'static str {
        use std::io::ErrorKind;
        let kind = match self {
            BedrockSaveError::Io(e) | BedrockSaveError::Zip(zip::result::ZipError::Io(e)) => {
                Some(e.kind())
            }
            BedrockSaveError::Database(s)
                if s.code == rusty_leveldb::StatusCode::PermissionDenied =>
            {
                Some(ErrorKind::PermissionDenied)
            }
            // rusty-leveldb keeps only the message of other io errors.
            BedrockSaveError::Database(s)
                if s.err.contains("os error 28") || s.err.contains("os error 112") =>
            {
                Some(ErrorKind::StorageFull)
            }
            _ => None,
        };
        match (kind, self) {
            (Some(ErrorKind::PermissionDenied), _) => "access denied",
            (Some(ErrorKind::StorageFull), _) => "disk is full",
            (Some(ErrorKind::AlreadyExists), _) => "folder in the way",
            (_, BedrockSaveError::Database(_)) => "LevelDB error",
            (_, BedrockSaveError::Io(_) | BedrockSaveError::Zip(_)) => "file error",
            (_, BedrockSaveError::Serialization(_) | BedrockSaveError::Nbt(_)) => "encoding error",
        }
    }
}

impl std::fmt::Display for BedrockSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BedrockSaveError::Io(err) => {
                write!(f, "I/O error while writing Bedrock world: {err}")
            }
            BedrockSaveError::Zip(err) => {
                write!(f, "Failed to package Bedrock world archive: {err}")
            }
            BedrockSaveError::Serialization(err) => {
                write!(f, "Failed to serialize Bedrock metadata: {err}")
            }
            BedrockSaveError::Database(status) => {
                write!(f, "LevelDB error: {status}")
            }
            BedrockSaveError::Nbt(err) => {
                write!(f, "NBT serialization error: {err}")
            }
        }
    }
}

impl std::error::Error for BedrockSaveError {}

impl From<std::io::Error> for BedrockSaveError {
    fn from(err: std::io::Error) -> Self {
        BedrockSaveError::Io(err)
    }
}

impl From<zip::result::ZipError> for BedrockSaveError {
    fn from(err: zip::result::ZipError) -> Self {
        BedrockSaveError::Zip(err)
    }
}

impl From<serde_json::Error> for BedrockSaveError {
    fn from(err: serde_json::Error) -> Self {
        BedrockSaveError::Serialization(err)
    }
}

impl From<rusty_leveldb::Status> for BedrockSaveError {
    fn from(status: rusty_leveldb::Status) -> Self {
        BedrockSaveError::Database(status)
    }
}

const DEFAULT_BEDROCK_COMPRESSION_LEVEL: u8 = 6;

static BEDROCK_MAX_FILE_SIZE_MB: AtomicUsize = AtomicUsize::new(64);
static BEDROCK_WRITE_BUFFER_SIZE_MB: AtomicUsize = AtomicUsize::new(64);
static BEDROCK_CACHE_SIZE_MB: AtomicUsize = AtomicUsize::new(64);

/// Configures LevelDB options for Bedrock export in megabytes.
pub fn set_bedrock_db_options(max_file_size_mb: usize, write_buffer_mb: usize, cache_mb: usize) {
    if max_file_size_mb >= 1 {
        BEDROCK_MAX_FILE_SIZE_MB.store(max_file_size_mb, AtomicOrdering::Relaxed);
    }
    if write_buffer_mb >= 1 {
        BEDROCK_WRITE_BUFFER_SIZE_MB.store(write_buffer_mb, AtomicOrdering::Relaxed);
    }
    if cache_mb >= 1 {
        BEDROCK_CACHE_SIZE_MB.store(cache_mb, AtomicOrdering::Relaxed);
    }
}

/// LevelDB options for Bedrock export with generous capacities and safe eviction.
fn bedrock_db_options() -> Options {
    let mut opts = mcpe_options(DEFAULT_BEDROCK_COMPRESSION_LEVEL);
    opts.env = Rc::new(Box::new(MonotonicDiskEnv::new()));
    let max_file_mb = BEDROCK_MAX_FILE_SIZE_MB.load(AtomicOrdering::Relaxed);
    let write_buffer_mb = BEDROCK_WRITE_BUFFER_SIZE_MB.load(AtomicOrdering::Relaxed);
    let cache_mb = BEDROCK_CACHE_SIZE_MB.load(AtomicOrdering::Relaxed);
    opts.write_buffer_size = write_buffer_mb.max(1) * 1024 * 1024;
    opts.max_file_size = max_file_mb.max(1) * 1024 * 1024;
    opts.max_open_files = 1000;
    opts.block_cache_capacity_bytes = cache_mb.max(1) * 1024 * 1024;
    opts
}

/// The stock disk env with a monotonic clock. rusty-leveldb times compactions
/// by subtracting wall-clock readings, which overflows when the system clock
/// steps backwards mid-compaction. The timings only feed its stats.
struct MonotonicDiskEnv {
    inner: PosixDiskEnv,
    epoch: Instant,
}

impl MonotonicDiskEnv {
    fn new() -> Self {
        Self {
            inner: PosixDiskEnv::new(),
            epoch: Instant::now(),
        }
    }
}

impl Env for MonotonicDiskEnv {
    fn open_sequential_file(&self, p: &Path) -> rusty_leveldb::Result<Box<dyn std::io::Read>> {
        self.inner.open_sequential_file(p)
    }
    fn open_random_access_file(&self, p: &Path) -> rusty_leveldb::Result<Box<dyn RandomAccess>> {
        self.inner.open_random_access_file(p)
    }
    fn open_writable_file(&self, p: &Path) -> rusty_leveldb::Result<Box<dyn IoWrite>> {
        self.inner.open_writable_file(p)
    }
    fn open_appendable_file(&self, p: &Path) -> rusty_leveldb::Result<Box<dyn IoWrite>> {
        self.inner.open_appendable_file(p)
    }
    fn exists(&self, p: &Path) -> rusty_leveldb::Result<bool> {
        self.inner.exists(p)
    }
    fn children(&self, p: &Path) -> rusty_leveldb::Result<Vec<PathBuf>> {
        self.inner.children(p)
    }
    fn size_of(&self, p: &Path) -> rusty_leveldb::Result<usize> {
        self.inner.size_of(p)
    }
    fn delete(&self, p: &Path) -> rusty_leveldb::Result<()> {
        self.inner.delete(p)
    }
    fn mkdir(&self, p: &Path) -> rusty_leveldb::Result<()> {
        self.inner.mkdir(p)
    }
    fn rmdir(&self, p: &Path) -> rusty_leveldb::Result<()> {
        self.inner.rmdir(p)
    }
    fn rename(&self, from: &Path, to: &Path) -> rusty_leveldb::Result<()> {
        match self.inner.rename(from, to) {
            Ok(()) => Ok(()),
            Err(e) => {
                #[cfg(target_os = "android")]
                {
                    // On Android FUSE filesystems, atomic rename fails if the target file already exists
                    // (e.g. updating CURRENT file in LevelDB).
                    if to.exists() {
                        let _ = fs::remove_file(to);
                        if self.inner.rename(from, to).is_ok() {
                            return Ok(());
                        }
                    }
                    Err(e)
                }
                #[cfg(not(target_os = "android"))]
                Err(e)
            }
        }
    }
    fn lock(&self, p: &Path) -> rusty_leveldb::Result<FileLock> {
        match self.inner.lock(p) {
            Ok(lock) => Ok(lock),
            Err(e) => {
                #[cfg(target_os = "android")]
                {
                    eprintln!("Warning: ignoring database lock error on Android FUSE: {e:?}");
                    Ok(FileLock {
                        id: p.to_str().unwrap_or("dummy_lock").to_string(),
                    })
                }
                #[cfg(not(target_os = "android"))]
                Err(e)
            }
        }
    }
    fn unlock(&self, l: FileLock) -> rusty_leveldb::Result<()> {
        #[cfg(target_os = "android")]
        {
            let _ = self.inner.unlock(l);
            Ok(())
        }
        #[cfg(not(target_os = "android"))]
        self.inner.unlock(l)
    }
    fn new_logger(&self, p: &Path) -> rusty_leveldb::Result<Logger> {
        self.inner.new_logger(p)
    }
    fn micros(&self) -> u64 {
        // Offset by one so the first reading is never 0, like a wall clock's.
        self.epoch.elapsed().as_micros() as u64 + 1
    }
    fn sleep_for(&self, micros: u32) {
        self.inner.sleep_for(micros)
    }
}

/// Marks the staging directory as ours. Never packaged,  names its files.
const STAGING_MARKER: &str = ".arnis-staging";

/// A LevelDB (key, value) record, and all records for one chunk.
type DbRecord = (Vec<u8>, Vec<u8>);
type ChunkRecords = Vec<DbRecord>;

/// The potted plants of a section by section index.
type PottedPlants = Vec<(usize, BedrockBlock)>;

/// A section in Bedrock's terms: its blocks, the water of waterlogged cells, its pots.
struct BedrockSection {
    palette: Vec<BedrockBlock>,
    indices: Box<[u16; 4096]>,
    water: Option<Box<[bool; 4096]>>,
    pots: PottedPlants,
}

/// Metadata for Bedrock worlds
#[derive(Serialize)]
struct BedrockMetadata {
    #[serde(flatten)]
    world: WorldMetadata,
    format: &'static str,
    chunk_count: usize,
}

/// Bedrock block state for NBT serialization
#[derive(Serialize)]
struct BedrockBlockState {
    name: String,
    states: BTreeMap<String, Value>,
    version: i32,
}

/// Local player entity structure saved into LevelDB key .
/// Tells Minecraft Bedrock the player is already positioned at the designated coordinates,
/// preventing the game from executing its safe-spawn relocation / danger avoidance algorithm.
#[derive(serde::Serialize)]
struct BedrockLocalPlayer {
    #[serde(rename = "Pos")]
    pos: Vec<f64>,
    #[serde(rename = "Rotation")]
    rotation: Vec<f32>,
    #[serde(rename = "Motion")]
    motion: Vec<f64>,
    #[serde(rename = "DimensionId")]
    dimension_id: i32,
    #[serde(rename = "Dimension")]
    dimension: i32,
    #[serde(rename = "SpawnX")]
    spawn_x: i32,
    #[serde(rename = "SpawnY")]
    spawn_y: i32,
    #[serde(rename = "SpawnZ")]
    spawn_z: i32,
    #[serde(rename = "SpawnDimension")]
    spawn_dimension: i32,
    #[serde(rename = "FallDistance")]
    fall_distance: f32,
    #[serde(rename = "OnGround")]
    on_ground: u8,
    #[serde(rename = "PlayerGameMode")]
    player_game_mode: i32,
}

/// Writer for Bedrock Edition worlds
pub struct BedrockWriter {
    pub output_dir: PathBuf,
    pub mcworld_path: PathBuf,
    pub level_name: String,
    pub spawn_point: Option<(i32, i32)>,
    pub ground: Option<Arc<Ground>>,
    pub ground_origin: Option<(i32, i32)>,
    pub center_lat: f64,
    pub extend_build_height: bool,
    pub projection: String,
    pub scale: f64,
    pub game_mode: crate::args::GameMode,
    pub world_time: i64,
    pub start_with_map: bool,
    pub mob_spawning: crate::args::MobSpawning,
    pub size_options: WorldSizeOptions,
    pub addon: Option<crate::bedrock_addon::Addon>,
    pub db: Option<DB>,
    pub prepared: bool,
    pub flushed_chunks_count: usize,
    pub resolved_spawn: Option<(i32, i32, i32)>,
}

impl BedrockWriter {
    /// Creates a new BedrockWriter
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        output_path: PathBuf,
        level_name: String,
        spawn_point: Option<(i32, i32)>,
        ground: Option<Arc<Ground>>,
        extend_build_height: bool,
        projection: String,
        scale: f64,
        game_mode: crate::args::GameMode,
        world_time: i64,
        start_with_map: bool,
    ) -> Self {
        // If the path ends with .mcworld, use it as the final archive path
        // and create a temp directory without that extension for working files
        let (output_dir, mcworld_path) =
            if output_path.extension().is_some_and(|ext| ext == "mcworld") {
                #[cfg(target_os = "android")]
                {
                    let file_stem = output_path.file_stem().and_then(|s| s.to_str()).unwrap_or("arnis_world");
                    let download_dir = crate::world_utils::resolve_android_download_dir();
                    let candidate_bases = [
                        std::env::var_os("TMPDIR").map(PathBuf::from),
                        Some(PathBuf::from("/data/data/com.louisdev.arnis/cache")),
                        Some(PathBuf::from("/data/user/0/com.louisdev.arnis/cache")),
                        dirs::cache_dir(),
                        Some(download_dir.join(".arnis_staging")),
                    ];
                    let staging_base = candidate_bases
                        .into_iter()
                        .flatten()
                        .find(|p| fs::create_dir_all(p).is_ok())
                        .unwrap_or_else(|| download_dir.join(".arnis_staging"));
                    let staging_dir = staging_base.join(format!("arnis_staging_{}", file_stem));
                    let _ = fs::create_dir_all(&download_dir);
                    let final_path = download_dir.join(
                        output_path.file_name().unwrap_or_default()
                    );
                    (staging_dir, final_path)
                }
                #[cfg(not(target_os = "android"))]
                (output_path.with_extension(""), output_path)
            } else {
                let mcworld_path = append_mcworld_extension(&output_path);
                #[cfg(target_os = "android")]
                {
                    let file_stem = output_path.file_name().and_then(|s| s.to_str()).unwrap_or("arnis_world");
                    let download_dir = crate::world_utils::resolve_android_download_dir();
                    let candidate_bases = [
                        std::env::var_os("TMPDIR").map(PathBuf::from),
                        Some(PathBuf::from("/data/data/com.louisdev.arnis/cache")),
                        Some(PathBuf::from("/data/user/0/com.louisdev.arnis/cache")),
                        dirs::cache_dir(),
                        Some(download_dir.join(".arnis_staging")),
                    ];
                    let staging_base = candidate_bases
                        .into_iter()
                        .flatten()
                        .find(|p| fs::create_dir_all(p).is_ok())
                        .unwrap_or_else(|| download_dir.join(".arnis_staging"));
                    let staging_dir = staging_base.join(format!("arnis_staging_{}", file_stem));
                    let _ = fs::create_dir_all(&download_dir);
                    let final_path = download_dir.join(
                        mcworld_path.file_name().unwrap_or_default()
                    );
                    (staging_dir, final_path)
                }
                #[cfg(not(target_os = "android"))]
                (output_path, mcworld_path)
            };

        // If a world with this filename already exists, rename it with (2), (3), etc.
        // rather than replacing/overwriting it.
        let (mcworld_path, level_name) = if mcworld_path.exists() {
            let parent = mcworld_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let stem = mcworld_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("arnis_world")
                .to_string();
            let mut counter = 2;
            loop {
                let candidate = parent.join(format!("{stem} ({counter}).mcworld"));
                if !candidate.exists() {
                    break (candidate, format!("{level_name} ({counter})"));
                }
                counter += 1;
            }
        } else {
            (mcworld_path, level_name)
        };

        let writer = Self {
            output_dir,
            mcworld_path,
            level_name,
            spawn_point,
            ground,
            ground_origin: None,
            center_lat: 0.0,
            extend_build_height,
            projection,
            scale,
            game_mode,
            world_time,
            start_with_map,
            mob_spawning: crate::args::MobSpawning::Off,
            size_options: WorldSizeOptions::default(),
            addon: None,
            db: None,
            prepared: false,
            flushed_chunks_count: 0,
            resolved_spawn: None,
        };
        writer.cleanup_destination_pngs();
        writer
    }

    /// Packs to ship inside the world, as the moving vehicles.
    pub fn with_addon(mut self, addon: Option<crate::bedrock_addon::Addon>) -> Self {
        self.addon = addon;
        self
    }

    /// Where the ground grid starts, which the biomes and the spawn height read it from.
    pub fn with_ground_origin(mut self, origin: (i32, i32)) -> Self {
        self.ground_origin = Some(origin);
        self
    }

    /// What spawns on its own, which also picks the difficulty.
    pub fn with_mob_spawning(mut self, mobs: crate::args::MobSpawning) -> Self {
        self.mob_spawning = mobs;
        self
    }

    /// The run's options, which size the world for the staging check.
    pub fn with_size_options(mut self, options: WorldSizeOptions) -> Self {
        self.size_options = options;
        self
    }

    /// Ensures the LevelDB database in the staging directory is open for streaming writes.
    pub fn ensure_db_open(&mut self) -> Result<&mut DB, BedrockSaveError> {
        if self.db.is_none() {
            self.prepare_output_dir()?;
            let db_path = self.output_dir.join("db");
            let mut opts = bedrock_db_options();
            opts.create_if_missing = true;
            let db = DB::open(db_path.into_boxed_path(), opts)?;
            self.db = Some(db);
        }
        Ok(self.db.as_mut().unwrap())
    }

    /// Stream a single sealed region directly to LevelDB on disk and release its memory.
    pub fn flush_single_region(
        &mut self,
        region_x: i32,
        region_z: i32,
        mut region: super::common::RegionToModify,
    ) -> Result<(), BedrockSaveError> {
        self.flushed_chunks_count += region.chunks.len();
        let db = self.ensure_db_open()?;
        const ENCODE_BATCH_CHUNKS: usize = if cfg!(target_os = "android") { 64 } else { 128 };
        let chunk_coords: Vec<(i32, i32)> = region.chunks.keys().copied().collect();
        for chunk_coord_batch in chunk_coords.chunks(ENCODE_BATCH_CHUNKS) {
            let mut chunk_batch: Vec<(i32, i32, ChunkToModify)> =
                Vec::with_capacity(chunk_coord_batch.len());
            for &coord in chunk_coord_batch {
                if let Some(chunk) = region.chunks.remove(&coord) {
                    chunk_batch.push((region_x * 32 + coord.0, region_z * 32 + coord.1, chunk));
                }
            }

            let ground_origin = self.ground_origin;
            let center_lat = self.center_lat;
            let ground = self.ground.clone();
            let extend_build_height = self.extend_build_height;

            let encoded: Vec<ChunkRecords> = chunk_batch
                .par_iter()
                .map(|(abs_x, abs_z, chunk)| {
                    Self::encode_chunk_records(
                        *abs_x,
                        *abs_z,
                        chunk,
                        ground_origin,
                        center_lat,
                        ground.as_deref(),
                        extend_build_height,
                    )
                })
                .collect::<Result<_, _>>()?;

            drop(chunk_batch);

            for records in encoded {
                for (key, value) in &records {
                    db.put(key, value)?;
                }
            }
        }
        Ok(())
    }

    /// Writes the world to disk
    pub fn write_world(
        &mut self,
        world: &WorldToModify,
        xzbbox: &XZBBox,
        llbbox: &LLBBox,
    ) -> Result<(), BedrockSaveError> {
        self.ground_origin
            .get_or_insert((xzbbox.min_x(), xzbbox.min_z()));
        self.center_lat = (llbbox.min().lat() + llbbox.max().lat()) * 0.5;

        self.prepare_output_dir()?;
        self.write_level_name()?;

        emit_gui_progress_update(91.0, "Saving Bedrock world...");
        self.write_level_dat(world, xzbbox)?;
        self.ground = None;

        crate::world_utils::collect_memory(true);

        emit_gui_progress_update(92.0, "Saving Bedrock world...");
        self.write_metadata(world, xzbbox, llbbox)?;

        self.write_chunks_to_db(world)?;

        emit_gui_progress_update(97.0, "Saving Bedrock world...");
        crate::world_utils::collect_memory(true);

        emit_gui_progress_update(98.0, "Saving Bedrock world...");
        self.package_mcworld()?;

        emit_gui_progress_update(99.0, "Saving Bedrock world...");
        self.cleanup_temp_dir()?;

        crate::world_utils::collect_memory(true);
        Ok(())
    }

    /// Prepares the staging directory, reusing leftovers but never deleting foreign data.
    pub fn prepare_output_dir(&mut self) -> Result<(), BedrockSaveError> {
        if self.prepared {
            return Ok(());
        }
        cleanup_orphaned_staging_dirs();
        if self.output_dir.exists() {
            if !is_arnis_staging_dir(&self.output_dir) {
                return Err(BedrockSaveError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "'{}' already exists and was not created by Arnis.                          Rename or move it, or choose a different Bedrock save path.",
                        self.output_dir.display()
                    ),
                )));
            }
            fs::remove_dir_all(&self.output_dir)?;
        }
        if self.mcworld_path.exists() {
            let _ = fs::remove_file(&self.mcworld_path);
        }

        fs::create_dir_all(&self.output_dir)?;
        fs::write(self.output_dir.join(STAGING_MARKER), b"")?;
        self.prepared = true;
        Ok(())
    }

    fn write_level_name(&self) -> Result<(), BedrockSaveError> {
        let levelname_path = self.output_dir.join("levelname.txt");
        fs::write(levelname_path, &self.level_name)?;
        Ok(())
    }

    fn write_level_dat(
        &mut self,
        world: &WorldToModify,
        xzbbox: &XZBBox,
    ) -> Result<(), BedrockSaveError> {
        let (spawn_x, spawn_z, spawn_y) = if let Some((user_x, user_z)) = self.spawn_point {
            let sx = user_x;
            let sz = user_z;

            let (origin_x, origin_z) = self.ground_origin.unwrap_or((xzbbox.min_x(), xzbbox.min_z()));
            let rel_x = sx - origin_x;
            let rel_z = sz - origin_z;
            let coord = crate::coordinate_system::cartesian::XZPoint::new(rel_x, rel_z);
            let ground_lvl = self.ground.as_ref().map(|g| g.level(coord)).unwrap_or(-60);

            let mut highest_solid_y = None;
            for y in (ground_lvl.max(-60)..=318).rev() {
                if let Some(block) = world.get_block(sx, y, sz) {
                    if block != crate::block_definitions::AIR
                        && block != crate::block_definitions::WATER
                    {
                        highest_solid_y = Some(y);
                        break;
                    }
                }
            }

            let surface_y = highest_solid_y.unwrap_or(ground_lvl.max(-60));
            let sy = (surface_y + 1).clamp(-62, 317);
            (sx, sz, sy)
        } else {
            let min_x = xzbbox.min_x().max(0);
            let min_z = xzbbox.min_z().max(0);
            let max_x = xzbbox.max_x().max(min_x);
            let max_z = xzbbox.max_z().max(min_z);

            let (origin_x, origin_z) = self.ground_origin.unwrap_or((xzbbox.min_x(), xzbbox.min_z()));
            let rel_0_x = 0 - origin_x;
            let rel_0_z = 0 - origin_z;
            let coord_0 = crate::coordinate_system::cartesian::XZPoint::new(rel_0_x, rel_0_z);
            let ground_lvl = self.ground.as_ref().map(|g| g.level(coord_0)).unwrap_or(-60);

            let mut best_coord = None;
            let max_search_r = max_x.max(max_z).min(2048).max(16);

            'search: for r in 2..=max_search_r {
                for z in 0..=r {
                    let x = r;
                    if x <= 1 && z <= 1 {
                        continue;
                    }
                    if x < min_x || x > max_x || z < min_z || z > max_z {
                        continue;
                    }

                    let rel_x = x - origin_x;
                    let rel_z = z - origin_z;
                    let ground_at = self
                        .ground
                        .as_ref()
                        .map(|g| g.level(crate::coordinate_system::cartesian::XZPoint::new(rel_x, rel_z)))
                        .unwrap_or(ground_lvl);

                    for y in (ground_at.max(-60)..=318).rev() {
                        if let Some(block) = world.get_block(x, y, z) {
                            if block != crate::block_definitions::AIR
                                && block != crate::block_definitions::WATER
                            {
                                best_coord = Some((x, z, y));
                                break 'search;
                            }
                        }
                    }
                }

                for x in 0..r {
                    let z = r;
                    if x <= 1 && z <= 1 {
                        continue;
                    }
                    if x < min_x || x > max_x || z < min_z || z > max_z {
                        continue;
                    }

                    let rel_x = x - origin_x;
                    let rel_z = z - origin_z;
                    let ground_at = self
                        .ground
                        .as_ref()
                        .map(|g| g.level(crate::coordinate_system::cartesian::XZPoint::new(rel_x, rel_z)))
                        .unwrap_or(ground_lvl);

                    for y in (ground_at.max(-60)..=318).rev() {
                        if let Some(block) = world.get_block(x, y, z) {
                            if block != crate::block_definitions::AIR
                                && block != crate::block_definitions::WATER
                            {
                                best_coord = Some((x, z, y));
                                break 'search;
                            }
                        }
                    }
                }
            }

            let fallback_x = 2.max(min_x);
            let fallback_z = 2.max(min_z);
            let (sx, sz, surface_y) =
                best_coord.unwrap_or((fallback_x, fallback_z, ground_lvl.max(-60)));
            let sy = (surface_y + 1).clamp(-62, 317);

            (sx, sz, sy)
        };

        self.resolved_spawn = Some((spawn_x, spawn_y, spawn_z));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let version_array: Vec<i32> = if self.extend_build_height {
            vec![1, 21, 40, 0, 0]
        } else {
            vec![1, 21, 0, 0, 0]
        };

        let (spawn_mobs, difficulty) = match self.mob_spawning {
            crate::args::MobSpawning::Off => (false, 0),
            crate::args::MobSpawning::Easy => (true, 1),
            crate::args::MobSpawning::Normal => (true, 2),
            crate::args::MobSpawning::Hard => (true, 3),
        };

        let level_dat = BedrockLevelDat {
            storage_version: 10,
            network_version: 685,
            world_version: 1,
            inventory_version: "1.21.0".to_string(),
            last_opened_with_version: version_array.clone(),
            minimum_compatible_client_version: version_array,

            level_name: self.level_name.clone(),
            random_seed: 0,

            spawn_x,
            spawn_y,
            spawn_z,

            generator: 2,
            flat_world_layers: r#"{"biome_id":1,"encoding_version":6,"preset_id":"TheVoid","world_version":"version.post_1_18"}"#.to_string(),
            spawn_mobs,

            game_type: self.game_mode.bedrock_game_type(),
            difficulty,
            force_game_type: false,

            last_played: now,
            time: self.world_time,
            current_tick: 0,

            commands_enabled: true,
            cheats_enabled: true,
            command_blocks_enabled: true,
            command_block_output: true,

            multiplayer_game: true,
            multiplayer_game_intent: true,
            lan_broadcast: true,
            lan_broadcast_intent: true,
            xbl_broadcast_intent: 3,
            platform_broadcast_intent: 3,
            platform: 2,

            do_daylight_cycle: true,
            do_weather_cycle: true,
            do_mob_spawning: spawn_mobs,
            do_mob_loot: true,
            do_tile_drops: true,
            do_entity_drops: true,
            do_fire_tick: true,
            mob_griefing: true,
            natural_regeneration: true,
            pvp: true,
            keep_inventory: false,
            send_command_feedback: true,
            show_coordinates: false,
            show_death_messages: true,
            tnt_explodes: true,
            respawn_blocks_explode: true,
            projectiles_can_break_blocks: true,

            drowning_damage: true,
            fall_damage: true,
            fire_damage: true,
            freeze_damage: true,

            rain_level: 0.0,
            rain_time: 100000,
            lightning_level: 0.0,
            lightning_time: 100000,

            nether_scale: 8,
            spawn_radius: 0,
            spawn_radius_camel: 0,
            spawn_radius_pascal: 0,
            random_tick_speed: 1,
            function_command_limit: 10000,
            max_command_chain_length: 65535,
            server_chunk_tick_range: 4,
            limited_world_depth: 0,
            limited_world_width: 0,
            limited_world_origin_x: spawn_x,
            limited_world_origin_y: spawn_y,
            limited_world_origin_z: spawn_z,
            world_start_count: 0xFFFFFFFE_u64 as i64,

            bonus_chest_enabled: false,
            bonus_chest_spawned: false,
            has_been_loaded_in_creative: true,
            has_locked_behavior_pack: false,
            has_locked_resource_pack: false,
            immutable_world: false,
            is_from_locked_template: false,
            is_from_world_template: false,
            is_single_use_world: false,
            is_world_template_option_locked: false,
            texture_packs_required: false,
            use_msa_gamertags_only: false,
            center_maps_to_origin: false,
            confirmed_platform_locked_content: false,
            education_features_enabled: false,
            start_with_map_enabled: self.start_with_map,
            requires_copied_pack_removal_check: false,
            spawn_v1_villagers: false,
            is_hardcore: false,
            is_created_in_editor: false,
            is_exported_from_editor: false,
            is_random_seed_allowed: false,
            has_uncomplete_world_file_on_disk: false,
            player_has_died: false,
            do_insomnia: true,
            do_immediate_respawn: false,
            do_limited_crafting: false,
            recipes_unlock: true,
            show_tags: true,
            show_recipe_messages: true,
            show_border_effect: true,
            show_days_played: false,
            locator_bar: true,
            tnt_explosion_drop_decay: true,

            saved_with_toggled_experiments: self.extend_build_height,
            experiments_ever_used: self.extend_build_height,
            experiments: BedrockExperiments {
                data_driven_biomes: self.extend_build_height,
            },

            editor_world_type: 0,
            edu_offer: 0,
            biome_override: "".to_string(),
            prid: "".to_string(),
            players_sleeping_percentage: 100,
            permissions_level: 0,
            player_permissions_level: 1,
            daylight_cycle: 0,
        };

        let nbt_bytes = nbtx::to_le_bytes(&level_dat)
            .map_err(|e| BedrockSaveError::Nbt(format!("level.dat: {e}")))?;

        let mut file = File::create(self.output_dir.join("level.dat"))?;
        file.write_u32::<LittleEndian>(10)?;
        file.write_u32::<LittleEndian>(nbt_bytes.len() as u32)?;
        file.write_all(&nbt_bytes)?;

        let _ = fs::copy(self.output_dir.join("level.dat"), self.output_dir.join("level.dat_old"));
        Ok(())
    }

    pub fn write_chunks_to_db(&mut self, world: &WorldToModify) -> Result<(), BedrockSaveError> {
        let mut db = if let Some(db) = self.db.take() {
            db
        } else {
            let db_path = self.output_dir.join("db");
            let mut opts = bedrock_db_options();
            opts.create_if_missing = true;
            DB::open(db_path.into_boxed_path(), opts)?
        };

        if let Some((sx, sy, sz)) = self.resolved_spawn {
            let game_type = self.game_mode.bedrock_game_type();
            let player = BedrockLocalPlayer {
                pos: vec![sx as f64 + 0.5, sy as f64, sz as f64 + 0.5],
                rotation: vec![0.0, 0.0],
                motion: vec![0.0, 0.0, 0.0],
                dimension_id: 0,
                dimension: 0,
                spawn_x: sx,
                spawn_y: sy,
                spawn_z: sz,
                spawn_dimension: 0,
                fall_distance: 0.0,
                on_ground: 1,
                player_game_mode: game_type,
            };
            if let Ok(player_bytes) = nbtx::to_le_bytes(&player) {
                let _ = db.put(b"~local_player", &player_bytes);
            }
        }

        const ENCODE_BATCH_CHUNKS: usize = if cfg!(target_os = "android") { 64 } else { 128 };

        let chunks: Vec<(i32, i32, &ChunkToModify)> = world
            .regions
            .iter()
            .flat_map(|((region_x, region_z), region)| {
                region
                    .chunks
                    .iter()
                    .map(move |((local_x, local_z), chunk)| {
                        (region_x * 32 + local_x, region_z * 32 + local_z, chunk)
                    })
            })
            .collect();

        let total_chunks = chunks.len();
        if total_chunks == 0 {
            db.flush()?;
            drop(db);
            return Ok(());
        }

        let progress_bar = ProgressBar::new(total_chunks as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:45.white/black}] {pos}/{len} chunks ({eta})")
                .unwrap()
                .progress_chars("█▓░"),
        );

        let mut chunks_processed: usize = 0;
        let ground_origin = self.ground_origin;
        let center_lat = self.center_lat;
        let ground = self.ground.clone();
        let extend_build_height = self.extend_build_height;

        for chunk_batch in chunks.chunks(ENCODE_BATCH_CHUNKS) {
            let encoded: Vec<ChunkRecords> = chunk_batch
                .par_iter()
                .map(|&(abs_x, abs_z, chunk)| {
                    Self::encode_chunk_records(
                        abs_x,
                        abs_z,
                        chunk,
                        ground_origin,
                        center_lat,
                        ground.as_deref(),
                        extend_build_height,
                    )
                })
                .collect::<Result<_, _>>()?;

            for records in encoded {
                for (key, value) in &records {
                    db.put(key, value)?;
                }
                chunks_processed += 1;
                progress_bar.inc(1);
                if chunks_processed.is_multiple_of(10) || chunks_processed == total_chunks {
                    let chunk_progress = chunks_processed as f64 / total_chunks as f64;
                    let progress_pct = 92.0 + (chunk_progress * 5.0);
                    emit_gui_progress_update(
                        progress_pct,
                        &format!(
                            "Saving Bedrock chunks: {}/{} ({:.0}%)",
                            chunks_processed, total_chunks, chunk_progress * 100.0
                        ),
                    );
                }
            }

            if cfg!(target_os = "android") {
                crate::world_utils::collect_memory(false);
            }
        }

        db.flush()?;
        drop(db);

        progress_bar.finish_with_message("Chunks written to LevelDB");
        Ok(())
    }

    /// Encodes every LevelDB record for one chunk: version, Data3D, subchunks, then entities.
    fn encode_chunk_records(
        abs_chunk_x: i32,
        abs_chunk_z: i32,
        chunk: &ChunkToModify,
        ground_origin: Option<(i32, i32)>,
        center_lat: f64,
        ground: Option<&Ground>,
        extend_build_height: bool,
    ) -> Result<ChunkRecords, BedrockSaveError> {
        let chunk_pos = Vec2::new(abs_chunk_x, abs_chunk_z);
        let mut records: ChunkRecords = Vec::with_capacity(chunk.sections.len() + 4);

        records.push((
            build_chunk_key_bytes(chunk_pos, Dimension::Overworld, KeyTypeTag::Version, None),
            vec![42],
        ));
        records.push((
            build_chunk_key_bytes(chunk_pos, Dimension::Overworld, KeyTypeTag::Data3D, None),
            Self::create_data3d(
                abs_chunk_x,
                abs_chunk_z,
                chunk,
                ground_origin,
                center_lat,
                ground,
                extend_build_height,
            ),
        ));

        let mut potted_across_sections: PottedPlants = Vec::new();
        for (&section_y, section) in &chunk.sections {
            let overrides = chunk.bedrock_overrides.as_ref();
            let (bytes, potted) = Self::encode_subchunk(section, section_y, overrides)?;
            potted_across_sections.extend(potted);
            records.push((
                build_chunk_key_bytes(
                    chunk_pos,
                    Dimension::Overworld,
                    KeyTypeTag::SubChunkPrefix,
                    Some(section_y),
                ),
                bytes,
            ));
        }

        let block_entities = bedrock_entities::build_block_entities(
            chunk_pos,
            chunk.other.get("block_entities"),
            &potted_across_sections,
        );
        if let Some(record) = Self::encode_compound_list_record(
            chunk_pos,
            KeyTypeTag::BlockEntity,
            block_entities.as_slice(),
        )? {
            records.push(record);
        }

        if let Some(record) = Self::encode_raw_compound_list_record(
            chunk_pos,
            KeyTypeTag::Entity,
            chunk.other.get("entities"),
        )? {
            records.push(record);
        }

        Ok(records)
    }

    /// Creates a Data3D record containing heightmap and biome data.
    fn create_data3d(
        abs_chunk_x: i32,
        abs_chunk_z: i32,
        chunk: &ChunkToModify,
        ground_origin: Option<(i32, i32)>,
        center_lat: f64,
        ground: Option<&Ground>,
        extend_build_height: bool,
    ) -> Vec<u8> {
        let mut buffer = Vec::with_capacity(512 + 64);
        for height in column_heights(chunk) {
            buffer.extend_from_slice(&height.to_le_bytes());
        }

        let origin = ground_origin.unwrap_or_default();
        let names = crate::biome::chunk_biome_names(
            abs_chunk_x,
            abs_chunk_z,
            ground,
            center_lat,
            origin,
        );
        write_biome_storage(&mut buffer, &names.map(bedrock_biome_id));
        let (min_section, max_section) = Self::section_span(extend_build_height);
        buffer.extend(std::iter::repeat_n(
            0xFF,
            (max_section - min_section) as usize,
        ));
        buffer
    }

    fn section_span(extend_build_height: bool) -> (i32, i32) {
        if extend_build_height {
            (-32, 31)
        } else {
            (-4, 19)
        }
    }

    fn encode_subchunk(
        section: &SectionToModify,
        y_index: i8,
        overrides: Option<&StdHashMap<usize, BedrockBlock>>,
    ) -> Result<(Vec<u8>, PottedPlants), BedrockSaveError> {
        let mut buffer = Cursor::new(Vec::new());
        let section = Self::build_palette_and_indices(section, overrides);

        buffer.write_u8(9)?;
        let layers = 1 + section.water.is_some() as u8;
        buffer.write_u8(layers)?;
        buffer.write_i8(y_index)?;

        let palette_count = section.palette.len() as u32;
        let bits_per_block = bedrock_bits_per_block(palette_count);
        buffer.write_u8(bits_per_block << 1)?;

        write_packed_indices(&mut buffer, bits_per_block, |idx| {
            section.indices[idx] as u32
        })?;

        buffer.write_u32::<LittleEndian>(palette_count)?;
        for block in &section.palette {
            let state = BedrockBlockState {
                name: block.name.clone(),
                states: block
                    .states
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v)))
                    .collect(),
                version: BLOCK_STATE_VERSION,
            };
            let nbt_bytes = nbtx::to_le_bytes(&state).map_err(|e| {
                BedrockSaveError::Nbt(format!("block palette state ({}): {e}", state.name))
            })?;
            buffer.write_all(&nbt_bytes)?;
        }

        if let Some(water) = section.water {
            let palette = [
                BedrockBlock::simple("air"),
                BedrockBlock::with_properties(
                    "water",
                    vec![("liquid_depth", BedrockBlockStateValue::Int(0))],
                ),
            ];
            write_block_storage(&mut buffer, &palette, |i| u32::from(water[i]))?;
        }

        Ok((buffer.into_inner(), section.pots))
    }

    fn build_palette_and_indices(
        section: &SectionToModify,
        overrides: Option<&StdHashMap<usize, BedrockBlock>>,
    ) -> BedrockSection {
        let mut palette: Vec<BedrockBlock> = Vec::new();
        let mut palette_map: StdHashMap<BedrockBlock, u16> = StdHashMap::new();
        let mut indices = Box::new([0u16; 4096]);
        let mut water = Box::new([false; 4096]);
        let mut pots = Vec::new();

        palette_slot(&mut palette, &mut palette_map, BedrockBlock::simple("air"));

        if let BlockStorage::Uniform(block) = section.storage {
            if section.properties.is_empty() && overrides.is_none() && potted_plant(block).is_none()
            {
                let idx = palette_slot(
                    &mut palette,
                    &mut palette_map,
                    to_bedrock_block_with_properties(block, None),
                );
                let wet = is_waterlogged(block, None);
                return BedrockSection {
                    palette,
                    indices: Box::new([idx; 4096]),
                    water: wet.then(|| Box::new([true; 4096])),
                    pots,
                };
            }
        }

        let mut converted: StdHashMap<(Block, usize), (u16, bool, bool)> = StdHashMap::new();
        converted.insert((AIR, 0), (0, false, false));
        for x in 0..16usize {
            for z in 0..16usize {
                for y in 0..16usize {
                    let internal_idx = y * 256 + z * 16 + x;
                    let bedrock_idx = x * 256 + z * 16 + y;
                    if let Some(frame) = overrides.and_then(|o| o.get(&internal_idx)) {
                        indices[bedrock_idx] =
                            palette_slot(&mut palette, &mut palette_map, frame.clone());
                        continue;
                    }
                    let block = section.get_block_at_index(internal_idx);
                    let properties = section.properties.get(&internal_idx);
                    let key = (block, properties.map_or(0, |p| Arc::as_ptr(p) as usize));
                    let (idx, wet, potted) = *converted.entry(key).or_insert_with(|| {
                        let properties = properties.map(|p| p.as_ref());
                        let bedrock_block = to_bedrock_block_with_properties(block, properties);
                        (
                            palette_slot(&mut palette, &mut palette_map, bedrock_block),
                            is_waterlogged(block, properties),
                            potted_plant(block).is_some(),
                        )
                    });
                    indices[bedrock_idx] = idx;
                    water[bedrock_idx] = wet;
                    if potted {
                        pots.extend(potted_plant(block).map(|plant| (internal_idx, plant)));
                    }
                }
            }
        }

        let any_water = water.iter().any(|&w| w);
        BedrockSection {
            palette,
            indices,
            water: any_water.then_some(water),
            pots,
        }
    }

    fn encode_compound_list_record(
        chunk_pos: Vec2<i32>,
        key_type: KeyTypeTag,
        compounds: &[StdHashMap<String, Value>],
    ) -> Result<Option<DbRecord>, BedrockSaveError> {
        if compounds.is_empty() {
            return Ok(None);
        }

        let mut data: Vec<u8> = Vec::new();
        for compound in compounds {
            let mut stripped = compound.clone();
            strip_empty_lists(&mut stripped);
            let bytes = nbtx::to_le_bytes(&stripped)
                .map_err(|e| BedrockSaveError::Nbt(format!("compound record: {e}")))?;
            data.extend_from_slice(&bytes);
        }

        let key = build_chunk_key_bytes(chunk_pos, Dimension::Overworld, key_type, None);
        Ok(Some((key, data)))
    }

    fn encode_raw_compound_list_record(
        chunk_pos: Vec2<i32>,
        key_type: KeyTypeTag,
        value: Option<&Value>,
    ) -> Result<Option<DbRecord>, BedrockSaveError> {
        let Some(Value::List(values)) = value else {
            return Ok(None);
        };
        if values.is_empty() {
            return Ok(None);
        }
        let deduped = dedup_compound_list(values);
        if deduped.is_empty() {
            return Ok(None);
        }

        let mut data: Vec<u8> = Vec::new();
        for compound in &deduped {
            let bytes = nbtx::to_le_bytes(compound)
                .map_err(|e| BedrockSaveError::Nbt(format!("entity compound: {e}")))?;
            data.extend_from_slice(&bytes);
        }

        let key = build_chunk_key_bytes(chunk_pos, Dimension::Overworld, key_type, None);
        Ok(Some((key, data)))
    }

    fn write_metadata(
        &self,
        world: &WorldToModify,
        xzbbox: &XZBBox,
        llbbox: &LLBBox,
    ) -> Result<(), BedrockSaveError> {
        let chunk_count: usize = self.flushed_chunks_count
            + world
                .regions
                .values()
                .map(|region| region.chunks.len())
                .sum::<usize>();

        let metadata = BedrockMetadata {
            world: WorldMetadata {
                min_mc_x: xzbbox.min_x(),
                max_mc_x: xzbbox.max_x(),
                min_mc_z: xzbbox.min_z(),
                max_mc_z: xzbbox.max_z(),
                min_geo_lat: llbbox.min().lat(),
                max_geo_lat: llbbox.max().lat(),
                min_geo_lon: llbbox.min().lng(),
                max_geo_lon: llbbox.max().lng(),
                projection: self.projection.clone(),
                scale: self.scale,
            },
            format: "bedrock-mcworld",
            chunk_count,
        };

        let metadata_bytes = serde_json::to_vec_pretty(&metadata)?;
        let metadata_path = self.output_dir.join("metadata.json");
        let mut file = File::create(metadata_path)?;
        file.write_all(&metadata_bytes)?;
        Ok(())
    }

    fn open_mcworld_target(path: &Path) -> Result<File, BedrockSaveError> {
        #[cfg(unix)]
        {
            use std::os::unix::io::FromRawFd;

            if let Ok(val) = std::env::var("ARNIS_MCWORLD_FD")
                .or_else(|_| std::env::var("MCWORLD_FD"))
                .or_else(|_| std::env::var("TARGET_FD"))
            {
                if let Ok(fd) = val.parse::<i32>() {
                    return Ok(unsafe { File::from_raw_fd(fd) });
                }
            }

            if let Some(s) = path.to_str() {
                if let Some(fd_str) = s
                    .strip_prefix("/proc/self/fd/")
                    .or_else(|| s.strip_prefix("fd://"))
                    .or_else(|| s.strip_prefix("fd:"))
                {
                    if let Ok(fd) = fd_str.parse::<i32>() {
                        return Ok(unsafe { File::from_raw_fd(fd) });
                    }
                }
            }
        }

        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match File::create(path) {
            Ok(file) => Ok(file),
            Err(e) => {
                #[cfg(target_os = "android")]
                {
                    eprintln!(
                        "Failed to create mcworld at {}: {}, attempting fallback to resolved download dir",
                        path.display(),
                        e
                    );
                    let fallback_dir = crate::world_utils::resolve_android_download_dir();
                    let _ = fs::create_dir_all(&fallback_dir);
                    let fallback_path = fallback_dir.join(path.file_name().unwrap_or_default());
                    File::create(&fallback_path).map_err(BedrockSaveError::Io)
                }
                #[cfg(not(target_os = "android"))]
                Err(BedrockSaveError::Io(e))
            }
        }
    }

    fn package_mcworld(&self) -> Result<(), BedrockSaveError> {
        let file = Self::open_mcworld_target(&self.mcworld_path)?;
        let buf_writer = BufWriter::with_capacity(64 * 1024, file);
        let mut writer = ZipWriter::new(buf_writer);
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .large_file(true);

        for file_name in ["levelname.txt", "metadata.json", "level.dat", "level.dat_old"] {
            let path = self.output_dir.join(file_name);
            if path.exists() {
                writer.start_file(file_name, options)?;
                let mut f = File::open(&path)?;
                std::io::copy(&mut f, &mut writer)?;
            }
        }

        const WORLD_ICON: &[u8] = include_bytes!("../../assets/minecraft/world_icon.jpeg");
        writer.start_file("world_icon.jpeg", options)?;
        IoWrite::write_all(&mut writer, WORLD_ICON)?;

        let mut behavior_packs: Vec<String> = Vec::new();
        let mut resource_packs: Vec<String> = Vec::new();

        if self.extend_build_height {
            const BP_MANIFEST: &[u8] =
                include_bytes!("../../assets/minecraft/bp_tall/manifest.json");
            const BP_OVERWORLD: &[u8] =
                include_bytes!("../../assets/minecraft/bp_tall/dimensions/overworld.json");
            const BP_HEADER_UUID: &str = "a7f3b2e0-8c4d-4e92-9b1a-3d7f5c8e4a61";

            writer.add_directory("behavior_packs/", options)?;
            writer.add_directory("behavior_packs/arnis_tall/", options)?;
            writer.add_directory("behavior_packs/arnis_tall/dimensions/", options)?;

            writer.start_file("behavior_packs/arnis_tall/manifest.json", options)?;
            IoWrite::write_all(&mut writer, BP_MANIFEST)?;

            writer.start_file(
                "behavior_packs/arnis_tall/dimensions/overworld.json",
                options,
            )?;
            IoWrite::write_all(&mut writer, BP_OVERWORLD)?;

            behavior_packs.push(BP_HEADER_UUID.to_string());
        }

        if let Some(addon) = &self.addon {
            let mut folders: BTreeSet<String> = BTreeSet::new();
            for (root, pack) in [
                ("behavior_packs", &addon.behavior),
                ("resource_packs", &addon.resources),
            ] {
                for (path, _) in &pack.files {
                    let full = format!("{root}/{}/{path}", pack.folder);
                    let mut end = 0;
                    while let Some(slash) = full[end..].find('/') {
                        end += slash + 1;
                        folders.insert(full[..end].to_string());
                    }
                }
            }
            if self.extend_build_height {
                folders.remove("behavior_packs/");
            }
            for folder in &folders {
                writer.add_directory(folder.as_str(), options)?;
            }
            for (root, pack) in [
                ("behavior_packs", &addon.behavior),
                ("resource_packs", &addon.resources),
            ] {
                for (path, bytes) in &pack.files {
                    writer.start_file(format!("{root}/{}/{path}", pack.folder), options)?;
                    IoWrite::write_all(&mut writer, bytes)?;
                }
            }
            behavior_packs.push(addon.behavior.id.clone());
            resource_packs.push(addon.resources.id.clone());
        }

        for (file, ids) in [
            ("world_behavior_packs.json", &behavior_packs),
            ("world_resource_packs.json", &resource_packs),
        ] {
            if ids.is_empty() {
                continue;
            }
            let list: Vec<String> = ids
                .iter()
                .map(|id| format!(r#"{{"pack_id":"{id}","version":[1,0,0]}}"#))
                .collect();
            writer.start_file(file, options)?;
            IoWrite::write_all(&mut writer, format!("[{}]", list.join(",")).as_bytes())?;
        }

        let db_path = self.output_dir.join("db");
        if db_path.is_dir() {
            add_directory_to_zip(&mut writer, &db_path, "db", options)?;
        }

        let mut buf_writer = writer.finish()?;
        IoWrite::flush(&mut buf_writer)?;
        drop(buf_writer);
        self.cleanup_destination_pngs();
        crate::world_utils::collect_memory(true);
        Ok(())
    }

    fn cleanup_temp_dir(&self) -> Result<(), BedrockSaveError> {
        if self.output_dir.exists() {
            let _ = fs::remove_dir_all(&self.output_dir);
        }
        cleanup_orphaned_staging_dirs();
        self.cleanup_destination_pngs();
        Ok(())
    }

    fn cleanup_destination_pngs(&self) {
        let parent = self.mcworld_path.parent();
        let stem = self.mcworld_path.file_stem().and_then(|s| s.to_str()).unwrap_or("arnis_world");
        Self::cleanup_png_in_dir(parent, stem);

        #[cfg(target_os = "android")]
        {
            let download_dir = crate::world_utils::resolve_android_download_dir();
            Self::cleanup_png_in_dir(Some(&download_dir), stem);
        }
    }

    fn cleanup_png_in_dir(dir: Option<&Path>, stem: &str) {
        if let Some(dir) = dir {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("png")) {
                        let name = path.file_name().unwrap_or_default().to_string_lossy();
                        if name.contains("map") || name.starts_with(stem) || name.starts_with("arnis_world") {
                            let _ = fs::remove_file(path);
                        }
                    }
                }
            }
        }
    }
}

pub fn cleanup_orphaned_staging_dirs() {
    #[cfg(target_os = "android")]
    {
        let download_dir = crate::world_utils::resolve_android_download_dir();
        let candidate_bases = [
            std::env::var_os("TMPDIR").map(PathBuf::from),
            Some(PathBuf::from("/data/data/com.louisdev.arnis/cache")),
            Some(PathBuf::from("/data/user/0/com.louisdev.arnis/cache")),
            dirs::cache_dir(),
            Some(download_dir.join(".arnis_staging")),
        ];
        for base in candidate_bases.into_iter().flatten() {
            if let Ok(entries) = fs::read_dir(&base) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let name = entry.file_name();
                        let name_str = name.to_string_lossy();
                        if name_str.starts_with("arnis_staging_") || name_str == ".arnis_staging" {
                            let _ = fs::remove_dir_all(&path);
                        }
                    }
                }
            }
        }
    }
}

fn append_mcworld_extension(dir: &std::path::Path) -> PathBuf {
    let mut name = dir.as_os_str().to_os_string();
    name.push(".mcworld");
    PathBuf::from(name)
}

fn is_arnis_staging_dir(dir: &std::path::Path) -> bool {
    if dir.join(STAGING_MARKER).is_file() {
        return true;
    }
    if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
        if name.contains("arnis_staging") || name.contains(".arnis_staging") {
            return true;
        }
    }
    fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

fn add_directory_to_zip<W: std::io::Write + std::io::Seek>(
    writer: &mut ZipWriter<W>,
    dir_path: &std::path::Path,
    zip_prefix: &str,
    options: SimpleFileOptions,
) -> Result<(), BedrockSaveError> {
    writer.add_directory(format!("{}/", zip_prefix), options)?;

    for entry in fs::read_dir(dir_path)? {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let zip_path = format!("{}/{}", zip_prefix, name);

        if path.is_file() {
            if name == "LOCK" {
                continue;
            }
            writer.start_file(&zip_path, options)?;
            let mut file = File::open(&path)?;
            std::io::copy(&mut file, writer)?;
        } else if path.is_dir() {
            add_directory_to_zip(writer, &path, &zip_path, options)?;
        }
    }

    Ok(())
}

fn column_heights(chunk: &ChunkToModify) -> [i16; 256] {
    let mut heights = [i16::MIN; 256];
    for (&section_y, section) in &chunk.sections {
        let base_y = i16::from(section_y) * 16;
        match section.storage {
            BlockStorage::Uniform(AIR) => continue,
            BlockStorage::Uniform(_) => {
                for h in &mut heights {
                    *h = (*h).max(base_y + 15);
                }
            }
            BlockStorage::Palette { .. } | BlockStorage::Direct(_) => {
                for x in 0..16usize {
                    for z in 0..16usize {
                        let col = x * 16 + z;
                        for y in (0..16i16).rev() {
                            if base_y + y <= heights[col] {
                                break;
                            }
                            let internal_idx = (y as usize) * 256 + z * 16 + x;
                            if section.get_block_at_index(internal_idx) != AIR {
                                heights[col] = base_y + y;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    heights
}

fn write_biome_storage(buffer: &mut Vec<u8>, ids: &[i32; 16]) {
    let mut palette: Vec<i32> = Vec::new();
    let mut indices = [0u8; 16];
    for (i, &id) in ids.iter().enumerate() {
        let idx = if let Some(pos) = palette.iter().position(|&p| p == id) {
            pos
        } else {
            palette.push(id);
            palette.len() - 1
        };
        indices[i] = idx as u8;
    }

    let bits = bedrock_bits_per_block(palette.len() as u32);
    buffer.push((bits << 1) | 1);

    if bits == 0 {
        buffer.extend_from_slice(&palette[0].to_le_bytes());
        return;
    }

    let blocks_per_word = 32 / u32::from(bits);
    let word_count = 16_u32.div_ceil(blocks_per_word);
    let mask = (1u32 << bits) - 1;
    let mut idx = 0usize;
    for _ in 0..word_count {
        let mut word = 0u32;
        for shift in (0..32).step_by(bits as usize) {
            if idx >= 16 {
                break;
            }
            word |= (u32::from(indices[idx]) & mask) << shift;
            idx += 1;
        }
        buffer.extend_from_slice(&word.to_le_bytes());
    }

    buffer.extend_from_slice(&(palette.len() as u32).to_le_bytes());
    for id in palette {
        buffer.extend_from_slice(&id.to_le_bytes());
    }
}

fn bedrock_biome_id(java: &str) -> i32 {
    match java {
        "ocean" => 0,
        "plains" => 1,
        "desert" => 2,
        "windswept_hills" => 3,
        "forest" => 4,
        "taiga" => 5,
        "swamp" => 6,
        "river" => 7,
        "frozen_ocean" => 10,
        "frozen_river" => 11,
        "snowy_plains" => 12,
        "snowy_mountains" => 13,
        "mushroom_fields" => 14,
        "beach" => 16,
        "jungle" => 21,
        "sparse_jungle" => 23,
        "deep_ocean" => 24,
        "stony_shore" => 25,
        "snowy_beach" => 26,
        "birch_forest" => 27,
        "dark_forest" => 29,
        "snowy_taiga" => 30,
        "old_growth_pine_taiga" => 32,
        "windswept_forest" => 34,
        "savanna" => 35,
        "savanna_plateau" => 36,
        "badlands" => 37,
        "wooded_badlands" => 38,
        "badlands_plateau" => 39,
        "warm_ocean" => 40,
        "lukewarm_ocean" => 41,
        "cold_ocean" => 42,
        "deep_warm_ocean" => 43,
        "deep_lukewarm_ocean" => 44,
        "deep_cold_ocean" => 45,
        "deep_frozen_ocean" => 46,
        "sunflower_plains" => 129,
        "windswept_gravelly_hills" => 131,
        "flower_forest" => 132,
        "ice_spikes" => 140,
        "old_growth_birch_forest" => 155,
        "old_growth_spruce_taiga" => 160,
        "windswept_savanna" => 163,
        "eroded_badlands" => 165,
        "bamboo_jungle" => 168,
        "meadow" => 177,
        "grove" => 178,
        "snowy_slopes" => 179,
        "jagged_peaks" => 180,
        "frozen_peaks" => 181,
        "stony_peaks" => 182,
        "cherry_grove" => 183,
        "pale_garden" => 184,
        _ => 1,
    }
}

fn write_block_storage(
    buffer: &mut impl IoWrite,
    palette: &[BedrockBlock],
    index: impl Fn(usize) -> u32,
) -> Result<(), BedrockSaveError> {
    let bits_per_block = bedrock_bits_per_block(palette.len() as u32);
    buffer.write_u8(bits_per_block << 1)?;
    write_packed_indices(buffer, bits_per_block, index)?;
    buffer.write_u32::<LittleEndian>(palette.len() as u32)?;
    for block in palette {
        let state = BedrockBlockState {
            name: block.name.clone(),
            states: block
                .states
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(v)))
                .collect(),
            version: BLOCK_STATE_VERSION,
        };
        let nbt_bytes = nbtx::to_le_bytes(&state).map_err(|e| {
            BedrockSaveError::Nbt(format!("block palette state ({}): {e}", state.name))
        })?;
        buffer.write_all(&nbt_bytes)?;
    }
    Ok(())
}

fn palette_slot(
    palette: &mut Vec<BedrockBlock>,
    palette_map: &mut StdHashMap<BedrockBlock, u16>,
    block: BedrockBlock,
) -> u16 {
    if let Some(&idx) = palette_map.get(&block) {
        return idx;
    }
    let idx = palette.len() as u16;
    palette_map.insert(block.clone(), idx);
    palette.push(block);
    idx
}

fn write_packed_indices(
    buffer: &mut impl IoWrite,
    bits_per_block: u8,
    index: impl Fn(usize) -> u32,
) -> std::io::Result<()> {
    let blocks_per_word = 32 / u32::from(bits_per_block);
    let mask = (1u32 << bits_per_block) - 1;
    let mut block_index = 0usize;
    let word_count = 4096_u32.div_ceil(blocks_per_word);
    for _ in 0..word_count {
        let mut word = 0u32;
        for block_in_word in 0..blocks_per_word {
            if block_index >= 4096 {
                break;
            }
            let shift = u32::from(bits_per_block) * block_in_word;
            let val = index(block_index) & mask;
            word |= val << shift;
            block_index += 1;
        }
        buffer.write_u32::<LittleEndian>(word)?;
    }
    Ok(())
}

#[inline]
fn bedrock_bits_per_block(palette_count: u32) -> u8 {
    const VALID_BITS: [u8; 8] = [1, 2, 3, 4, 5, 6, 8, 16];
    for &bits in &VALID_BITS {
        if palette_count <= (1u32 << bits) {
            return bits;
        }
    }
    16
}

fn build_chunk_key_bytes(
    chunk_pos: Vec2<i32>,
    dimension: Dimension,
    key_type: KeyTypeTag,
    y_index: Option<i8>,
) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(
        9 + if dimension != Dimension::Overworld {
            4
        } else {
            0
        } + 1,
    );
    buffer.extend_from_slice(&chunk_pos.x.to_le_bytes());
    buffer.extend_from_slice(&chunk_pos.y.to_le_bytes());

    if dimension != Dimension::Overworld {
        let dim_id = match dimension {
            Dimension::Overworld => 0i32,
            Dimension::Nether => 1i32,
            Dimension::End => 2i32,
        };
        buffer.extend_from_slice(&dim_id.to_le_bytes());
    }

    buffer.push(key_type.to_byte());
    if let Some(y) = y_index {
        buffer.push(y as u8);
    }

    buffer
}

fn strip_empty_lists(compound: &mut StdHashMap<String, Value>) {
    fn keep(value: &mut Value) -> bool {
        match value {
            Value::List(list) => {
                list.retain_mut(keep);
                !list.is_empty()
            }
            Value::Compound(nested) => {
                nested.retain(|_, v| keep(v));
                true
            }
            _ => true,
        }
    }
    compound.retain(|_, v| keep(v));
}

fn dedup_compound_list(values: &[Value]) -> Vec<Value> {
    let mut coord_index: StdHashMap<(i32, i32, i32), usize> = StdHashMap::new();
    let mut deduped: Vec<Value> = Vec::with_capacity(values.len());

    for value in values {
        if let Value::Compound(map) = value {
            if let Some(coords) = get_entity_coords(map) {
                if let Some(idx) = coord_index.get(&coords).copied() {
                    deduped[idx] = value.clone();
                    continue;
                } else {
                    coord_index.insert(coords, deduped.len());
                }
            }
        }
        deduped.push(value.clone());
    }

    deduped
}

fn get_entity_coords(entity: &StdHashMap<String, Value>) -> Option<(i32, i32, i32)> {
    if let Some(Value::List(pos)) = entity.get("Pos") {
        if pos.len() == 3 {
            if let (Some(x), Some(y), Some(z)) = (
                bedrock_entities::int(&pos[0]),
                bedrock_entities::int(&pos[1]),
                bedrock_entities::int(&pos[2]),
            ) {
                return Some((x, y, z));
            }
        }
    }

    let (Some(x), Some(y), Some(z)) = (
        entity.get("x").and_then(bedrock_entities::int),
        entity.get("y").and_then(bedrock_entities::int),
        entity.get("z").and_then(bedrock_entities::int),
    ) else {
        return None;
    };

    Some((x, y, z))
}

/// Level.dat structure for Bedrock Edition
#[derive(serde::Serialize)]
struct BedrockLevelDat {
    #[serde(rename = "StorageVersion")]
    storage_version: i32,
    #[serde(rename = "NetworkVersion")]
    network_version: i32,
    #[serde(rename = "WorldVersion")]
    world_version: i32,
    #[serde(rename = "InventoryVersion")]
    inventory_version: String,
    #[serde(rename = "lastOpenedWithVersion")]
    last_opened_with_version: Vec<i32>,
    #[serde(rename = "MinimumCompatibleClientVersion")]
    minimum_compatible_client_version: Vec<i32>,

    #[serde(rename = "LevelName")]
    level_name: String,
    #[serde(rename = "RandomSeed")]
    random_seed: i64,

    #[serde(rename = "SpawnX")]
    spawn_x: i32,
    #[serde(rename = "SpawnY")]
    spawn_y: i32,
    #[serde(rename = "SpawnZ")]
    spawn_z: i32,

    #[serde(rename = "Generator")]
    generator: i32,
    #[serde(rename = "FlatWorldLayers")]
    flat_world_layers: String,
    #[serde(rename = "spawnMobs")]
    spawn_mobs: bool,

    #[serde(rename = "GameType")]
    game_type: i32,
    #[serde(rename = "Difficulty")]
    difficulty: i32,
    #[serde(rename = "ForceGameType")]
    force_game_type: bool,

    #[serde(rename = "LastPlayed")]
    last_played: i64,
    #[serde(rename = "Time")]
    time: i64,
    #[serde(rename = "currentTick")]
    current_tick: i64,

    #[serde(rename = "commandsEnabled")]
    commands_enabled: bool,
    #[serde(rename = "cheatsEnabled")]
    cheats_enabled: bool,
    #[serde(rename = "commandblocksenabled")]
    command_blocks_enabled: bool,
    #[serde(rename = "commandblockoutput")]
    command_block_output: bool,

    #[serde(rename = "MultiplayerGame")]
    multiplayer_game: bool,
    #[serde(rename = "MultiplayerGameIntent")]
    multiplayer_game_intent: bool,
    #[serde(rename = "LANBroadcast")]
    lan_broadcast: bool,
    #[serde(rename = "LANBroadcastIntent")]
    lan_broadcast_intent: bool,
    #[serde(rename = "XBLBroadcastIntent")]
    xbl_broadcast_intent: i32,
    #[serde(rename = "PlatformBroadcastIntent")]
    platform_broadcast_intent: i32,
    #[serde(rename = "Platform")]
    platform: i32,

    #[serde(rename = "dodaylightcycle")]
    do_daylight_cycle: bool,
    #[serde(rename = "doweathercycle")]
    do_weather_cycle: bool,
    #[serde(rename = "domobspawning")]
    do_mob_spawning: bool,
    #[serde(rename = "domobloot")]
    do_mob_loot: bool,
    #[serde(rename = "dotiledrops")]
    do_tile_drops: bool,
    #[serde(rename = "doentitydrops")]
    do_entity_drops: bool,
    #[serde(rename = "dofiretick")]
    do_fire_tick: bool,
    #[serde(rename = "mobgriefing")]
    mob_griefing: bool,
    #[serde(rename = "naturalregeneration")]
    natural_regeneration: bool,
    #[serde(rename = "pvp")]
    pvp: bool,
    #[serde(rename = "keepinventory")]
    keep_inventory: bool,
    #[serde(rename = "sendcommandfeedback")]
    send_command_feedback: bool,
    #[serde(rename = "showcoordinates")]
    show_coordinates: bool,
    #[serde(rename = "showdeathmessages")]
    show_death_messages: bool,
    #[serde(rename = "tntexplodes")]
    tnt_explodes: bool,
    #[serde(rename = "respawnblocksexplode")]
    respawn_blocks_explode: bool,
    #[serde(rename = "projectilescanbreakblocks")]
    projectiles_can_break_blocks: bool,

    #[serde(rename = "drowningdamage")]
    drowning_damage: bool,
    #[serde(rename = "falldamage")]
    fall_damage: bool,
    #[serde(rename = "firedamage")]
    fire_damage: bool,
    #[serde(rename = "freezedamage")]
    freeze_damage: bool,

    #[serde(rename = "rainLevel")]
    rain_level: f32,
    #[serde(rename = "rainTime")]
    rain_time: i32,
    #[serde(rename = "lightningLevel")]
    lightning_level: f32,
    #[serde(rename = "lightningTime")]
    lightning_time: i32,

    #[serde(rename = "NetherScale")]
    nether_scale: i32,
    #[serde(rename = "spawnradius")]
    spawn_radius: i32,
    #[serde(rename = "spawnRadius")]
    spawn_radius_camel: i32,
    #[serde(rename = "SpawnRadius")]
    spawn_radius_pascal: i32,
    #[serde(rename = "randomtickspeed")]
    random_tick_speed: i32,
    #[serde(rename = "functioncommandlimit")]
    function_command_limit: i32,
    #[serde(rename = "maxcommandchainlength")]
    max_command_chain_length: i32,
    #[serde(rename = "serverChunkTickRange")]
    server_chunk_tick_range: i32,
    #[serde(rename = "limitedWorldDepth")]
    limited_world_depth: i32,
    #[serde(rename = "limitedWorldWidth")]
    limited_world_width: i32,
    #[serde(rename = "LimitedWorldOriginX")]
    limited_world_origin_x: i32,
    #[serde(rename = "LimitedWorldOriginY")]
    limited_world_origin_y: i32,
    #[serde(rename = "LimitedWorldOriginZ")]
    limited_world_origin_z: i32,
    #[serde(rename = "worldStartCount")]
    world_start_count: i64,

    #[serde(rename = "bonusChestEnabled")]
    bonus_chest_enabled: bool,
    #[serde(rename = "bonusChestSpawned")]
    bonus_chest_spawned: bool,
    #[serde(rename = "hasBeenLoadedInCreative")]
    has_been_loaded_in_creative: bool,
    #[serde(rename = "hasLockedBehaviorPack")]
    has_locked_behavior_pack: bool,
    #[serde(rename = "hasLockedResourcePack")]
    has_locked_resource_pack: bool,
    #[serde(rename = "immutableWorld")]
    immutable_world: bool,
    #[serde(rename = "isFromLockedTemplate")]
    is_from_locked_template: bool,
    #[serde(rename = "isFromWorldTemplate")]
    is_from_world_template: bool,
    #[serde(rename = "isSingleUseWorld")]
    is_single_use_world: bool,
    #[serde(rename = "isWorldTemplateOptionLocked")]
    is_world_template_option_locked: bool,
    #[serde(rename = "texturePacksRequired")]
    texture_packs_required: bool,
    #[serde(rename = "useMsaGamertagsOnly")]
    use_msa_gamertags_only: bool,
    #[serde(rename = "CenterMapsToOrigin")]
    center_maps_to_origin: bool,
    #[serde(rename = "ConfirmedPlatformLockedContent")]
    confirmed_platform_locked_content: bool,
    #[serde(rename = "educationFeaturesEnabled")]
    education_features_enabled: bool,
    #[serde(rename = "startWithMapEnabled")]
    start_with_map_enabled: bool,
    #[serde(rename = "requiresCopiedPackRemovalCheck")]
    requires_copied_pack_removal_check: bool,
    #[serde(rename = "SpawnV1Villagers")]
    spawn_v1_villagers: bool,
    #[serde(rename = "IsHardcore")]
    is_hardcore: bool,
    #[serde(rename = "isCreatedInEditor")]
    is_created_in_editor: bool,
    #[serde(rename = "isExportedFromEditor")]
    is_exported_from_editor: bool,
    #[serde(rename = "isRandomSeedAllowed")]
    is_random_seed_allowed: bool,
    #[serde(rename = "HasUncompleteWorldFileOnDisk")]
    has_uncomplete_world_file_on_disk: bool,
    #[serde(rename = "PlayerHasDied")]
    player_has_died: bool,
    #[serde(rename = "doinsomnia")]
    do_insomnia: bool,
    #[serde(rename = "doimmediaterespawn")]
    do_immediate_respawn: bool,
    #[serde(rename = "dolimitedcrafting")]
    do_limited_crafting: bool,
    #[serde(rename = "recipesunlock")]
    recipes_unlock: bool,
    #[serde(rename = "showtags")]
    show_tags: bool,
    #[serde(rename = "showrecipemessages")]
    show_recipe_messages: bool,
    #[serde(rename = "showbordereffect")]
    show_border_effect: bool,
    #[serde(rename = "showdaysplayed")]
    show_days_played: bool,
    #[serde(rename = "locatorbar")]
    locator_bar: bool,
    #[serde(rename = "tntexplosiondropdecay")]
    tnt_explosion_drop_decay: bool,
    #[serde(rename = "experiments")]
    experiments: BedrockExperiments,

    #[serde(rename = "editorWorldType")]
    editor_world_type: i32,
    #[serde(rename = "eduOffer")]
    edu_offer: i32,
    #[serde(rename = "BiomeOverride")]
    biome_override: String,
    #[serde(rename = "prid")]
    prid: String,
    #[serde(rename = "playerssleepingpercentage")]
    players_sleeping_percentage: i32,
    #[serde(rename = "permissionsLevel")]
    permissions_level: i32,
    #[serde(rename = "playerPermissionsLevel")]
    player_permissions_level: i32,
    #[serde(rename = "daylightCycle")]
    daylight_cycle: i32,
}

#[derive(Serialize)]
struct BedrockExperiments {
    experiments_ever_used: bool,
    saved_with_toggled_experiments: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bedrockrs_level::level::db_interface::bedrock_key::ChunkKey;
    use serde_json::Value;
    use zip::ZipArchive;

    fn canonical_key(chunk_key: ChunkKey) -> Vec<u8> {
        use bedrockrs_level::level::db_interface::db::LevelDBKey;
        use std::io::Cursor;
        let mut key_bytes: Vec<u8> = vec![0; chunk_key.estimate_size()];
        let mut buff: Cursor<&mut [u8]> = Cursor::new(&mut key_bytes);
        chunk_key.write_key(&mut buff);
        key_bytes
    }

    fn test_writer(output_path: PathBuf) -> BedrockWriter {
        BedrockWriter::new(
            output_path,
            "Arnis World: Test".to_string(),
            None,
            None,
            false,
            "local".to_string(),
            1.0,
            crate::args::GameMode::Creative,
            0,
            false,
        )
    }

    #[test]
    fn mcworld_path_keeps_dots_in_area_name() {
        let output = PathBuf::from("Arnis St. Louis.mcworld");
        let writer = test_writer(output.clone());
        assert_eq!(writer.mcworld_path, output);
        assert_eq!(writer.output_dir, PathBuf::from("Arnis St. Louis"));
    }

    #[test]
    fn prepare_output_dir_keeps_foreign_directory() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join("existing_world");
        fs::create_dir_all(&path).expect("mkdir");
        fs::write(path.join("notes.txt"), b"keep me").expect("write notes");

        let mut writer = test_writer(path);
        assert!(matches!(
            writer.prepare_output_dir(),
            Err(BedrockSaveError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists
        ));
    }

    #[test]
    fn prepare_output_dir_keeps_existing_bedrock_world() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join("existing_world");
        fs::create_dir_all(path.join("db")).expect("mkdir db");
        fs::write(path.join("levelname.txt"), b"World").expect("write levelname");

        let mut writer = test_writer(path);
        assert!(matches!(
            writer.prepare_output_dir(),
            Err(BedrockSaveError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists
        ));
    }

    #[test]
    fn prepare_output_dir_wipes_previous_staging_dir() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join("staging_world");
        fs::create_dir_all(path.join("db")).expect("mkdir db");
        fs::write(path.join(STAGING_MARKER), b"").expect("write marker");
        fs::write(path.join("leftover.txt"), b"stale").expect("write leftover");

        let mut writer = test_writer(path.clone());
        writer.prepare_output_dir().expect("prepare dir");
        assert!(path.join(STAGING_MARKER).is_file());
        assert!(!path.join("leftover.txt").exists());
    }

    #[test]
    fn prepare_output_dir_accepts_empty_directory() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join("empty_world");
        fs::create_dir_all(&path).expect("mkdir");

        let mut writer = test_writer(path.clone());
        writer.prepare_output_dir().expect("prepare dir");
        assert!(path.join(STAGING_MARKER).is_file());
    }

    #[test]
    fn build_chunk_key_bytes_matches_canonical_version() {
        let pos = Vec2::new(12, -34);
        let canonical = canonical_key(ChunkKey::new(pos, Dimension::Overworld, KeyTypeTag::Version));
        let fast = build_chunk_key_bytes(pos, Dimension::Overworld, KeyTypeTag::Version, None);
        assert_eq!(fast, canonical);
    }

    #[test]
    fn build_chunk_key_bytes_matches_canonical_data3d() {
        let pos = Vec2::new(-5, 42);
        let canonical = canonical_key(ChunkKey::new(pos, Dimension::Overworld, KeyTypeTag::Data3D));
        let fast = build_chunk_key_bytes(pos, Dimension::Overworld, KeyTypeTag::Data3D, None);
        assert_eq!(fast, canonical);
    }

    #[test]
    fn build_chunk_key_bytes_matches_canonical_subchunk() {
        let pos = Vec2::new(7, 8);
        let canonical = canonical_key(ChunkKey::new_subchunk(pos, Dimension::Overworld, 4));
        let fast = build_chunk_key_bytes(
            pos,
            Dimension::Overworld,
            KeyTypeTag::SubChunkPrefix,
            Some(4),
        );
        assert_eq!(fast, canonical);
    }

    #[test]
    fn build_chunk_key_bytes_matches_canonical_negative_y() {
        let pos = Vec2::new(-1, -1);
        let canonical = canonical_key(ChunkKey::new_subchunk(pos, Dimension::Overworld, -4));
        let fast = build_chunk_key_bytes(
            pos,
            Dimension::Overworld,
            KeyTypeTag::SubChunkPrefix,
            Some(-4),
        );
        assert_eq!(fast, canonical);
    }

    #[test]
    fn writes_mcworld_package_with_metadata() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let output_dir = temp_dir.path().join("bedrock_world");
        let archive_path = temp_dir.path().join("bedrock_world.mcworld");
        let world = WorldToModify::default();
        let xzbbox = XZBBox::rect_from_xz_lengths(15.0, 15.0).unwrap();
        let llbbox = LLBBox::new(0.0, 0.0, 1.0, 1.0).unwrap();

        BedrockWriter::new(
            output_dir,
            "archive-world".to_string(),
            None,
            None,
            false,
            "local".to_string(),
            1.0,
            crate::args::GameMode::Creative,
            6000,
            false,
        )
        .write_world(&world, &xzbbox, &llbbox)
        .expect("write_world");

        assert!(archive_path.is_file(), "missing mcworld package");

        let zip_file = File::open(&archive_path).expect("open mcworld");
        let mut archive = ZipArchive::new(zip_file).expect("read zip");

        let entries: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        assert!(entries.contains(&"levelname.txt".to_string()), "missing levelname.txt: {entries:?}");
        assert!(entries.contains(&"level.dat".to_string()), "missing level.dat: {entries:?}");
        assert!(entries.contains(&"world_icon.jpeg".to_string()), "missing world_icon.jpeg: {entries:?}");
        assert!(entries.contains(&"metadata.json".to_string()), "missing metadata.json: {entries:?}");

        let mut metadata_file = archive.by_name("metadata.json").expect("open metadata.json");
        let mut metadata_str = String::new();
        std::io::Read::read_to_string(&mut metadata_file, &mut metadata_str).expect("read metadata");
        let meta: Value = serde_json::from_str(&metadata_str).expect("parse metadata");

        assert_eq!(meta["format"], "bedrock-mcworld");
        assert_eq!(meta["world"]["projection"], "local");
        assert_eq!(meta["world"]["scale"], 1.0);
    }

    #[test]
    fn writes_mcworld_with_custom_spawn_point() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let output_dir = temp_dir.path().join("bedrock_world_spawn");
        let archive_path = temp_dir.path().join("bedrock_world_spawn.mcworld");
        let mut world = WorldToModify::default();
        let xzbbox = XZBBox::rect_from_xz_lengths(64.0, 64.0).unwrap();
        let llbbox = LLBBox::new(0.0, 0.0, 1.0, 1.0).unwrap();

        let custom_spawn = (42, 17);
        world.set_block_if_absent(custom_spawn.0, 64, custom_spawn.1, crate::block_definitions::GRASS_BLOCK);

        BedrockWriter::new(
            output_dir,
            "spawn-world".to_string(),
            Some(custom_spawn),
            None,
            false,
            "local".to_string(),
            1.0,
            crate::args::GameMode::Creative,
            6000,
            false,
        )
        .write_world(&world, &xzbbox, &llbbox)
        .expect("write_world");

        let zip_file = File::open(&archive_path).expect("open mcworld");
        let mut archive = ZipArchive::new(zip_file).expect("read zip");
        let mut level_dat = archive.by_name("level.dat").expect("open level.dat");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut level_dat, &mut bytes).expect("read level.dat");

        assert!(bytes.len() > 8);
        let nbt_data = &bytes[8..];
        let val: nbtx::Value = nbtx::from_le_bytes(nbt_data).expect("parse NBT");
        let nbt_map = val.as_compound().expect("NBT root compound");

        assert_eq!(nbt_map.get("SpawnX").and_then(|v| v.as_i32()), Some(custom_spawn.0));
        assert_eq!(nbt_map.get("SpawnZ").and_then(|v| v.as_i32()), Some(custom_spawn.1));
    }

    #[test]
    fn mob_spawning_sets_the_game_rules_and_the_difficulty_monsters_need() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let output_dir = temp_dir.path().join("bedrock_world_peaceful");
        let archive_path = temp_dir.path().join("bedrock_world_peaceful.mcworld");
        let world = WorldToModify::default();
        let xzbbox = XZBBox::rect_from_xz_lengths(15.0, 15.0).unwrap();
        let llbbox = LLBBox::new(0.0, 0.0, 1.0, 1.0).unwrap();

        BedrockWriter::new(
            output_dir,
            "peaceful-world".to_string(),
            None,
            None,
            false,
            "local".to_string(),
            1.0,
            crate::args::GameMode::Creative,
            6000,
            false,
        )
        .with_mob_spawning(crate::args::MobSpawning::Off)
        .write_world(&world, &xzbbox, &llbbox)
        .expect("write_world");

        let zip_file = File::open(&archive_path).expect("open mcworld");
        let mut archive = ZipArchive::new(zip_file).expect("read zip");
        let mut level_dat = archive.by_name("level.dat").expect("open level.dat");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut level_dat, &mut bytes).expect("read level.dat");

        let val: nbtx::Value = nbtx::from_le_bytes(&bytes[8..]).expect("parse NBT");
        let map = val.as_compound().expect("NBT root compound");
        assert_eq!(map.get("Difficulty").and_then(|v| v.as_i32()), Some(0));
        assert_eq!(map.get("domobspawning").and_then(|v| v.as_u8()), Some(0));
        assert_eq!(map.get("spawnMobs").and_then(|v| v.as_u8()), Some(0));
    }

    #[test]
    fn writes_mcworld_with_tall_behavior_pack_when_extended() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let output_dir = temp_dir.path().join("bedrock_world_tall");
        let archive_path = temp_dir.path().join("bedrock_world_tall.mcworld");
        let world = WorldToModify::default();
        let xzbbox = XZBBox::rect_from_xz_lengths(15.0, 15.0).unwrap();
        let llbbox = LLBBox::new(0.0, 0.0, 1.0, 1.0).unwrap();

        BedrockWriter::new(
            output_dir,
            "tall-world".to_string(),
            None,
            None,
            true,
            "local".to_string(),
            1.0,
            crate::args::GameMode::Creative,
            6000,
            false,
        )
        .write_world(&world, &xzbbox, &llbbox)
        .expect("write_world");

        let zip_file = File::open(&archive_path).expect("open mcworld");
        let mut archive = ZipArchive::new(zip_file).expect("read zip");

        let entries: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        assert!(
            entries.contains(&"world_behavior_packs.json".to_string()),
            "missing world_behavior_packs.json: {entries:?}"
        );
        assert!(
            entries.contains(&"behavior_packs/arnis_tall/manifest.json".to_string()),
            "missing BP manifest: {entries:?}"
        );
        assert!(
            entries.contains(&"behavior_packs/arnis_tall/dimensions/overworld.json".to_string()),
            "missing BP overworld.json: {entries:?}"
        );
    }

    #[test]
    fn writes_the_vehicle_packs_beside_the_tall_pack_and_lists_both() {
        use crate::bedrock_addon::{Addon, Pack};
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let output_dir = temp_dir.path().join("bedrock_world_vehicles");
        let archive_path = temp_dir.path().join("bedrock_world_vehicles.mcworld");
        let world = WorldToModify::default();
        let xzbbox = XZBBox::rect_from_xz_lengths(15.0, 15.0).unwrap();
        let llbbox = LLBBox::new(0.0, 0.0, 1.0, 1.0).unwrap();
        let pack = |id: &str, files: &[&str]| Pack {
            id: id.to_string(),
            folder: "arnis_vehicles",
            files: files
                .iter()
                .map(|f| (f.to_string(), b"{}".to_vec()))
                .collect(),
        };
        let addon = Addon {
            behavior: pack("bp-id", &["manifest.json", "scripts/main.js"]),
            resources: pack("rp-id", &["manifest.json", "models/entity/car.geo.json"]),
        };

        BedrockWriter::new(
            output_dir,
            "vehicle-world".to_string(),
            None,
            None,
            true,
            "local".to_string(),
            1.0,
            crate::args::GameMode::Creative,
            6000,
            false,
        )
        .with_addon(Some(addon))
        .write_world(&world, &xzbbox, &llbbox)
        .expect("write_world");

        let zip_file = File::open(&archive_path).expect("open mcworld");
        let mut archive = ZipArchive::new(zip_file).expect("read zip");

        let read = |name: &str| -> serde_json::Value {
            let mut f = archive.by_name(name).expect(name);
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut buf).unwrap();
            serde_json::from_slice(&buf).unwrap()
        };

        let behavior = read("world_behavior_packs.json");
        let ids: Vec<&str> = behavior
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["pack_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a7f3b2e0-8c4d-4e92-9b1a-3d7f5c8e4a61", "bp-id"]);
        assert_eq!(read("world_resource_packs.json")[0]["pack_id"], "rp-id");
    }

    #[test]
    fn parallel_chunk_encode_writes_expected_records() {
        let mut world = WorldToModify::default();
        let chunk_coords = [(0i32, 0i32), (1, 0), (0, 1), (33, 2)];
        for &(cx, cz) in &chunk_coords {
            for y in [70i32, 5, -20] {
                world.set_block_if_absent(
                    cx * 16 + 2,
                    y,
                    cz * 16 + 3,
                    crate::block_definitions::STONE,
                );
            }
        }

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut writer = BedrockWriter::new(
            temp_dir.path().join("world"),
            "Parallel Test".to_string(),
            None,
            None,
            false,
            "local".to_string(),
            1.0,
            crate::args::GameMode::Creative,
            6000,
            false,
        );
        writer.prepare_output_dir().expect("prepare dir");
        writer.write_chunks_to_db(&world).expect("write chunks");

        let mut opts = bedrock_db_options();
        opts.create_if_missing = false;
        let mut db =
            DB::open(writer.output_dir.join("db").into_boxed_path(), opts).expect("reopen db");

        for &(cx, cz) in &chunk_coords {
            let pos = Vec2::new(cx, cz);
            let version =
                build_chunk_key_bytes(pos, Dimension::Overworld, KeyTypeTag::Version, None);
            assert_eq!(
                db.get(&version).as_deref(),
                Some(&[42u8][..]),
                "version record missing for chunk ({cx}, {cz})"
            );

            let data3d = build_chunk_key_bytes(pos, Dimension::Overworld, KeyTypeTag::Data3D, None);
            assert!(
                db.get(&data3d).is_some(),
                "Data3D record missing for chunk ({cx}, {cz})"
            );

            for section_y in [4i8, 0, -2] {
                let subchunk = build_chunk_key_bytes(
                    pos,
                    Dimension::Overworld,
                    KeyTypeTag::SubChunkPrefix,
                    Some(section_y),
                );
                assert!(
                    db.get(&subchunk).is_some_and(|b| !b.is_empty()),
                    "subchunk {section_y} missing for chunk ({cx}, {cz})"
                );
            }
        }
    }

    #[test]
    fn monotonic_env_clock_never_goes_backwards() {
        let env = MonotonicDiskEnv::new();
        let mut last = env.micros();
        assert!(last > 0);
        for _ in 0..1000 {
            let now = env.micros();
            assert!(now >= last);
            last = now;
        }
    }
}
