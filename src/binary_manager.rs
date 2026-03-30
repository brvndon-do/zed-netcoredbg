use crate::logger::Logger;
use fs_extra::dir;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use zed_extension_api::{self as zed, DownloadedFileType, GithubReleaseOptions};

/// GitHub release version information
#[derive(Debug, Clone)]
pub struct AdapterVersion {
    /// Release tag name (version)
    pub tag_name: String,
    /// Download URL for the release asset
    pub download_url: String,
}

pub struct BinaryManager {
    /// Cached absolute path to the netcoredbg binary - set once and reused
    cached_binary_path: OnceLock<String>,
}

impl Default for BinaryManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BinaryManager {
    const GITHUB_OWNER: &str = "Samsung";
    const GITHUB_REPO: &str = "netcoredbg";

    /// Metadata file that records the currently installed version
    const INSTALLED_VERSION_FILE: &str = ".installed_version";

    pub fn new() -> Self {
        Self {
            cached_binary_path: OnceLock::new(),
        }
    }

    fn get_executable_name() -> &'static str {
        if zed::current_platform().0 == zed::Os::Windows {
            "netcoredbg.exe"
        } else {
            "netcoredbg"
        }
    }

    /// Determines the appropriate asset name for the current platform
    /// Supported assets:
    /// - netcoredbg-linux-amd64.tar.gz
    /// - netcoredbg-linux-arm64.tar.gz
    /// - netcoredbg-osx-amd64.tar.gz
    /// - netcoredbg-osx-arm64.tar.gz
    /// - netcoredbg-win64.zip
    fn get_platform_asset_name() -> Result<String, String> {
        let (platform, arch) = zed::current_platform();

        let (platform_arch, extension) = match (platform, arch) {
            (zed::Os::Linux, zed::Architecture::X8664) => ("linux-amd64", ".tar.gz"),
            (zed::Os::Linux, zed::Architecture::Aarch64) => ("linux-arm64", ".tar.gz"),
            (zed::Os::Mac, zed::Architecture::X8664) => ("osx-amd64", ".tar.gz"),
            (zed::Os::Mac, zed::Architecture::Aarch64) => ("osx-arm64", ".tar.gz"),
            (zed::Os::Windows, zed::Architecture::X8664) => ("win64", ".zip"),
            (zed::Os::Windows, zed::Architecture::Aarch64) => {
                // Windows ARM64 is not officially supported by netcoredbg,
                // but we can try the x64 version as a fallback
                ("win64", ".zip")
            }
            (_, zed::Architecture::X86) => {
                return Err("Unsupported architecture: x86 (32-bit). NetCoreDbg only supports 64-bit architectures (amd64/arm64).".to_string());
            }
        };

        Ok(format!("netcoredbg-{}{}", platform_arch, extension))
    }

    fn bin_dir() -> PathBuf {
        PathBuf::from("bin")
    }

    fn version_dir(version: &str) -> PathBuf {
        Self::bin_dir().join(version)
    }

    fn tmp_dir() -> PathBuf {
        PathBuf::from("tmp")
    }

    fn tmp_version_dir(version: &str) -> PathBuf {
        Self::tmp_dir().join(version)
    }

    fn installed_version_file() -> PathBuf {
        Self::bin_dir().join(Self::INSTALLED_VERSION_FILE)
    }

    fn ensure_directory(path: &Path) -> Result<(), String> {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("Failed to create directory {}: {}", path.display(), e))
    }

    fn remove_directory_if_exists(path: &Path) -> Result<(), String> {
        if path.exists() {
            std::fs::remove_dir_all(path)
                .map_err(|e| format!("Failed to remove directory {}: {}", path.display(), e))?;
        }
        Ok(())
    }

    fn working_dir() -> Result<PathBuf, String> {
        std::env::current_dir()
            .map_err(|e| format!("Failed to get extension working directory: {}", e))
    }

    fn to_absolute(relative: &Path) -> Result<PathBuf, String> {
        Ok(Self::working_dir()?.join(relative))
    }

    fn read_installed_version() -> Option<String> {
        let path = Self::installed_version_file();
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let version = contents.trim().to_string();
                if version.is_empty() {
                    None
                } else {
                    Some(version)
                }
            }
            Err(_) => None,
        }
    }

    fn write_installed_version(version: &str) -> Result<(), String> {
        let path = Self::installed_version_file();
        std::fs::write(&path, version)
            .map_err(|e| format!("Failed to write installed version file: {}", e))
    }

    /// Fetches the latest release information from GitHub
    fn fetch_latest_release(&self) -> Result<AdapterVersion, String> {
        let release = zed::latest_github_release(
            &format!("{}/{}", Self::GITHUB_OWNER, Self::GITHUB_REPO),
            GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )
        .map_err(|e| {
            Logger::debug(&format!("Failed to fetch latest release: {}", e));
            format!("Failed to fetch latest release: {}", e)
        })?;

        let asset_name = Self::get_platform_asset_name()?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| {
                let msg = format!(
                    "No compatible asset found for platform. \
                     Looking for: '{}'. Available assets: [{}]",
                    asset_name,
                    release
                        .assets
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                Logger::debug(&msg);
                msg
            })?;

        Logger::debug(&format!(
            "Found release version: {}, asset: {}",
            release.version, asset.name
        ));

        Ok(AdapterVersion {
            tag_name: release.version,
            download_url: asset.download_url.clone(),
        })
    }

    /// Downloads the release archive, extracts it, and installs the binary under `bin/<version>/`
    fn download_and_install(&self, version: &AdapterVersion) -> Result<String, String> {
        let asset_name = Self::get_platform_asset_name()?;

        let file_type = if asset_name.ends_with(".zip") {
            DownloadedFileType::Zip
        } else if asset_name.ends_with(".tar.gz") {
            DownloadedFileType::GzipTar
        } else {
            return Err(format!("Unsupported file type for asset: {}", asset_name));
        };

        let version_dir = Self::version_dir(&version.tag_name);
        let tmp_dir = Self::tmp_version_dir(&version.tag_name);

        // prepare directories
        Self::ensure_directory(&Self::bin_dir())?;
        Self::remove_directory_if_exists(&tmp_dir)?;
        Self::ensure_directory(&tmp_dir)?;
        Self::remove_directory_if_exists(&version_dir)?;
        Self::ensure_directory(&version_dir)?;

        Logger::debug(&format!(
            "Downloading netcoredbg {} to tmp dir: {}",
            version.tag_name,
            tmp_dir.display()
        ));

        zed::download_file(&version.download_url, &tmp_dir.to_string_lossy(), file_type).map_err(
            |e| {
                Logger::debug(&format!("Failed to download netcoredbg: {}", e));
                format!("Failed to download netcoredbg: {}", e)
            },
        )?;
        Logger::debug("Download completed successfully.");

        Logger::debug(&format!(
            "Copying extracted content from {} to {}",
            tmp_dir.display(),
            version_dir.display()
        ));

        self.copy_extracted_content(&tmp_dir, &version_dir)?;

        let exe_name = Self::get_executable_name();
        let binary_path = version_dir.join(exe_name);

        if !binary_path.exists() {
            Logger::debug(&format!(
                "Binary not found at expected path: {}",
                binary_path.display()
            ));
            return Err(format!(
                "netcoredbg executable not found at: {}",
                binary_path.display()
            ));
        }

        zed::make_file_executable(&binary_path.to_string_lossy()).map_err(|e| {
            Logger::debug(&format!("Failed to make file executable: {}", e));
            format!("Failed to make file executable: {}", e)
        })?;

        Logger::debug("Cleaning up tmp dir...");
        Self::remove_directory_if_exists(&tmp_dir)?;

        Self::write_installed_version(&version.tag_name)?;

        let abs_path = Self::to_absolute(&binary_path)?;
        Logger::debug(&format!(
            "Successfully installed netcoredbg to: {}",
            abs_path.display()
        ));

        Ok(abs_path.to_string_lossy().to_string())
    }

    /// Copies extracted content from temp_dir into version_dir, handling nested directory structure
    fn copy_extracted_content(&self, tmp_dir: &Path, version_dir: &Path) -> Result<(), String> {
        let exe_name = Self::get_executable_name();
        let binary_source_path = self.find_binary_in_extracted_content(tmp_dir, exe_name)?;
        let source_dir = binary_source_path.parent().ok_or_else(|| {
            Logger::debug("Binary has no parent directory");
            "Binary has no parent directory".to_string()
        })?;

        Logger::debug(&format!(
            "Found binary at: {}, copying parent dir contents: {}",
            binary_source_path.display(),
            source_dir.display()
        ));

        let copy_options = dir::CopyOptions::new().content_only(true);
        dir::copy(source_dir, version_dir, &copy_options).map_err(|e| {
            let msg = format!(
                "Failed to copy extracted content from {}: {}",
                source_dir.display(),
                e
            );
            Logger::debug(&msg);
            msg
        })?;

        Ok(())
    }

    /// Recursively searches for the netcoredbg binary in the extracted content
    fn find_binary_in_extracted_content(
        &self,
        search_dir: &Path,
        exe_name: &str,
    ) -> Result<PathBuf, String> {
        fn find_binary_recursive(dir: &Path, exe_name: &str) -> Result<Option<PathBuf>, String> {
            let entries = std::fs::read_dir(dir)
                .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

            for entry in entries {
                let entry = entry.map_err(|e| format!("Failed to read directory entry: {}", e))?;
                let path = entry.path();

                if path.is_file() && path.file_name().is_some_and(|name| name == exe_name) {
                    return Ok(Some(path));
                } else if path.is_dir() {
                    if let Some(found) = find_binary_recursive(&path, exe_name)? {
                        return Ok(Some(found));
                    }
                }
            }
            Ok(None)
        }

        let result = find_binary_recursive(search_dir, exe_name)?;
        if let Some(ref path) = result {
            Logger::debug(&format!(
                "Found binary '{}' at {}",
                exe_name,
                path.display()
            ));
        } else {
            Logger::debug(&format!(
                "Could not find '{}' in {}",
                exe_name,
                search_dir.display()
            ));
        }

        result.ok_or_else(|| {
            format!(
                "Could not find {} binary in extracted content at {}",
                exe_name,
                search_dir.display()
            )
        })
    }

    /// Gets the netcoredbg binary path, downloading if necessary
    pub fn get_binary_path(&self, user_provided_path: Option<String>) -> Result<String, String> {
        Logger::debug("Starting get_binary_path");
        Logger::debug(&format!(
            "Extension working directory: {:?}",
            Self::working_dir()
        ));

        // Priority 1: User-provided path return as is without any validation
        if let Some(user_path) = user_provided_path {
            Logger::debug(&format!("Using user-provided path: {}", user_path));
            return Ok(user_path);
        }

        // Priority 2: Check in-memory cache
        if let Some(cached_path) = self.cached_binary_path.get() {
            if Path::new(cached_path).exists() {
                Logger::debug(&format!("Using cached binary path: {}", cached_path));
                return Ok(cached_path.clone());
            }
            Logger::debug("Cached binary no longer exists on disk, will resolve again");
        }

        // Priority 3: Check existing binary on disk before downloading
        if let Some(installed_version) = Self::read_installed_version() {
            Logger::debug(&format!(
                "Found installed version metadata: {}",
                installed_version
            ));

            let exe_name = Self::get_executable_name();
            let binary_rel = Self::version_dir(&installed_version).join(exe_name);
            let binary_abs = Self::to_absolute(&binary_rel)?;

            if binary_abs.exists() {
                Logger::debug(&format!(
                    "Using previously installed binary: {}",
                    binary_abs.display()
                ));
                let path_str = binary_abs.to_string_lossy().to_string();
                let _ = self.cached_binary_path.set(path_str.clone());
                return Ok(path_str);
            }

            Logger::debug(&format!(
                "Installed version {} recorded but binary missing at {}",
                installed_version,
                binary_abs.display()
            ));
        } else {
            Logger::debug("No installed version metadata found");
        }

        // Priority 4: Fetch latest release info from GitHub
        Logger::debug("Fetching latest release info from GitHub...");
        let version = self.fetch_latest_release()?;
        Logger::debug(&format!("Latest version: {}", version.tag_name));

        // check if this version is already installed on disk
        let exe_name = Self::get_executable_name();
        let binary_rel = Self::version_dir(&version.tag_name).join(exe_name);
        let binary_abs = Self::to_absolute(&binary_rel)?;

        if binary_abs.exists() {
            Logger::debug(&format!(
                "Latest version already installed on disk: {}",
                binary_abs.display()
            ));
            // update metadata file
            Self::write_installed_version(&version.tag_name)?;
            let path_str = binary_abs.to_string_lossy().to_string();
            let _ = self.cached_binary_path.set(path_str.clone());
            return Ok(path_str);
        }

        // Priority 5: Download, extract, and install
        Logger::debug("Binary not found on disk, downloading from GitHub...");
        let binary_path = match self.download_and_install(&version) {
            Ok(path) => path,
            Err(e) => {
                Logger::debug(&format!("Failed to download and install: {}", e));
                return Err(e);
            }
        };

        Logger::debug(&format!(
            "Successfully downloaded and installed to: {}",
            binary_path
        ));
        let _ = self.cached_binary_path.set(binary_path.clone());

        Ok(binary_path)
    }
}
