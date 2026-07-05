use crate::backend::VersionInfo;
use crate::backend::backend_type::BackendType;
use crate::backend::options::BackendOptions;
use crate::backend::platform_target::PlatformTarget;
use crate::backend::runtime_path_for_install_path;
use crate::backend::static_helpers::{
    clean_binary_name, eval_checksum_expr, fetch_checksum_from_file, fetch_checksum_from_shasums,
    get_filename_from_url, rename_executable_in_dir, shasums_has_entries, template_string,
    template_string_for_target, verify_artifact,
};
use crate::backend::version_list;
use crate::backend::{Backend, BackendInstallPlan, InstallPhase, StoreInstallMode};
use crate::cli::args::BackendArg;
use crate::config::Config;
use crate::config::Settings;
use crate::http::HTTP;
use crate::install_context::InstallContext;
use crate::lockfile::PlatformInfo;
use crate::runtime_symlinks::is_runtime_symlink;
use crate::store::{
    ProvenanceRecord, StagedStoreInstallPublishInput, StoreObjectPublishInput,
    StoreRealisationMetadata, StoreRoot, StoreTxn, StoreTxnInput, StoreTxnState,
    publish_staged_store_install,
};
use crate::toolset::ToolRequest;
use crate::toolset::ToolVersion;
use crate::toolset::ToolVersionOptions;
use crate::toolset::install_state;
use crate::ui::progress_report::SingleReport;
use crate::{dirs, file, hash};
use async_trait::async_trait;
use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

// Constants
const HTTP_TARBALLS_DIR: &str = "http-tarballs";
const METADATA_FILE: &str = "metadata.json";

/// Metadata stored alongside cached extractions
#[derive(Debug, Serialize, Deserialize)]
struct CacheMetadata {
    url: String,
    checksum: Option<String>,
    size: u64,
    extracted_at: u64,
    platform: String,
}

/// Describes what type of content was extracted to cache
#[derive(Debug, Clone)]
enum ExtractionType {
    /// A single raw file (not an archive) with its filename
    RawFile { filename: String },
    /// An archive (tarball, zip, etc.) that was extracted
    Archive,
}

/// Information about a downloaded file's format
struct FileInfo {
    /// Path with effective extension (after applying format option)
    effective_path: PathBuf,
    /// File extension
    extension: String,
    /// Detected archive format
    format: file::ExtractionFormat,
    /// Whether this is a compressed single binary (not a tar archive)
    is_compressed_binary: bool,
}

struct CachePlan {
    key: String,
    file_info: FileInfo,
    strip_components: usize,
}

struct PreparedHttpArtifact {
    url: String,
    filename: String,
    file_path: PathBuf,
    cache_plan: CachePlan,
    extraction_type: ExtractionType,
    lockfile_enabled: bool,
    has_lockfile_checksum: bool,
    artifact_file_available: bool,
}

impl FileInfo {
    /// Analyze a file path and options to determine format information
    fn new(file_path: &Path, opts: &HttpOptions<'_>) -> Self {
        // Apply format config to determine effective extension
        let effective_path = if let Some(added_ext) = opts.format() {
            let mut path = file_path.to_path_buf();
            let current_ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let new_ext = if current_ext.is_empty() {
                added_ext
            } else {
                format!("{}.{}", current_ext, added_ext)
            };
            path.set_extension(new_ext);
            path
        } else {
            file_path.to_path_buf()
        };

        let file_name = effective_path.file_name().unwrap().to_string_lossy();
        let format = file::ExtractionFormat::from_file_name(&file_name);

        let extension = format.extension().unwrap_or_else(|| {
            effective_path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        });

        let is_compressed_binary = !format.is_archive() && format != file::ExtractionFormat::Raw;

        Self {
            effective_path,
            extension,
            format,
            is_compressed_binary,
        }
    }

    /// Get the filename portion of the effective path
    fn file_name(&self) -> String {
        self.effective_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    /// Get the decompressed name (for compressed binaries)
    fn decompressed_name(&self) -> String {
        self.file_name()
            .trim_end_matches(&format!(".{}", self.extension))
            .to_string()
    }
}

#[derive(Debug)]
pub struct HttpBackend {
    ba: Arc<BackendArg>,
}

#[derive(Debug, Clone, Copy)]
struct HttpOptions<'a> {
    values: BackendOptions<'a>,
}

impl<'a> HttpOptions<'a> {
    fn new(raw: &'a ToolVersionOptions) -> Self {
        Self {
            values: BackendOptions::new(raw),
        }
    }

    fn raw(&self) -> &'a ToolVersionOptions {
        self.values.raw()
    }

    fn url(&self) -> Option<String> {
        self.values.platform_string("url")
    }

    fn checksum(&self) -> Option<String> {
        self.values.platform_string("checksum")
    }

    fn format(&self) -> Option<String> {
        self.values.platform_string("format")
    }

    fn strip_components(&self) -> Option<String> {
        self.values.platform_string("strip_components")
    }

    fn bin(&self) -> Option<String> {
        self.values.platform_string("bin")
    }

    fn rename_exe(&self) -> Option<String> {
        self.values.platform_string("rename_exe")
    }

    fn bin_path(&self) -> Option<String> {
        self.values.platform_string("bin_path")
    }

    fn checksum_expr(&self) -> Option<&'a str> {
        self.values.str("checksum_expr")
    }

    // Target-aware accessors for cross-platform `mise lock`. These resolve
    // `platforms.<key>.<opt>` for an arbitrary target rather than the host.
    fn url_for_target(&self, target: &PlatformTarget) -> Option<String> {
        self.values.platform_string_for_target("url", target)
    }

    fn checksum_for_target(&self, target: &PlatformTarget) -> Option<String> {
        self.values.platform_string_for_target("checksum", target)
    }

    fn checksum_url_for_target(&self, target: &PlatformTarget) -> Option<String> {
        self.values
            .platform_string_for_target("checksum_url", target)
    }

    fn format_for_target(&self, target: &PlatformTarget) -> Option<String> {
        self.values.platform_string_for_target("format", target)
    }

    fn strip_components_for_target(&self, target: &PlatformTarget) -> Option<String> {
        self.values
            .platform_string_for_target("strip_components", target)
    }

    fn rename_exe_for_target(&self, target: &PlatformTarget) -> Option<String> {
        self.values.platform_string_for_target("rename_exe", target)
    }

    fn version_list_url(&self) -> Option<&'a str> {
        self.values.str("version_list_url")
    }

    fn version_regex(&self) -> Option<&'a str> {
        self.values.str("version_regex")
    }

    fn version_json_path(&self) -> Option<&'a str> {
        self.values.str("version_json_path")
    }

    fn version_expr(&self) -> Option<&'a str> {
        self.values.str("version_expr")
    }

    fn url_platforms(&self) -> Vec<String> {
        self.values.available_platforms_with_key("url")
    }
}

impl HttpBackend {
    pub fn from_arg(ba: BackendArg) -> Self {
        Self { ba: Arc::new(ba) }
    }

    // -------------------------------------------------------------------------
    // Cache path helpers
    // -------------------------------------------------------------------------

    /// Get the http-tarballs directory in DATA (survives `mise cache clear`)
    fn tarballs_dir() -> PathBuf {
        dirs::DATA.join(HTTP_TARBALLS_DIR)
    }

    /// Get the path to a specific cache entry
    fn cache_path(&self, cache_key: &str) -> PathBuf {
        Self::tarballs_dir().join(cache_key)
    }

    /// Get the path to the metadata file for a cache entry
    fn metadata_path(&self, cache_key: &str) -> PathBuf {
        self.cache_path(cache_key).join(METADATA_FILE)
    }

    /// Check if a cache entry exists and is valid
    fn is_cached(&self, cache_key: &str) -> bool {
        self.cache_path(cache_key).exists() && self.metadata_path(cache_key).exists()
    }

    fn read_cache_metadata(&self, cache_key: &str) -> Result<CacheMetadata> {
        Ok(serde_json::from_str(&file::read_to_string(
            self.metadata_path(cache_key),
        )?)?)
    }

    fn find_offline_cache_plan(
        &self,
        url: &str,
        file_path: &Path,
        opts: &HttpOptions<'_>,
    ) -> Result<Option<(CachePlan, ExtractionType)>> {
        let Some(expected_checksum) = opts.checksum() else {
            return Ok(None);
        };
        let tarballs_dir = Self::tarballs_dir();
        if !tarballs_dir.is_dir() {
            return Ok(None);
        }

        let mut entries =
            fs::read_dir(tarballs_dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let cache_key = entry.file_name().to_string_lossy().to_string();
            let Ok(metadata) = self.read_cache_metadata(&cache_key) else {
                continue;
            };
            if metadata.url != url
                || metadata.checksum.as_deref() != Some(expected_checksum.as_str())
            {
                continue;
            }
            let file_info = FileInfo::new(file_path, opts);
            let strip_components = opts
                .strip_components()
                .map(|s| {
                    s.parse::<usize>()
                        .map_err(|_| eyre::eyre!("Invalid strip_components value: {s}"))
                })
                .transpose()?
                .unwrap_or(0);
            let cache_plan = CachePlan {
                key: cache_key,
                file_info,
                strip_components,
            };
            let extraction_type =
                self.extraction_type_from_cache(&cache_plan.key, &cache_plan.file_info);
            return Ok(Some((cache_plan, extraction_type)));
        }
        Ok(None)
    }

    // -------------------------------------------------------------------------
    // Cache key generation
    // -------------------------------------------------------------------------

    /// Generate a cache key based on file content and extraction options
    fn cache_key(
        &self,
        file_path: &Path,
        opts: &HttpOptions<'_>,
        strip_components: usize,
    ) -> Result<String> {
        let checksum = hash::file_hash_blake3(file_path, None)?;

        // Include extraction options that affect output structure
        // Note: bin_path is NOT included - handled at symlink time for deduplication
        let mut parts = vec![checksum];

        if let Some(strip) = opts.strip_components() {
            parts.push(format!("strip_{strip}"));
        } else if strip_components > 0 {
            parts.push(format!("strip_{strip_components}"));
        }

        // Include rename_exe in cache key since it modifies the extracted content
        if let Some(rename) = opts.rename_exe() {
            parts.push(format!("rename_{rename}"));
            // When rename_exe is used, bin_path affects where the rename happens,
            // so different bin_path values result in different cached content
            if let Some(bin_path) = opts.bin_path() {
                parts.push(format!("binpath_{bin_path}"));
            }
        }

        let key = parts.join("_");
        debug!("Cache key: {}", key);
        Ok(key)
    }

    fn cache_plan(&self, file_path: &Path, opts: &HttpOptions<'_>) -> Result<CachePlan> {
        let file_info = FileInfo::new(file_path, opts);
        let strip_components = self.effective_strip_components(file_path, &file_info, opts)?;
        let key = self.cache_key(file_path, opts, strip_components)?;

        Ok(CachePlan {
            key,
            file_info,
            strip_components,
        })
    }

    fn effective_strip_components(
        &self,
        file_path: &Path,
        file_info: &FileInfo,
        opts: &HttpOptions<'_>,
    ) -> Result<usize> {
        let mut strip_components: Option<usize> = opts
            .strip_components()
            .map(|s| {
                s.parse::<usize>()
                    .map_err(|_| eyre::eyre!("Invalid strip_components value: {s}"))
            })
            .transpose()?;

        // Auto-detect strip_components=1 for single-directory archives
        if strip_components.is_none()
            && !file_info.is_compressed_binary
            && file_info.format != file::ExtractionFormat::Raw
            && opts.bin_path().is_none()
            && file::should_strip_components(file_path, file_info.format).unwrap_or(false)
        {
            debug!("Auto-detected single directory archive, using strip_components=1");
            strip_components = Some(1);
        }

        Ok(strip_components.unwrap_or(0))
    }

    // -------------------------------------------------------------------------
    // Filename determination
    // -------------------------------------------------------------------------

    /// Determine the destination filename for a raw file or compressed binary
    fn dest_filename(
        &self,
        file_path: &Path,
        file_info: &FileInfo,
        opts: &HttpOptions<'_>,
    ) -> String {
        // Check for explicit bin name first
        if let Some(bin_name) = opts.bin() {
            return bin_name;
        }

        // Auto-clean the binary name
        let raw_name = if file_info.is_compressed_binary {
            file_info.decompressed_name()
        } else {
            file_path.file_name().unwrap().to_string_lossy().to_string()
        };

        clean_binary_name(&raw_name, Some(&self.ba.tool_name))
    }

    // -------------------------------------------------------------------------
    // Extraction type detection
    // -------------------------------------------------------------------------

    /// Detect extraction type from an existing cache directory
    /// This handles the case where a cache hit occurs but the original extraction
    /// used different options (e.g., different `bin` name)
    fn extraction_type_from_cache(&self, cache_key: &str, file_info: &FileInfo) -> ExtractionType {
        // For archives, we don't need to detect the filename
        if !file_info.is_compressed_binary && file_info.format != file::ExtractionFormat::Raw {
            return ExtractionType::Archive;
        }

        // For raw files, find the actual filename in the cache directory
        let cache_path = self.cache_path(cache_key);
        for entry in xx::file::ls(&cache_path).unwrap_or_default() {
            if let Some(name) = entry.file_name().map(|n| n.to_string_lossy().to_string()) {
                // Skip metadata file
                if name != METADATA_FILE {
                    return ExtractionType::RawFile { filename: name };
                }
            }
        }

        // Fallback: shouldn't happen if cache is valid, but use a sensible default
        ExtractionType::RawFile {
            filename: self.ba.tool_name.clone(),
        }
    }

    // -------------------------------------------------------------------------
    // Extraction
    // -------------------------------------------------------------------------

    /// Extract artifact to cache with atomic rename
    fn extract_to_cache(
        &self,
        tv: &ToolVersion,
        file_path: &Path,
        cache_plan: &CachePlan,
        url: &str,
        opts: &HttpOptions<'_>,
        pr: Option<&dyn SingleReport>,
    ) -> Result<ExtractionType> {
        let cache_path = self.cache_path(&cache_plan.key);

        // Ensure parent directory exists
        file::create_dir_all(Self::tarballs_dir())?;

        // Create unique temp directory for atomic extraction
        let tmp_path = Self::tarballs_dir().join(format!(
            "{}.tmp-{}-{}",
            cache_plan.key,
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
        ));

        // Clean up any stale temp directory
        if tmp_path.exists() {
            let _ = file::remove_all(&tmp_path);
        }

        // Perform extraction
        let extraction_type =
            self.extract_artifact(tv, &tmp_path, file_path, cache_plan, opts, pr)?;

        // Atomic replace
        if cache_path.exists() {
            file::remove_all(&cache_path)?;
        }
        std::fs::rename(&tmp_path, &cache_path)?;

        // Write metadata
        self.write_metadata(&cache_plan.key, url, file_path, opts)?;

        Ok(extraction_type)
    }

    /// Extract a single artifact to the given directory
    fn extract_artifact(
        &self,
        tv: &ToolVersion,
        dest: &Path,
        file_path: &Path,
        cache_plan: &CachePlan,
        opts: &HttpOptions<'_>,
        pr: Option<&dyn SingleReport>,
    ) -> Result<ExtractionType> {
        file::create_dir_all(dest)?;

        if cache_plan.file_info.is_compressed_binary {
            self.extract_compressed_binary(dest, file_path, &cache_plan.file_info, opts, pr)
        } else if cache_plan.file_info.format == file::ExtractionFormat::Raw {
            self.extract_raw_file(dest, file_path, &cache_plan.file_info, opts, pr)
        } else {
            self.extract_archive(tv, dest, file_path, cache_plan, opts, pr)
        }
    }

    /// Extract a compressed binary (gz, xz, bz2, zst)
    fn extract_compressed_binary(
        &self,
        dest: &Path,
        file_path: &Path,
        file_info: &FileInfo,
        opts: &HttpOptions<'_>,
        pr: Option<&dyn SingleReport>,
    ) -> Result<ExtractionType> {
        let filename = self.dest_filename(file_path, file_info, opts);
        let dest_file = dest.join(&filename);

        // Report extraction progress (no bytes - we don't know total for extraction)
        if let Some(pr) = pr {
            pr.set_message(format!("extract {}", file_info.file_name()));
        }

        file::decompress_file(file_path, &dest_file, file_info.format)?;

        file::make_executable(&dest_file)?;
        Ok(ExtractionType::RawFile { filename })
    }

    /// Extract a raw (uncompressed) file
    fn extract_raw_file(
        &self,
        dest: &Path,
        file_path: &Path,
        file_info: &FileInfo,
        opts: &HttpOptions<'_>,
        pr: Option<&dyn SingleReport>,
    ) -> Result<ExtractionType> {
        let filename = self.dest_filename(file_path, file_info, opts);
        let dest_file = dest.join(&filename);

        // Report extraction progress (no bytes - we don't know total for extraction)
        if let Some(pr) = pr {
            pr.set_message(format!("extract {}", file_info.file_name()));
        }

        file::copy(file_path, &dest_file)?;

        file::make_executable(&dest_file)?;
        Ok(ExtractionType::RawFile { filename })
    }

    /// Extract an archive (tar, zip, etc.)
    fn extract_archive(
        &self,
        tv: &ToolVersion,
        dest: &Path,
        file_path: &Path,
        cache_plan: &CachePlan,
        opts: &HttpOptions<'_>,
        pr: Option<&dyn SingleReport>,
    ) -> Result<ExtractionType> {
        let extract_opts = file::ExtractOptions {
            strip_components: cache_plan.strip_components,
            pr,
            preserve_mtime: false,
        };

        file::extract_archive(file_path, dest, cache_plan.file_info.format, &extract_opts)?;

        // Handle rename_exe option for archives
        if let Some(rename_to) = opts.rename_exe() {
            // When bin_path is not explicitly set, auto-detect bin/ subdirectory to match
            // the same logic used by discover_bin_paths() for PATH construction
            let search_dir = if let Some(bin_path_template) = opts.bin_path() {
                let bin_path = template_string(&bin_path_template, tv);
                dest.join(&bin_path)
            } else {
                let bin_dir = dest.join("bin");
                if bin_dir.is_dir() {
                    bin_dir
                } else {
                    dest.to_path_buf()
                }
            };
            // rsplit('/') always yields at least one element (the full string if no delimiter)
            let tool_name = self.ba.tool_name.rsplit('/').next().unwrap();
            rename_executable_in_dir(&search_dir, &rename_to, Some(tool_name))?;
        }

        Ok(ExtractionType::Archive)
    }

    /// Write cache metadata file
    fn write_metadata(
        &self,
        cache_key: &str,
        url: &str,
        file_path: &Path,
        opts: &HttpOptions<'_>,
    ) -> Result<()> {
        let metadata = CacheMetadata {
            url: url.to_string(),
            checksum: opts.checksum(),
            size: file_path.metadata()?.len(),
            extracted_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            platform: self.get_platform_key(),
        };

        let json = serde_json::to_string_pretty(&metadata)?;
        file::write(self.metadata_path(cache_key), json)?;
        Ok(())
    }

    async fn prepare_artifact(
        &self,
        ctx: &InstallContext,
        tv: &mut ToolVersion,
        opts: &HttpOptions<'_>,
    ) -> Result<PreparedHttpArtifact> {
        let url_template = opts.url().ok_or_else(|| {
            let platform_key = self.get_platform_key();
            let available = opts.url_platforms();
            if !available.is_empty() {
                eyre::eyre!(
                    "No URL for platform {platform_key}. Available: {}. \
                     Provide 'url' or add 'platforms.{platform_key}.url'",
                    available.join(", ")
                )
            } else {
                eyre::eyre!("Http backend requires 'url' option")
            }
        })?;

        let url = template_string(&url_template, tv);
        let filename = get_filename_from_url(&url);
        let file_path = tv.download_path().join(&filename);

        let platform_key = self.get_platform_key();
        tv.lock_platforms
            .entry(platform_key.clone())
            .or_default()
            .url = Some(url.clone());

        let settings = Settings::get();
        let lockfile_enabled = settings.lockfile_enabled();
        let has_lockfile_checksum = tv
            .lock_platforms
            .get(&platform_key)
            .and_then(|p| p.checksum.as_ref())
            .is_some();

        if settings.offline()
            && !file_path.exists()
            && let Some((cache_plan, extraction_type)) =
                self.find_offline_cache_plan(&url, &file_path, opts)?
        {
            ctx.pr.set_message(format!("use cached tarball {filename}"));
            if let Some(checksum) = opts.checksum() {
                tv.lock_platforms
                    .entry(platform_key.clone())
                    .or_default()
                    .checksum = Some(checksum);
            }
            return Ok(PreparedHttpArtifact {
                url,
                filename,
                file_path,
                cache_plan,
                extraction_type,
                lockfile_enabled,
                has_lockfile_checksum: true,
                artifact_file_available: false,
            });
        }

        if settings.offline() && file_path.exists() {
            ctx.pr
                .set_message(format!("use cached download {filename}"));
        } else {
            ctx.pr.set_message(format!("download {filename}"));
            HTTP.download_file(&url, &file_path, Some(ctx.pr.as_ref()))
                .await?;
        }

        if opts.checksum().is_some() {
            ctx.pr.next_operation();
        }
        verify_artifact(tv, &file_path, opts.raw(), Some(ctx.pr.as_ref()))?;

        let cache_plan = self.cache_plan(&file_path, opts)?;
        let cache_path = self.cache_path(&cache_plan.key);
        let _lock = crate::lock_file::get(&cache_path, ctx.force)?;

        ctx.pr.next_operation();
        let extraction_type = if self.is_cached(&cache_plan.key) {
            ctx.pr.set_message("using cached tarball".into());
            ctx.pr.set_length(1);
            ctx.pr.set_position(1);
            self.extraction_type_from_cache(&cache_plan.key, &cache_plan.file_info)
        } else {
            ctx.pr.set_message("extracting to cache".into());
            self.extract_to_cache(
                tv,
                &file_path,
                &cache_plan,
                &url,
                opts,
                Some(ctx.pr.as_ref()),
            )?
        };

        Ok(PreparedHttpArtifact {
            url,
            filename,
            file_path,
            cache_plan,
            extraction_type,
            lockfile_enabled,
            has_lockfile_checksum,
            artifact_file_available: true,
        })
    }

    fn stage_cached_payload_for_store(
        &self,
        tv: &ToolVersion,
        build_path: &Path,
        cache_key: &str,
        extraction_type: &ExtractionType,
        opts: &HttpOptions<'_>,
    ) -> Result<()> {
        let cache_path = self.cache_path(cache_key);
        self.stage_cached_payload_from_path_for_store(
            tv,
            build_path,
            &cache_path,
            extraction_type,
            opts,
        )
    }

    fn stage_cached_payload_from_path_for_store(
        &self,
        tv: &ToolVersion,
        build_path: &Path,
        cache_path: &Path,
        extraction_type: &ExtractionType,
        opts: &HttpOptions<'_>,
    ) -> Result<()> {
        match extraction_type {
            ExtractionType::RawFile { filename } => {
                if let Some(bin_path_template) = opts.bin_path() {
                    let bin_path = template_string(&bin_path_template, tv);
                    let dest_dir = build_path.join(bin_path);
                    file::create_dir_all(&dest_dir)?;
                    self.copy_cache_file_to_store(
                        &cache_path.join(filename),
                        &dest_dir.join(filename),
                    )?;
                } else {
                    self.copy_cache_tree_to_store_build(&cache_path, build_path)?;
                }
            }
            ExtractionType::Archive => {
                self.copy_cache_tree_to_store_build(&cache_path, build_path)?;
            }
        }
        Ok(())
    }

    fn copy_cache_tree_to_store_build(&self, cache_path: &Path, build_path: &Path) -> Result<()> {
        for entry in WalkDir::new(cache_path)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
        {
            let entry = entry?;
            let src = entry.path();
            let rel = src.strip_prefix(cache_path)?;
            if rel.as_os_str().is_empty() || rel == Path::new(METADATA_FILE) {
                continue;
            }
            let dest = build_path.join(rel);
            if entry.file_type().is_dir() {
                file::create_dir_all(&dest)?;
            } else if entry.file_type().is_file() {
                self.copy_cache_file_to_store(src, &dest)?;
            } else if entry.file_type().is_symlink() {
                self.copy_cache_symlink_to_store(src, &dest)?;
            } else {
                bail!(
                    "http cache contains unsupported file type: {}",
                    file::display_path(src)
                );
            }
        }
        Ok(())
    }

    fn copy_cache_file_to_store(&self, src: &Path, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            file::create_dir_all(parent)?;
        }
        file::copy(src, dest)?;
        let permissions = fs::symlink_metadata(src)?.permissions();
        fs::set_permissions(dest, permissions).wrap_err_with(|| {
            format!("failed to copy permissions to {}", file::display_path(dest))
        })?;
        Ok(())
    }

    fn copy_cache_symlink_to_store(&self, src: &Path, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            file::create_dir_all(parent)?;
        }
        let target = fs::read_link(src)?;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, dest).wrap_err_with(|| {
                format!(
                    "failed to copy symlink {} -> {}",
                    file::display_path(src),
                    file::display_path(dest)
                )
            })?;
        }
        #[cfg(not(unix))]
        {
            let resolved = src.parent().unwrap_or_else(|| Path::new(".")).join(&target);
            if resolved.is_dir() {
                file::copy_dir_all(&resolved, dest)?;
            } else {
                self.copy_cache_file_to_store(&resolved, dest)?;
            }
        }
        Ok(())
    }

    fn store_bin_paths_for_build(
        &self,
        build_path: &Path,
        tv: &ToolVersion,
        opts: &HttpOptions<'_>,
    ) -> Result<Vec<PathBuf>> {
        if let Some(bin_path_template) = opts.bin_path() {
            return Ok(vec![PathBuf::from(template_string(
                bin_path_template.as_str(),
                tv,
            ))]);
        }

        if build_path.join("bin").is_dir() {
            return Ok(vec![PathBuf::from("bin")]);
        }

        let mut paths = Vec::new();
        if build_path.is_dir() {
            for entry in fs::read_dir(build_path)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let sub_bin = entry.path().join("bin");
                    if sub_bin.is_dir() {
                        paths.push(entry.file_name().to_string_lossy().to_string());
                    }
                }
            }
        }
        paths.sort();
        if paths.is_empty() {
            Ok(vec![PathBuf::from(".")])
        } else {
            Ok(paths
                .into_iter()
                .map(|path| PathBuf::from(path).join("bin"))
                .collect())
        }
    }

    fn store_executable_paths_for_build(
        &self,
        build_path: &Path,
        bin_paths: &[PathBuf],
    ) -> Result<Vec<PathBuf>> {
        let mut executable_paths = Vec::new();
        for bin_path in bin_paths {
            let full_bin_path = build_path.join(bin_path);
            if !full_bin_path.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&full_bin_path)? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_file() || file_type.is_symlink() {
                    executable_paths.push(bin_path.join(entry.file_name()));
                }
            }
        }
        executable_paths.sort();
        Ok(executable_paths)
    }

    fn store_derivation_id(
        &self,
        tv: &ToolVersion,
        prepared: &PreparedHttpArtifact,
    ) -> Result<String> {
        let options = toml::to_string(&tv.request.options())?;
        let key = format!(
            "schema_version=1\nbackend_type=http\nbackend={}\nshort={}\nversion={}\nplatform={}\nurl={}\ncache_key={}\nstrip_components={}\noptions={options}",
            self.ba.full_without_opts(),
            self.ba.short,
            tv.version,
            self.get_platform_key(),
            prepared.url,
            prepared.cache_plan.key,
            prepared.cache_plan.strip_components,
        );
        Ok(format!("sha256:{}", hash::hash_sha256_to_str(&key)))
    }

    fn store_options_hash(&self, tv: &ToolVersion) -> Result<String> {
        let options = toml::to_string(&tv.request.options())?;
        Ok(format!("sha256:{}", hash::hash_sha256_to_str(&options)))
    }

    fn store_source_hash(&self, prepared: &PreparedHttpArtifact) -> Result<String> {
        let artifact_hash = if prepared.file_path.exists() {
            hash::file_hash_blake3(&prepared.file_path, None)?
        } else {
            prepared
                .cache_plan
                .key
                .split('_')
                .next()
                .unwrap_or(&prepared.cache_plan.key)
                .to_string()
        };
        let key = format!(
            "url={}\nfilename={}\nartifact_blake3={artifact_hash}\ncache_key={}",
            prepared.url, prepared.filename, prepared.cache_plan.key
        );
        Ok(format!("sha256:{}", hash::hash_sha256_to_str(&key)))
    }

    // -------------------------------------------------------------------------
    // Symlink creation
    // -------------------------------------------------------------------------

    /// Return the single path component used for the HTTP install symlink.
    fn install_version_name(tv: &ToolVersion, cache_key: &str) -> String {
        if tv.version == "latest" {
            Self::content_version_name(cache_key)
        } else if tv.version.is_empty() {
            "_implicit".to_string()
        } else {
            Self::sanitize_install_version_name(&tv.version, tv.tv_pathname())
        }
    }

    /// Return the absolute path where the HTTP install symlink should live.
    fn install_path_for(tv: &ToolVersion, cache_key: &str) -> PathBuf {
        tv.ba()
            .installs_path
            .join(Self::install_version_name(tv, cache_key))
    }

    /// Return the install path later lookups should check for this HTTP tool.
    fn lookup_install_path(tv: &ToolVersion) -> PathBuf {
        if let Some(path) = &tv.install_path {
            return path.clone();
        }
        if tv.version == "latest" {
            tv.install_path()
        } else {
            tv.ba()
                .installs_path
                .join(Self::install_version_name(tv, ""))
        }
    }

    /// Return a deterministic content-derived version name for `latest` installs.
    fn content_version_name(cache_key: &str) -> String {
        let short = &cache_key[..7.min(cache_key.len())];
        if short.is_empty() {
            "_implicit".to_string()
        } else {
            short.to_string()
        }
    }

    /// Sanitize a requested version into a path component without collapsing identities.
    fn sanitize_install_version_name(raw_version: &str, version_name: String) -> String {
        let sanitized = match version_name.replace('\\', "-").as_str() {
            "." => "_".to_string(),
            ".." => "__".to_string(),
            name => name.to_string(),
        };
        if sanitized == raw_version {
            sanitized
        } else {
            let hash = hash::hash_sha256_to_str(raw_version);
            format!("{}-{}", sanitized, &hash[..7])
        }
    }

    /// Create install symlink(s) from install directory to cache
    fn create_install_symlink(
        &self,
        tv: &ToolVersion,
        cache_key: &str,
        extraction_type: &ExtractionType,
        opts: &HttpOptions<'_>,
    ) -> Result<()> {
        let cache_path = self.cache_path(cache_key);

        // Determine version name for install path
        let install_path = Self::install_path_for(tv, cache_key);

        // Clean up existing install
        if install_path.exists() {
            file::remove_all(&install_path)?;
        }
        if let Some(parent) = install_path.parent() {
            file::create_dir_all(parent)?;
        }

        // Handle raw files with bin_path specially for deduplication
        if let ExtractionType::RawFile { filename } = extraction_type
            && let Some(bin_path_template) = opts.bin_path()
        {
            let bin_path = template_string(&bin_path_template, tv);
            let dest_dir = install_path.join(&bin_path);
            file::create_dir_all(&dest_dir)?;

            let cached_file = cache_path.join(filename);
            let install_file = dest_dir.join(filename);
            file::make_symlink(&cached_file, &install_file)?;
            return Ok(());
        }

        // Default: symlink entire install path to cache
        file::make_symlink(&cache_path, &install_path)?;
        Ok(())
    }

    /// Create additional symlink for latest versions
    fn create_version_alias_symlink(&self, tv: &ToolVersion, cache_key: &str) -> Result<()> {
        if tv.version != "latest" {
            return Ok(());
        }

        let content_version = Self::content_version_name(cache_key);
        let original_path = tv.ba().installs_path.join(&tv.version);
        let content_path = tv.ba().installs_path.join(&content_version);

        if original_path.exists() {
            file::remove_all(&original_path)?;
        }
        if let Some(parent) = original_path.parent() {
            file::create_dir_all(parent)?;
        }

        file::make_symlink(&content_path, &original_path)?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Checksum verification
    // -------------------------------------------------------------------------

    /// Verify or generate checksum for lockfile support
    fn verify_checksum(
        &self,
        ctx: &InstallContext,
        tv: &mut ToolVersion,
        file_path: &Path,
    ) -> Result<()> {
        let settings = Settings::get();
        let filename = file_path.file_name().unwrap().to_string_lossy();
        let lockfile_enabled = settings.lockfile_enabled();

        let platform_key = self.get_platform_key();
        let platform_info = tv.lock_platforms.entry(platform_key).or_default();

        // Verify or generate checksum
        if let Some(checksum) = &platform_info.checksum {
            ctx.pr.set_message(format!("checksum {filename}"));
            let (algo, check) = checksum
                .split_once(':')
                .ok_or_else(|| eyre::eyre!("Invalid checksum format: {checksum}"))?;
            hash::ensure_checksum(file_path, check, Some(ctx.pr.as_ref()), algo)?;
        } else if lockfile_enabled {
            ctx.pr.set_message(format!("generate checksum {filename}"));
            let h = hash::file_hash_blake3(file_path, Some(ctx.pr.as_ref()))?;
            platform_info.checksum = Some(format!("blake3:{h}"));
        }

        // Verify or record size
        if let Some(expected_size) = platform_info.size {
            ctx.pr.set_message(format!("verify size {filename}"));
            let actual_size = file_path.metadata()?.len();
            if actual_size != expected_size {
                return Err(eyre::eyre!(
                    "Size mismatch for {filename}: expected {expected_size}, got {actual_size}"
                ));
            }
        } else if lockfile_enabled {
            platform_info.size = Some(file_path.metadata()?.len());
        }

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Version listing
    // -------------------------------------------------------------------------

    /// Fetch versions from version_list_url if configured
    async fn fetch_versions(&self, config: &Arc<Config>) -> Result<Vec<String>> {
        let raw_opts = config.get_tool_opts_with_overrides(&self.ba).await?;
        let opts = HttpOptions::new(&raw_opts);

        let url = match opts.version_list_url() {
            Some(url) => url.to_string(),
            None => return Ok(vec![]),
        };

        let regex = opts.version_regex();
        let json_path = opts.version_json_path();
        let version_expr = opts.version_expr();

        version_list::fetch_versions(&url, regex, json_path, version_expr).await
    }

    // -------------------------------------------------------------------------
    // Cross-platform lock resolution
    // -------------------------------------------------------------------------

    /// Resolve the artifact URL for a target platform during `mise lock`.
    /// Renders `os()`/`arch()` for the target rather than the host.
    fn lock_url_for_target(
        &self,
        opts: &HttpOptions<'_>,
        tv: &ToolVersion,
        target: &PlatformTarget,
    ) -> Option<String> {
        opts.url_for_target(target)
            .map(|template| template_string_for_target(&template, tv, target))
    }

    /// Resolve a published checksum for a target platform without downloading
    /// the artifact. Tries, in order: a checksum configured directly for the
    /// platform, a manifest evaluated via `checksum_expr`, a SHASUMS file keyed
    /// by filename, then an individual checksum file. Returns `None`
    /// (best-effort) when no published checksum is available.
    async fn resolve_lock_checksum(
        &self,
        opts: &HttpOptions<'_>,
        tv: &ToolVersion,
        target: &PlatformTarget,
        url: &str,
    ) -> Option<String> {
        // 1. Checksum declared directly for this platform.
        if let Some(checksum) = opts.checksum_for_target(target) {
            return Some(checksum);
        }

        // 2. Fetch from a declared checksum source.
        let checksum_url_template = opts.checksum_url_for_target(target)?;
        let checksum_url = template_string_for_target(&checksum_url_template, tv, target);
        let filename = get_filename_from_url(url);

        // 2a. Manifest with an extraction expression. The expression returns an
        // `algo:hash` string. The manifest is the same across platforms, so use
        // the cached fetch.
        if let Some(expr) = opts.checksum_expr() {
            let body = match HTTP.get_text_cached(&checksum_url).await {
                Ok(body) => body,
                Err(e) => {
                    debug!("failed to fetch checksum manifest {checksum_url}: {e}");
                    return None;
                }
            };
            let vars = [
                ("version", tv.version.as_str()),
                ("os", target.os_name()),
                ("arch", target.arch_name()),
                ("url", url),
                ("filename", filename.as_str()),
            ];
            return eval_checksum_expr(expr, &body, &vars);
        }

        // 2b. Checksum file: a SHASUMS list (filename match) first, then an
        // individual checksum file. The algorithm is detected from its name.
        if let Some(checksum) = fetch_checksum_from_shasums(&checksum_url, &filename).await {
            return Some(checksum);
        }
        // A SHASUMS list that has entries but none matching our artifact is a
        // naming mismatch, not an individual checksum file. Falling back to the
        // individual-file scan would return the first hash in the list — another
        // platform's checksum — and silently lock it. Bail so the platform is
        // reported unresolved instead.
        if shasums_has_entries(&checksum_url).await {
            debug!(
                "checksum_url {checksum_url} is a SHASUMS list with no entry for {filename}; \
                 not falling back to a first-hash scan"
            );
            return None;
        }
        let file_algo = crate::backend::asset_matcher::detect_checksum_algorithm(
            &get_filename_from_url(&checksum_url),
        );
        fetch_checksum_from_file(&checksum_url, &file_algo).await
    }
}

/// Returns install-time-only option keys for HTTP backend.
pub fn install_time_option_keys() -> Vec<String> {
    vec![
        "url".into(),
        "checksum".into(),
        "version_list_url".into(),
        "version_regex".into(),
        "version_json_path".into(),
        "version_expr".into(),
        "format".into(),
        "rename_exe".into(),
        "checksum_url".into(),
        "checksum_expr".into(),
    ]
}

#[async_trait]
impl Backend for HttpBackend {
    fn get_type(&self) -> BackendType {
        BackendType::Http
    }

    fn ba(&self) -> &Arc<BackendArg> {
        &self.ba
    }

    fn mark_prereleases_from_version_pattern(&self) -> bool {
        true
    }

    fn remote_version_listing_tool_option_keys(&self) -> &'static [&'static str] {
        &[
            "version_list_url",
            "version_regex",
            "version_json_path",
            "version_expr",
        ]
    }

    fn store_install_mode(&self, _tv: &ToolVersion) -> StoreInstallMode {
        StoreInstallMode::ImmutableRelocatable
    }

    fn store_install_plan(&self, _tv: &ToolVersion) -> BackendInstallPlan {
        BackendInstallPlan {
            phases: vec![
                InstallPhase::Fetch,
                InstallPhase::Build,
                InstallPhase::Smoke,
            ],
            monolithic: false,
            strict_sandbox_supported: false,
        }
    }

    async fn install_operation_count(&self, tv: &ToolVersion, _ctx: &InstallContext) -> usize {
        let raw_opts = tv.request.options();
        let opts = HttpOptions::new(&raw_opts);
        super::http_install_operation_count(opts.checksum().is_some(), &self.get_platform_key(), tv)
    }

    /// Options that affect which artifact is downloaded, resolved for the target
    /// platform so cross-platform lockfile entries match install-time lookups.
    fn resolve_lockfile_options(
        &self,
        request: &ToolRequest,
        target: &PlatformTarget,
    ) -> Result<BTreeMap<String, String>> {
        let raw_opts = request.options();
        let opts = HttpOptions::new(&raw_opts);
        let mut result = BTreeMap::new();
        if let Some(format) = opts.format_for_target(target) {
            result.insert("format".to_string(), format);
        }
        if let Some(strip) = opts.strip_components_for_target(target) {
            result.insert("strip_components".to_string(), strip);
        }
        if let Some(rename) = opts.rename_exe_for_target(target) {
            result.insert("rename_exe".to_string(), rename);
        }
        Ok(result)
    }

    /// Resolve URL + published checksum for a target platform during `mise lock`,
    /// without downloading the artifact. Best-effort: a platform with no
    /// resolvable URL fails closed (`Err`) so the lock run reports it as skipped
    /// rather than writing nothing under a success count; a missing checksum
    /// yields a url-only entry.
    async fn resolve_lock_info(
        &self,
        tv: &ToolVersion,
        target: &PlatformTarget,
    ) -> Result<PlatformInfo> {
        let raw_opts = tv.request.options();
        let opts = HttpOptions::new(&raw_opts);

        // Fail closed when the platform can't be resolved so the lock
        // orchestration reports it as skipped, rather than returning an empty
        // entry that is miscounted as a successful platform (see #7113).
        let Some(url) = self.lock_url_for_target(&opts, tv, target) else {
            return Err(eyre::eyre!(
                "no URL configured for {} on {}; skipping",
                self.ba.full(),
                target.to_key()
            ));
        };

        let checksum = self.resolve_lock_checksum(&opts, tv, target, &url).await;

        // A checksum source was configured but produced nothing for this target
        // (manifest miss, SHASUMS naming mismatch, unreachable file, ...). The
        // url-only entry is still written, but surface it so it isn't a silent
        // drop of checksum verification.
        if checksum.is_none() && opts.checksum_url_for_target(target).is_some() {
            warn!(
                "could not resolve a checksum for {} on {}; locking the URL without checksum verification",
                self.ba.full(),
                target.to_key()
            );
        }

        Ok(PlatformInfo {
            url: Some(url),
            checksum,
            ..Default::default()
        })
    }

    async fn _list_remote_versions(&self, config: &Arc<Config>) -> Result<Vec<VersionInfo>> {
        let versions = self.fetch_versions(config).await?;
        Ok(versions
            .into_iter()
            .map(|v| VersionInfo {
                version: v,
                ..Default::default()
            })
            .collect())
    }

    async fn install_version_(
        &self,
        ctx: &InstallContext,
        mut tv: ToolVersion,
    ) -> Result<ToolVersion> {
        let raw_opts = tv.request.options();
        let opts = HttpOptions::new(&raw_opts);
        let prepared = self.prepare_artifact(ctx, &mut tv, &opts).await?;

        // Create symlinks
        self.create_install_symlink(
            &tv,
            &prepared.cache_plan.key,
            &prepared.extraction_type,
            &opts,
        )?;
        self.create_version_alias_symlink(&tv, &prepared.cache_plan.key)?;
        tv.install_path = Some(Self::install_path_for(&tv, &prepared.cache_plan.key));

        // Verify checksum for lockfile
        if prepared.artifact_file_available
            && (prepared.lockfile_enabled || prepared.has_lockfile_checksum)
        {
            ctx.pr.next_operation();
            self.verify_checksum(ctx, &mut tv, &prepared.file_path)?;
        }

        Ok(tv)
    }

    async fn install_version_store(
        &self,
        ctx: &InstallContext,
        mut tv: ToolVersion,
    ) -> Result<ToolVersion> {
        self.validate_nise_immutable_store_install(&tv)?;
        if tv.request.options().contains_key("postinstall") {
            bail!(
                "http immutable store installs do not support postinstall hooks yet; use legacy install mode for {}",
                tv.style()
            );
        }

        let raw_opts = tv.request.options();
        let opts = HttpOptions::new(&raw_opts);
        let install_ops = self.install_operation_count(&tv, ctx).await + 2;
        ctx.pr.start_operations(install_ops);

        let prepared = self.prepare_artifact(ctx, &mut tv, &opts).await?;
        if prepared.artifact_file_available
            && (prepared.lockfile_enabled || prepared.has_lockfile_checksum)
        {
            ctx.pr.next_operation();
            self.verify_checksum(ctx, &mut tv, &prepared.file_path)?;
        }

        let compatibility_path = Self::install_path_for(&tv, &prepared.cache_plan.key);
        tv.install_path = Some(compatibility_path.clone());
        let _install_lock = crate::lock_file::get(&compatibility_path, ctx.force)?;
        if !ctx.force && self.is_version_installed(&ctx.config, &tv, true) {
            return Ok(tv);
        }

        let store = StoreRoot::default();
        let derivation_id = self.store_derivation_id(&tv, &prepared)?;
        let mut txn = StoreTxn::begin(
            store.clone(),
            StoreTxnInput {
                derivation_id,
                compatibility_path: compatibility_path.clone(),
            },
        )?;

        ctx.pr.next_operation();
        ctx.pr.set_message("stage store object".into());
        txn.set_state(StoreTxnState::Building)?;
        if let Err(err) = self.stage_cached_payload_for_store(
            &tv,
            txn.build_path(),
            &prepared.cache_plan.key,
            &prepared.extraction_type,
            &opts,
        ) {
            let _ = txn.fail();
            return Err(err);
        }

        let bin_paths = match self.store_bin_paths_for_build(txn.build_path(), &tv, &opts) {
            Ok(paths) => paths,
            Err(err) => {
                let _ = txn.fail();
                return Err(err);
            }
        };
        let executable_paths =
            match self.store_executable_paths_for_build(txn.build_path(), &bin_paths) {
                Ok(paths) => paths,
                Err(err) => {
                    let _ = txn.fail();
                    return Err(err);
                }
            };
        let cache_path = self.cache_path(&prepared.cache_plan.key);
        let platform_key = self.get_platform_key();
        let checksum = tv
            .lock_platforms
            .get(&platform_key)
            .and_then(|platform| platform.checksum.clone());
        let provenance = vec![ProvenanceRecord {
            kind: "http-artifact".to_string(),
            source: Some(prepared.url.clone()),
            value: checksum.clone(),
            verified: Some(checksum.is_some() || opts.checksum().is_some()),
        }];
        let object_name = format!(
            "{}-{}",
            self.ba.short,
            Self::install_version_name(&tv, &prepared.cache_plan.key)
        );
        let relocation_tokens = vec![
            txn.build_path().to_string_lossy().to_string(),
            cache_path.to_string_lossy().to_string(),
        ];

        ctx.pr.next_operation();
        ctx.pr.set_message("publish store object".into());
        let publication = match publish_staged_store_install(
            &store,
            &mut txn,
            StagedStoreInstallPublishInput {
                object: StoreObjectPublishInput {
                    name: object_name,
                    platform: platform_key.clone(),
                    created_by: "nise-http".to_string(),
                    executable_paths,
                    bin_paths,
                    references: vec![],
                    realisations: vec![],
                    relocation_tokens,
                },
                realisation: StoreRealisationMetadata {
                    tool: self.ba.short.clone(),
                    backend: self.ba.full_without_opts(),
                    version: tv.tv_pathname(),
                    platform: platform_key,
                    options_hash: self.store_options_hash(&tv)?,
                    source_hash: self.store_source_hash(&prepared)?,
                    lock_policy: if ctx.locked {
                        "locked".to_string()
                    } else {
                        "artifact".to_string()
                    },
                    provenance,
                    closure: vec![],
                },
            },
        ) {
            Ok(publication) => publication,
            Err(err) => {
                let _ = txn.fail();
                return Err(err);
            }
        };
        txn.complete()?;

        self.create_version_alias_symlink(&tv, &prepared.cache_plan.key)?;

        if compatibility_path.starts_with(*dirs::INSTALLS) {
            install_state::write_backend_meta(self.ba())?;
            install_state::add_tool_version(self.ba(), &compatibility_path, &tv.tv_pathname());
        }

        let mut touch_dirs = vec![dirs::DATA.to_path_buf()];
        touch_dirs.extend(ctx.config.config_files.keys().cloned());
        for path in touch_dirs {
            if let Err(err) = file::touch_dir(&path) {
                trace!("error touching config file: {:?} {:?}", path, err);
            }
        }

        debug!(
            "published http store install {} -> {}",
            tv.style(),
            file::display_path(publication.object.path)
        );
        Ok(tv)
    }

    fn is_version_installed(
        &self,
        _config: &Arc<Config>,
        tv: &ToolVersion,
        check_symlink: bool,
    ) -> bool {
        match tv.request {
            ToolRequest::System { .. } => true,
            _ => {
                let install_path = Self::lookup_install_path(tv);
                install_path.exists()
                    && !self.incomplete_file_path(tv).exists()
                    && (!check_symlink || !is_runtime_symlink(&install_path))
            }
        }
    }

    async fn list_bin_paths(
        &self,
        _config: &Arc<Config>,
        tv: &ToolVersion,
    ) -> Result<Vec<PathBuf>> {
        let raw_opts = tv.request.options();
        let opts = HttpOptions::new(&raw_opts);
        let install_path = Self::lookup_install_path(tv);
        let mut tv = tv.clone();
        tv.install_path = Some(install_path.clone());

        // Check for explicit bin_path
        if let Some(bin_path_template) = opts.bin_path() {
            let bin_path = template_string(&bin_path_template, &tv);
            return Ok(vec![runtime_path_for_install_path(
                &tv,
                install_path.join(bin_path),
            )]);
        }

        // Check for bin directory
        let bin_dir = install_path.join("bin");
        if bin_dir.exists() {
            return Ok(vec![runtime_path_for_install_path(
                &tv,
                install_path.join("bin"),
            )]);
        }

        // Search subdirectories for bin directories
        let mut paths = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&install_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let sub_bin = path.join("bin");
                    if sub_bin.exists() {
                        paths.push(sub_bin);
                    }
                }
            }
        }

        if paths.is_empty() {
            Ok(vec![runtime_path_for_install_path(&tv, install_path)])
        } else {
            Ok(paths
                .into_iter()
                .map(|path| runtime_path_for_install_path(&tv, path))
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::BackendResolution;
    use crate::toolset::{ToolRequest, ToolSource};

    fn http_test_backend() -> HttpBackend {
        HttpBackend::from_arg(BackendArg::new_raw(
            "http-absolute-version".to_string(),
            Some("http:absolute-version".to_string()),
            "absolute-version".to_string(),
            None,
            BackendResolution::new(true),
        ))
    }

    fn http_test_tv(version: &str) -> ToolVersion {
        let backend = Arc::new(BackendArg::new_raw(
            "http-absolute-version".to_string(),
            Some("http:absolute-version".to_string()),
            "absolute-version".to_string(),
            None,
            BackendResolution::new(true),
        ));
        let request = ToolRequest::Version {
            backend,
            version: version.to_string(),
            options: ToolVersionOptions::default(),
            source: ToolSource::Argument,
        };
        ToolVersion::new(request, version.to_string())
    }

    fn http_test_tv_with_options(version: &str, options: ToolVersionOptions) -> ToolVersion {
        let backend = Arc::new(BackendArg::new_raw(
            "http-absolute-version".to_string(),
            Some("http:absolute-version".to_string()),
            "absolute-version".to_string(),
            None,
            BackendResolution::new(true),
        ));
        let request = ToolRequest::Version {
            backend,
            version: version.to_string(),
            options,
            source: ToolSource::Argument,
        };
        ToolVersion::new(request, version.to_string())
    }

    fn version_hash(version: &str) -> String {
        crate::hash::hash_sha256_to_str(version)[..7].to_string()
    }

    #[test]
    fn template_string_for_target_renders_target_os_arch() {
        let tv = http_test_tv("0.40.0");
        let template =
            r#"sentinel_{{ version }}_{{ os(macos="darwin") }}_{{ arch(x64="amd64") }}.zip"#;
        let linux = PlatformTarget::new(crate::platform::Platform::parse("linux-x64").unwrap());
        assert_eq!(
            template_string_for_target(template, &tv, &linux),
            "sentinel_0.40.0_linux_amd64.zip"
        );
        let win = PlatformTarget::new(crate::platform::Platform::parse("windows-x64").unwrap());
        assert_eq!(
            template_string_for_target(template, &tv, &win),
            "sentinel_0.40.0_windows_amd64.zip"
        );
    }

    #[test]
    fn install_symlink_path_uses_sanitized_version_pathname() {
        let version = "/outside-root/mise-http-version-out/selected-prefix";
        let tv = http_test_tv(version);
        let version_name = HttpBackend::install_version_name(&tv, "abcdef123456");

        assert_eq!(
            version_name,
            format!(
                "-outside-root-mise-http-version-out-selected-prefix-{}",
                version_hash(version)
            )
        );
        assert!(!Path::new(&version_name).is_absolute());
    }

    #[test]
    fn install_symlink_path_sanitizes_parent_version() {
        let version = "..";
        let tv = http_test_tv(version);
        let version_name = HttpBackend::install_version_name(&tv, "abcdef123456");

        assert_eq!(version_name, format!("__-{}", version_hash(version)));
        assert!(
            Path::new(&version_name)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
        );
    }

    #[test]
    fn install_symlink_path_sanitizes_windows_separators() {
        let version = r"..\..\outside-root\mise-http-version-out\selected-prefix";
        let tv = http_test_tv(version);
        let version_name = HttpBackend::install_version_name(&tv, "abcdef123456");

        assert_eq!(
            version_name,
            format!(
                "..-..-outside-root-mise-http-version-out-selected-prefix-{}",
                version_hash(version)
            )
        );
        assert!(!version_name.contains('\\'));
    }

    #[test]
    fn install_symlink_path_sanitizes_windows_unc_paths() {
        let version = r"\\server\share";
        let tv = http_test_tv(version);
        let version_name = HttpBackend::install_version_name(&tv, "abcdef123456");

        assert_eq!(
            version_name,
            format!("--server-share-{}", version_hash(version))
        );
        assert!(!version_name.contains('\\'));
    }

    #[test]
    fn install_symlink_path_preserves_distinct_sanitized_versions() {
        let slash = HttpBackend::install_version_name(&http_test_tv("a/b"), "abcdef123456");
        let colon = HttpBackend::install_version_name(&http_test_tv("a:b"), "abcdef123456");
        let backslash = HttpBackend::install_version_name(&http_test_tv(r"a\b"), "abcdef123456");
        let dash = HttpBackend::install_version_name(&http_test_tv("a-b"), "abcdef123456");

        assert_eq!(dash, "a-b");
        assert_ne!(slash, dash);
        assert_ne!(colon, dash);
        assert_ne!(backslash, dash);
        assert_ne!(slash, colon);
        assert_ne!(slash, backslash);
        assert_ne!(colon, backslash);
    }

    #[test]
    fn latest_install_symlink_still_uses_content_version() {
        let tv = http_test_tv("latest");
        let version_name = HttpBackend::install_version_name(&tv, "abcdef123456");

        assert_eq!(version_name, "abcdef1");
    }

    #[test]
    fn empty_install_symlink_uses_implicit_version() {
        let tv = http_test_tv("");
        let version_name = HttpBackend::install_version_name(&tv, "abcdef123456");

        assert_eq!(version_name, "_implicit");
    }

    #[test]
    fn empty_install_path_uses_implicit_version_path() {
        let tv = http_test_tv("");
        let install_path = HttpBackend::install_path_for(&tv, "abcdef123456");

        assert_eq!(install_path, tv.ba().installs_path.join("_implicit"));
        assert_ne!(install_path, tv.ba().installs_path);
    }

    #[test]
    fn lookup_install_path_matches_sanitized_install_path() {
        let version = "/outside-root/mise-http-version-out/selected-prefix";
        let tv = http_test_tv(version);
        let install_path = HttpBackend::install_path_for(&tv, "abcdef123456");
        let lookup_path = HttpBackend::lookup_install_path(&tv);

        assert_eq!(lookup_path, install_path);
    }

    #[test]
    fn store_cache_copy_excludes_metadata_and_discovers_bin_paths() -> Result<()> {
        let backend = http_test_backend();
        let tmp = tempfile::tempdir()?;
        let cache = tmp.path().join("cache");
        let build = tmp.path().join("build");
        crate::file::create_dir_all(cache.join("bin"))?;
        crate::file::create_dir_all(&build)?;
        crate::file::write(cache.join(METADATA_FILE), "cache metadata")?;
        crate::file::write(cache.join("bin").join("demo"), "demo")?;
        crate::file::write(cache.join("README.md"), "readme")?;

        backend.copy_cache_tree_to_store_build(&cache, &build)?;

        assert!(!build.join(METADATA_FILE).exists());
        assert_eq!(
            crate::file::read_to_string(build.join("bin").join("demo"))?,
            "demo"
        );
        assert_eq!(
            crate::file::read_to_string(build.join("README.md"))?,
            "readme"
        );

        let tv = http_test_tv("1.0.0");
        let raw_opts = tv.request.options();
        let opts = HttpOptions::new(&raw_opts);
        let bin_paths = backend.store_bin_paths_for_build(&build, &tv, &opts)?;
        let executable_paths = backend.store_executable_paths_for_build(&build, &bin_paths)?;

        assert_eq!(bin_paths, vec![PathBuf::from("bin")]);
        assert_eq!(executable_paths, vec![PathBuf::from("bin/demo")]);
        Ok(())
    }

    #[test]
    fn raw_file_store_stage_honors_bin_path_layout() -> Result<()> {
        let backend = http_test_backend();
        let tmp = tempfile::tempdir()?;
        let cache = tmp.path().join("cache");
        let build = tmp.path().join("build");
        crate::file::create_dir_all(&cache)?;
        crate::file::create_dir_all(&build)?;
        crate::file::write(cache.join("demo"), "demo")?;
        crate::file::write(cache.join(METADATA_FILE), "cache metadata")?;

        let mut options = ToolVersionOptions::default();
        options
            .insert_option(
                "bin_path".to_string(),
                toml::Value::String("tools/bin".to_string()),
            )
            .unwrap();
        let tv = http_test_tv_with_options("1.0.0", options);
        let raw_opts = tv.request.options();
        let opts = HttpOptions::new(&raw_opts);

        backend.stage_cached_payload_from_path_for_store(
            &tv,
            &build,
            &cache,
            &ExtractionType::RawFile {
                filename: "demo".to_string(),
            },
            &opts,
        )?;

        assert_eq!(
            crate::file::read_to_string(build.join("tools/bin/demo"))?,
            "demo"
        );
        assert!(!build.join("demo").exists());
        assert!(!build.join(METADATA_FILE).exists());

        let bin_paths = backend.store_bin_paths_for_build(&build, &tv, &opts)?;
        let executable_paths = backend.store_executable_paths_for_build(&build, &bin_paths)?;

        assert_eq!(bin_paths, vec![PathBuf::from("tools/bin")]);
        assert_eq!(executable_paths, vec![PathBuf::from("tools/bin/demo")]);
        Ok(())
    }
}
