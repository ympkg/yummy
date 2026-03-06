use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Main package.json configuration
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct YmConfig {
    pub name: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<BTreeMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub dev_dependencies: Option<BTreeMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_dependencies: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub jvm_args: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub scripts: Option<BTreeMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolutions: Option<BTreeMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub registries: Option<BTreeMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub jvm: Option<JvmConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub compiler: Option<CompilerConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub hot_reload: Option<HotReloadConfig>,

    /// Transitive dependency exclusions (e.g., ["commons-logging:commons-logging"])
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclusions: Option<Vec<String>>,

    /// Custom source directory (defaults to src/main/java or src/)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_dir: Option<String>,

    /// Custom test directory (defaults to src/test/java or test/)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_dir: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct JvmConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_download: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompilerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// Annotation processor dependencies (groupId:artifactId format)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation_processors: Option<Vec<String>>,
    /// Javac lint options (e.g., ["all", "-serial", "deprecation"])
    /// Passed as -Xlint:option to javac
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lint: Option<Vec<String>>,
    /// Additional compiler arguments passed directly to javac/ecj
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HotReloadConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch_extensions: Option<Vec<String>>,
}

/// Lock file (package-lock.json)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LockFile {
    pub version: u32,
    pub dependencies: BTreeMap<String, LockedDependency>,
}

impl Default for LockFile {
    fn default() -> Self {
        Self {
            version: 1,
            dependencies: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LockedDependency {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<Vec<String>>,
}
