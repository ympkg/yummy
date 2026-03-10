use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

/// Dependency value: either a simple version string or a detailed spec
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum DependencyValue {
    /// Simple version string: `"com.google.guava:guava" = "33.4.0"`
    Simple(String),
    /// Detailed spec: `{ version = "2.19.0", scope = "test", exclude = [...] }`
    /// or workspace ref: `{ workspace = true }`
    Detailed(DependencySpec),
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct DependencySpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<bool>,
    /// Direct URL to a JAR file
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Git repository URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    /// Git branch/tag/commit (defaults to HEAD)
    #[serde(skip_serializing_if = "Option::is_none", rename = "ref")]
    pub git_ref: Option<String>,
}

impl DependencyValue {
    /// Get the version string (resolves simple and detailed variants)
    pub fn version(&self) -> Option<&str> {
        match self {
            DependencyValue::Simple(v) => Some(v),
            DependencyValue::Detailed(spec) => spec.version.as_deref(),
        }
    }

    /// Get the scope (defaults to "compile")
    pub fn scope(&self) -> &str {
        match self {
            DependencyValue::Simple(_) => "compile",
            DependencyValue::Detailed(spec) => spec.scope.as_deref().unwrap_or("compile"),
        }
    }

    /// Check if this is a workspace reference
    pub fn is_workspace(&self) -> bool {
        match self {
            DependencyValue::Simple(_) => false,
            DependencyValue::Detailed(spec) => spec.workspace.unwrap_or(false),
        }
    }

    /// Get the classifier (e.g. "natives-linux", "sources", "javadoc")
    pub fn classifier(&self) -> Option<&str> {
        match self {
            DependencyValue::Simple(_) => None,
            DependencyValue::Detailed(spec) => spec.classifier.as_deref(),
        }
    }

    /// Check if this is a URL dependency
    pub fn url(&self) -> Option<&str> {
        match self {
            DependencyValue::Simple(_) => None,
            DependencyValue::Detailed(spec) => spec.url.as_deref(),
        }
    }

    /// Check if this is a Git dependency
    pub fn git(&self) -> Option<&str> {
        match self {
            DependencyValue::Simple(_) => None,
            DependencyValue::Detailed(spec) => spec.git.as_deref(),
        }
    }

    /// Get the git ref (branch/tag/commit)
    pub fn git_ref(&self) -> Option<&str> {
        match self {
            DependencyValue::Simple(_) => None,
            DependencyValue::Detailed(spec) => spec.git_ref.as_deref(),
        }
    }
}

/// Registry value: simple URL string or detailed spec with scope routing
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum RegistryValue {
    /// Simple URL: `central = "https://repo1.maven.org/maven2"`
    Simple(String),
    /// Detailed: `{ url = "https://...", scope = "com.mycompany.*" }`
    Detailed(RegistrySpec),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RegistrySpec {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Script value — either a simple command string or a detailed spec with timeout
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum ScriptValue {
    /// Simple: `build = "ymc build"`
    Simple(String),
    /// Detailed: `test = { command = "ymc test", timeout = "5m" }`
    Detailed(ScriptSpec),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScriptSpec {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

impl ScriptValue {
    pub fn command(&self) -> &str {
        match self {
            ScriptValue::Simple(s) => s,
            ScriptValue::Detailed(spec) => &spec.command,
        }
    }

    pub fn timeout_secs(&self) -> Option<u64> {
        match self {
            ScriptValue::Simple(_) => None,
            ScriptValue::Detailed(spec) => spec.timeout.as_ref().and_then(|t| parse_duration_secs(t)),
        }
    }
}

/// Parse human-readable duration: "5m" → 300, "30s" → 30, "1h" → 3600
fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(minutes) = s.strip_suffix('m') {
        minutes.parse::<u64>().ok().map(|m| m * 60)
    } else if let Some(seconds) = s.strip_suffix('s') {
        seconds.parse::<u64>().ok()
    } else if let Some(hours) = s.strip_suffix('h') {
        hours.parse::<u64>().ok().map(|h| h * 3600)
    } else {
        s.parse::<u64>().ok() // raw seconds
    }
}

impl std::fmt::Display for DependencyValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DependencyValue::Simple(v) => write!(f, "{}", v),
            DependencyValue::Detailed(spec) => {
                if let Some(ref v) = spec.version {
                    write!(f, "{}", v)?;
                    if let Some(ref s) = spec.scope {
                        write!(f, " ({})", s)?;
                    }
                } else if spec.workspace.unwrap_or(false) {
                    write!(f, "workspace")?;
                } else {
                    write!(f, "*")?;
                }
                Ok(())
            }
        }
    }
}

impl std::fmt::Display for RegistryValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryValue::Simple(url) => write!(f, "{}", url),
            RegistryValue::Detailed(spec) => write!(f, "{}", spec.url),
        }
    }
}

impl RegistryValue {
    pub fn url(&self) -> &str {
        match self {
            RegistryValue::Simple(url) => url,
            RegistryValue::Detailed(spec) => &spec.url,
        }
    }

    pub fn scope(&self) -> Option<&str> {
        match self {
            RegistryValue::Simple(_) => None,
            RegistryValue::Detailed(spec) => spec.scope.as_deref(),
        }
    }
}

/// Main package.toml configuration
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct YmConfig {
    pub name: String,

    /// Maven groupId (required, default "com.example")
    #[serde(default = "default_group_id")]
    pub group_id: String,

    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_version_or_workspace",
        default
    )]
    pub version: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Java target version (e.g., "21" or 21)
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_string_or_int",
        default
    )]
    pub target: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,

    /// Main class (fully qualified)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,

    /// Base package name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,

    /// Environment variables for scripts
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,

    /// Unified dependencies with scope support
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<BTreeMap<String, DependencyValue>>,

    /// Workspace glob patterns
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Vec<String>>,

    /// JVM arguments for runtime
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jvm_args: Option<Vec<String>>,

    /// Named scripts (ym <script-name> → executes command)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scripts: Option<BTreeMap<String, ScriptValue>>,

    /// Version overrides (always win over any other version)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolutions: Option<BTreeMap<String, String>>,

    /// Maven registries
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registries: Option<BTreeMap<String, RegistryValue>>,

    /// JVM/JDK configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jvm: Option<JvmConfig>,

    /// Compiler configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compiler: Option<CompilerConfig>,

    /// Hot reload configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hot_reload: Option<HotReloadConfig>,

    /// Global transitive dependency exclusions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclusions: Option<Vec<String>>,

    /// Custom source directory (default: src/main/java → fallback src/)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_dir: Option<String>,

    /// Custom test directory (default: src/test/java → fallback test/)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_dir: Option<String>,

    /// Native image configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeConfig>,
}

fn default_group_id() -> String {
    "com.example".to_string()
}

impl YmConfig {
    /// Extract Maven dependencies as BTreeMap<coordinate, version>.
    /// Filters out workspace module refs (keys without ':').
    /// Resolves DependencyValue to plain version strings.
    pub fn maven_dependencies(&self) -> BTreeMap<String, String> {
        let mut result = BTreeMap::new();
        if let Some(ref deps) = self.dependencies {
            for (key, value) in deps {
                // Skip workspace module refs (no colon = module name)
                if !key.contains(':') {
                    continue;
                }
                // Skip { workspace = true } entries that inherit version from root
                if value.is_workspace() {
                    continue;
                }
                // Skip URL and Git dependencies (handled separately)
                if value.url().is_some() || value.git().is_some() {
                    continue;
                }
                if let Some(version) = value.version() {
                    // Encode classifier into key: "g:a:classifier" for MavenCoord::parse
                    let coord_key = if let Some(classifier) = value.classifier() {
                        format!("{}:{}", key, classifier)
                    } else {
                        key.clone()
                    };
                    result.insert(coord_key, version.to_string());
                }
            }
        }
        result
    }

    /// Extract workspace module dependency names.
    /// Returns names of local workspace modules this package depends on.
    pub fn workspace_module_deps(&self) -> Vec<String> {
        let mut result = Vec::new();
        if let Some(ref deps) = self.dependencies {
            for (key, value) in deps {
                // No colon = workspace module name
                if !key.contains(':') && value.is_workspace() {
                    result.push(key.clone());
                }
            }
        }
        result
    }

    /// Extract Maven dependencies, resolving `{ workspace = true }` entries
    /// by inheriting version from the root config's `[dependencies]`.
    /// Root deps are NOT auto-inherited — child must explicitly declare.
    pub fn maven_dependencies_with_root(&self, root: &YmConfig) -> BTreeMap<String, String> {
        let mut result = BTreeMap::new();
        if let Some(ref deps) = self.dependencies {
            for (key, value) in deps {
                if !key.contains(':') {
                    continue;
                }
                if value.url().is_some() || value.git().is_some() {
                    continue;
                }
                if value.is_workspace() {
                    // Inherit version from root
                    if let Some(ref root_deps) = root.dependencies {
                        if let Some(root_value) = root_deps.get(key) {
                            if let Some(version) = root_value.version() {
                                result.insert(key.clone(), version.to_string());
                            }
                        }
                    }
                    continue;
                }
                if let Some(version) = value.version() {
                    result.insert(key.clone(), version.to_string());
                }
            }
        }
        result
    }

    /// Extract Maven dependencies filtered by allowed scopes.
    /// Returns BTreeMap<coordinate, version> for dependencies matching any of the given scopes.
    pub fn maven_dependencies_for_scopes(&self, scopes: &[&str]) -> BTreeMap<String, String> {
        let mut result = BTreeMap::new();
        if let Some(ref deps) = self.dependencies {
            for (key, value) in deps {
                if !key.contains(':') {
                    continue;
                }
                if value.url().is_some() || value.git().is_some() {
                    continue;
                }
                if value.is_workspace() {
                    continue;
                }
                let dep_scope = value.scope();
                if scopes.contains(&dep_scope) {
                    if let Some(version) = value.version() {
                        let coord_key = if let Some(classifier) = value.classifier() {
                            format!("{}:{}", key, classifier)
                        } else {
                            key.clone()
                        };
                        result.insert(coord_key, version.to_string());
                    }
                }
            }
        }
        result
    }

    /// Extract Maven dependencies filtered by scope, resolving `{ workspace = true }`
    /// entries by inheriting version from root config but using child's own scope.
    pub fn maven_dependencies_for_scopes_with_root(&self, scopes: &[&str], root: &YmConfig) -> BTreeMap<String, String> {
        let mut result = BTreeMap::new();
        if let Some(ref deps) = self.dependencies {
            for (key, value) in deps {
                if !key.contains(':') {
                    continue;
                }
                if value.url().is_some() || value.git().is_some() {
                    continue;
                }
                if value.is_workspace() {
                    // Use child's scope, inherit version from root
                    let dep_scope = value.scope();
                    if scopes.contains(&dep_scope) {
                        if let Some(ref root_deps) = root.dependencies {
                            if let Some(root_value) = root_deps.get(key) {
                                if let Some(version) = root_value.version() {
                                    result.insert(key.clone(), version.to_string());
                                }
                            }
                        }
                    }
                    continue;
                }
                let dep_scope = value.scope();
                if scopes.contains(&dep_scope) {
                    if let Some(version) = value.version() {
                        result.insert(key.clone(), version.to_string());
                    }
                }
            }
        }
        result
    }

    /// Extract URL dependencies: key → (url, scope)
    pub fn url_dependencies(&self) -> Vec<(String, String, String)> {
        let mut result = Vec::new();
        if let Some(ref deps) = self.dependencies {
            for (key, value) in deps {
                if let Some(url) = value.url() {
                    result.push((key.clone(), url.to_string(), value.scope().to_string()));
                }
            }
        }
        result
    }

    /// Extract Git dependencies: key → (git_url, git_ref, scope)
    pub fn git_dependencies(&self) -> Vec<(String, String, Option<String>, String)> {
        let mut result = Vec::new();
        if let Some(ref deps) = self.dependencies {
            for (key, value) in deps {
                if let Some(git) = value.git() {
                    result.push((
                        key.clone(),
                        git.to_string(),
                        value.git_ref().map(|s| s.to_string()),
                        value.scope().to_string(),
                    ));
                }
            }
        }
        result
    }

    /// Validate dependency declarations in a workspace child module.
    /// Returns errors for:
    /// - `{ workspace = true }` Maven dep (has colon) not found in root
    /// - Non-colon key without `workspace = true` (must be explicit module ref)
    pub fn validate_workspace_deps(&self, root: &YmConfig) -> Vec<String> {
        let mut errors = Vec::new();
        if let Some(ref deps) = self.dependencies {
            for (key, value) in deps {
                if key.contains(':') {
                    if value.is_workspace() {
                        let found = root.dependencies.as_ref()
                            .and_then(|d| d.get(key))
                            .and_then(|v| v.version())
                            .is_some();
                        if !found {
                            errors.push(format!(
                                "Dependency '{}' uses {{ workspace = true }} but root package.toml has no version for it",
                                key
                            ));
                        }
                    }
                } else if !value.is_workspace() {
                    errors.push(format!(
                        "Dependency '{}' has no colon (not a Maven coordinate) and no {{ workspace = true }} — must be a workspace module reference",
                        key
                    ));
                }
            }
        }
        errors
    }

    /// Collect all per-dependency exclude entries.
    /// Returns a Vec of "groupId:artifactId" strings that should be excluded from transitive deps.
    pub fn per_dependency_exclusions(&self) -> Vec<String> {
        let mut result = Vec::new();
        if let Some(ref deps) = self.dependencies {
            for value in deps.values() {
                if let DependencyValue::Detailed(spec) = value {
                    if let Some(ref excludes) = spec.exclude {
                        result.extend(excludes.iter().cloned());
                    }
                }
            }
        }
        result
    }

    /// Compute a fingerprint of dependency-relevant config fields.
    /// Used to detect when resolved.json should be invalidated.
    pub fn dependency_fingerprint(&self) -> String {
        use std::fmt::Write;
        let mut data = String::new();

        // [dependencies]
        if let Some(ref deps) = self.dependencies {
            for (k, v) in deps {
                let _ = writeln!(data, "dep:{}={}", k, v);
            }
        }

        // [resolutions]
        if let Some(ref res) = self.resolutions {
            for (k, v) in res {
                let _ = writeln!(data, "res:{}={}", k, v);
            }
        }

        // exclusions
        if let Some(ref exc) = self.exclusions {
            for e in exc {
                let _ = writeln!(data, "exc:{}", e);
            }
        }

        // [registries]
        if let Some(ref regs) = self.registries {
            for (k, v) in regs {
                let _ = writeln!(data, "reg:{}={}", k, v);
            }
        }

        crate::compiler::incremental::hash_bytes(data.as_bytes())
    }

    /// Get registry entries with scope routing info
    pub fn registry_entries(&self) -> Vec<crate::workspace::resolver::RegistryEntry> {
        let mut entries = Vec::new();
        if let Some(ref registries) = self.registries {
            for value in registries.values() {
                entries.push(crate::workspace::resolver::RegistryEntry {
                    url: value.url().to_string(),
                    scope: value.scope().map(|s| s.to_string()),
                });
            }
        }
        entries
    }
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
    pub encoding: Option<String>,
    /// Annotation processor dependencies (groupId:artifactId format)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation_processors: Option<Vec<String>>,
    /// Javac lint options (e.g., ["all", "-serial"])
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lint: Option<Vec<String>>,
    /// Additional compiler arguments passed directly to javac
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Resource file extensions to copy (replaces default list)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_extensions: Option<Vec<String>>,
    /// Regex patterns to exclude resource files (matched against relative path)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_exclude: Option<Vec<String>>,
    /// JaCoCo version for code coverage
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jacoco_version: Option<String>,
    /// Local JAR directories to add to classpath (e.g., ["sdk/lib"])
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libs: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HotReloadConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch_extensions: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct NativeConfig {
    /// Extra args passed to native-image
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Docker image override (default: ghcr.io/graalvm/native-image-community:{target})
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docker_image: Option<String>,
}

/// Internal resolved dependency cache (.ym/resolved.json)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResolvedCache {
    pub version: u32,
    /// SHA-256 hash of dependency-relevant config fields for cache invalidation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_hash: Option<String>,
    pub dependencies: BTreeMap<String, ResolvedDependency>,
}

impl Default for ResolvedCache {
    fn default() -> Self {
        Self {
            version: 1,
            config_hash: None,
            dependencies: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResolvedDependency {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<Vec<String>>,
    /// Effective scope after transitive propagation (compile/runtime/provided/test)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Deserializer that accepts both string and integer for the `target` field.
/// TOML allows `target = 21` (integer) or `target = "21"` (string).
fn deserialize_string_or_int<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de;

    struct StringOrInt;

    impl<'de> de::Visitor<'de> for StringOrInt {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string, integer, or { workspace = true }")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            // { workspace = true } → None (inherited from root)
            while let Some((key, _value)) = map.next_entry::<String, toml::Value>()? {
                let _ = key;
            }
            Ok(None)
        }

        fn visit_some<D2>(self, deserializer: D2) -> Result<Self::Value, D2::Error>
        where
            D2: Deserializer<'de>,
        {
            deserializer.deserialize_any(StringOrInt)
        }
    }

    deserializer.deserialize_any(StringOrInt)
}

/// Deserialize version field: accepts a string or { workspace = true } (returns None).
fn deserialize_version_or_workspace<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de;

    struct VersionOrWorkspace;

    impl<'de> de::Visitor<'de> for VersionOrWorkspace {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a version string or { workspace = true }")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            while let Some((key, _value)) = map.next_entry::<String, toml::Value>()? {
                let _ = key;
            }
            Ok(None)
        }

        fn visit_some<D2>(self, deserializer: D2) -> Result<Self::Value, D2::Error>
        where
            D2: Deserializer<'de>,
        {
            deserializer.deserialize_any(VersionOrWorkspace)
        }
    }

    deserializer.deserialize_any(VersionOrWorkspace)
}
