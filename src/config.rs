// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use directories::ProjectDirs;
use regex::Regex;
use serde::de::{Deserialize, DeserializeOwned, Deserializer};
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
use serde::ser::{Serialize, Serializer};
#[cfg(test)]
use serial_test::serial;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::result::Result as StdResult;
use std::str::FromStr;
use std::sync::Mutex;

use crate::errors::*;

lazy_static! {
    static ref CACHED_CONFIG_PATH: PathBuf = CachedConfig::file_config_path();
    static ref CACHED_CONFIG: Mutex<Option<CachedFileConfig>> = Mutex::new(None);
}

const ORGANIZATION: &str = "Mozilla";
const APP_NAME: &str = "sccache";
const DIST_APP_NAME: &str = "sccache-dist-client";
const TEN_GIGS: u64 = 10 * 1024 * 1024 * 1024;

const MOZILLA_OAUTH_PKCE_CLIENT_ID: &str = "F1VVD6nRTckSVrviMRaOdLBWIk1AvHYo";
// The sccache audience is an API set up in auth0 for sccache to allow 7 day expiry,
// the openid scope allows us to query the auth0 /userinfo endpoint which contains
// group information due to Mozilla rules.
const MOZILLA_OAUTH_PKCE_AUTH_URL: &str =
    "https://auth.mozilla.auth0.com/authorize?audience=sccache&scope=openid%20profile";
const MOZILLA_OAUTH_PKCE_TOKEN_URL: &str = "https://auth.mozilla.auth0.com/oauth/token";

pub const INSECURE_DIST_CLIENT_TOKEN: &str = "dangerously_insecure_client";

// Unfortunately this means that nothing else can use the sccache cache dir as
// this top level directory is used directly to store sccache cached objects...
pub fn default_disk_cache_dir() -> PathBuf {
    ProjectDirs::from("", ORGANIZATION, APP_NAME)
        .expect("Unable to retrieve disk cache directory")
        .cache_dir()
        .to_owned()
}
// ...whereas subdirectories are used of this one
pub fn default_dist_cache_dir() -> PathBuf {
    ProjectDirs::from("", ORGANIZATION, DIST_APP_NAME)
        .expect("Unable to retrieve dist cache directory")
        .cache_dir()
        .to_owned()
}

fn default_disk_cache_size() -> u64 {
    TEN_GIGS
}
fn default_toolchain_cache_size() -> u64 {
    TEN_GIGS
}

pub fn parse_size(val: &str) -> Option<u64> {
    let re = Regex::new(r"^(\d+)([KMGT])$").expect("Fixed regex parse failure");
    re.captures(val)
        .and_then(|caps| {
            caps.get(1)
                .and_then(|size| u64::from_str(size.as_str()).ok())
                .map(|size| (size, caps.get(2)))
        })
        .and_then(|(size, suffix)| match suffix.map(|s| s.as_str()) {
            Some("K") => Some(1024 * size),
            Some("M") => Some(1024 * 1024 * size),
            Some("G") => Some(1024 * 1024 * 1024 * size),
            Some("T") => Some(1024 * 1024 * 1024 * 1024 * size),
            _ => None,
        })
}

#[cfg(any(feature = "dist-client", feature = "dist-server"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HTTPUrl(reqwest::Url);
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
impl Serialize for HTTPUrl {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
impl<'a> Deserialize<'a> for HTTPUrl {
    fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
    where
        D: Deserializer<'a>,
    {
        use serde::de::Error;
        let helper: String = Deserialize::deserialize(deserializer)?;
        let url = parse_http_url(&helper).map_err(D::Error::custom)?;
        Ok(HTTPUrl(url))
    }
}
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
fn parse_http_url(url: &str) -> Result<reqwest::Url> {
    use std::net::SocketAddr;
    let url = if let Ok(sa) = url.parse::<SocketAddr>() {
        warn!("Url {} has no scheme, assuming http", url);
        reqwest::Url::parse(&format!("http://{}", sa))
    } else {
        reqwest::Url::parse(url)
    }?;
    if url.scheme() != "http" && url.scheme() != "https" {
        bail!("url not http or https")
    }
    // TODO: relative url handling just hasn't been implemented and tested
    if url.path() != "/" {
        bail!("url has a relative path (currently unsupported)")
    }
    Ok(url)
}
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
impl HTTPUrl {
    pub fn from_url(u: reqwest::Url) -> Self {
        HTTPUrl(u)
    }
    pub fn to_url(&self) -> reqwest::Url {
        self.0.clone()
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AzureCacheConfig {
    pub connection_string: String,
    pub container: String,
    pub key_prefix: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct DiskCacheConfig {
    pub dir: PathBuf,
    // TODO: use deserialize_with to allow human-readable sizes in toml
    pub size: u64,
}

impl Default for DiskCacheConfig {
    fn default() -> Self {
        DiskCacheConfig {
            dir: default_disk_cache_dir(),
            size: default_disk_cache_size(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum GCSCacheRWMode {
    #[serde(rename = "READ_ONLY")]
    ReadOnly,
    #[serde(rename = "READ_WRITE")]
    ReadWrite,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GCSCacheConfig {
    pub bucket: String,
    pub key_prefix: String,
    pub cred_path: Option<String>,
    pub service_account: Option<String>,
    pub rw_mode: GCSCacheRWMode,
    pub credential_url: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GHACacheConfig {
    /// Version for gha cache is a namespace. By setting different versions,
    /// we can avoid mixed caches.
    pub version: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemcachedCacheConfig {
    pub url: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisCacheConfig {
    pub url: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3CacheConfig {
    pub bucket: String,
    pub region: Option<String>,
    pub key_prefix: String,
    pub no_credentials: bool,
    pub endpoint: Option<String>,
    pub use_ssl: Option<bool>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CacheType {
    Azure(AzureCacheConfig),
    GCS(GCSCacheConfig),
    GHA(GHACacheConfig),
    Memcached(MemcachedCacheConfig),
    Redis(RedisCacheConfig),
    S3(S3CacheConfig),
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfigs {
    pub azure: Option<AzureCacheConfig>,
    pub disk: Option<DiskCacheConfig>,
    pub gcs: Option<GCSCacheConfig>,
    pub gha: Option<GHACacheConfig>,
    pub memcached: Option<MemcachedCacheConfig>,
    pub redis: Option<RedisCacheConfig>,
    pub s3: Option<S3CacheConfig>,
}

impl CacheConfigs {
    /// Return cache type in an arbitrary but
    /// consistent ordering
    fn into_fallback(self) -> (Option<CacheType>, DiskCacheConfig) {
        let CacheConfigs {
            azure,
            disk,
            gcs,
            gha,
            memcached,
            redis,
            s3,
        } = self;

        let cache_type = s3
            .map(CacheType::S3)
            .or_else(|| redis.map(CacheType::Redis))
            .or_else(|| memcached.map(CacheType::Memcached))
            .or_else(|| gcs.map(CacheType::GCS))
            .or_else(|| gha.map(CacheType::GHA))
            .or_else(|| azure.map(CacheType::Azure));

        let fallback = disk.unwrap_or_default();

        (cache_type, fallback)
    }

    /// Override self with any existing fields from other
    fn merge(&mut self, other: Self) {
        let CacheConfigs {
            azure,
            disk,
            gcs,
            gha,
            memcached,
            redis,
            s3,
        } = other;

        if azure.is_some() {
            self.azure = azure
        }
        if disk.is_some() {
            self.disk = disk
        }
        if gcs.is_some() {
            self.gcs = gcs
        }
        if gha.is_some() {
            self.gha = gha
        }
        if memcached.is_some() {
            self.memcached = memcached
        }
        if redis.is_some() {
            self.redis = redis
        }
        if s3.is_some() {
            self.s3 = s3
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "type")]
pub enum DistToolchainConfig {
    #[serde(rename = "no_dist")]
    NoDist { compiler_executable: PathBuf },
    #[serde(rename = "path_override")]
    PathOverride {
        compiler_executable: PathBuf,
        archive: PathBuf,
        archive_compiler_executable: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum DistAuth {
    #[serde(rename = "token")]
    Token { token: String },
    #[serde(rename = "oauth2_code_grant_pkce")]
    Oauth2CodeGrantPKCE {
        client_id: String,
        auth_url: String,
        token_url: String,
    },
    #[serde(rename = "oauth2_implicit")]
    Oauth2Implicit { client_id: String, auth_url: String },
}

// Convert a type = "mozilla" immediately into an actual oauth configuration
// https://github.com/serde-rs/serde/issues/595 could help if implemented
impl<'a> Deserialize<'a> for DistAuth {
    fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
    where
        D: Deserializer<'a>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        #[serde(tag = "type")]
        pub enum Helper {
            #[serde(rename = "token")]
            Token { token: String },
            #[serde(rename = "mozilla")]
            Mozilla,
            #[serde(rename = "oauth2_code_grant_pkce")]
            Oauth2CodeGrantPKCE {
                client_id: String,
                auth_url: String,
                token_url: String,
            },
            #[serde(rename = "oauth2_implicit")]
            Oauth2Implicit { client_id: String, auth_url: String },
        }

        let helper: Helper = Deserialize::deserialize(deserializer)?;

        Ok(match helper {
            Helper::Token { token } => DistAuth::Token { token },
            Helper::Mozilla => DistAuth::Oauth2CodeGrantPKCE {
                client_id: MOZILLA_OAUTH_PKCE_CLIENT_ID.to_owned(),
                auth_url: MOZILLA_OAUTH_PKCE_AUTH_URL.to_owned(),
                token_url: MOZILLA_OAUTH_PKCE_TOKEN_URL.to_owned(),
            },
            Helper::Oauth2CodeGrantPKCE {
                client_id,
                auth_url,
                token_url,
            } => DistAuth::Oauth2CodeGrantPKCE {
                client_id,
                auth_url,
                token_url,
            },
            Helper::Oauth2Implicit {
                client_id,
                auth_url,
            } => DistAuth::Oauth2Implicit {
                client_id,
                auth_url,
            },
        })
    }
}

impl Default for DistAuth {
    fn default() -> Self {
        DistAuth::Token {
            token: INSECURE_DIST_CLIENT_TOKEN.to_owned(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct DistConfig {
    pub auth: DistAuth,
    #[cfg(any(feature = "dist-client", feature = "dist-server"))]
    pub scheduler_url: Option<HTTPUrl>,
    #[cfg(not(any(feature = "dist-client", feature = "dist-server")))]
    pub scheduler_url: Option<String>,
    pub cache_dir: PathBuf,
    pub toolchains: Vec<DistToolchainConfig>,
    pub toolchain_cache_size: u64,
    pub rewrite_includes_only: bool,
}

impl Default for DistConfig {
    fn default() -> Self {
        Self {
            auth: Default::default(),
            scheduler_url: Default::default(),
            cache_dir: default_dist_cache_dir(),
            toolchains: Default::default(),
            toolchain_cache_size: default_toolchain_cache_size(),
            rewrite_includes_only: false,
        }
    }
}

// TODO: fields only pub for tests
#[derive(Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub cache: CacheConfigs,
    pub dist: DistConfig,
    pub server_startup_timeout_ms: Option<u64>,
}

// If the file doesn't exist or we can't read it, log the issue and proceed. If the
// config exists but doesn't parse then something is wrong - return an error.
pub fn try_read_config_file<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    debug!("Attempting to read config file at {:?}", path);
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            debug!("Couldn't open config file: {}", e);
            return Ok(None);
        }
    };

    let mut string = String::new();
    match file.read_to_string(&mut string) {
        Ok(_) => (),
        Err(e) => {
            warn!("Failed to read config file: {}", e);
            return Ok(None);
        }
    }

    let res = if path.extension().map_or(false, |e| e == "json") {
        serde_json::from_str(&string)
            .with_context(|| format!("Failed to load json config file from {}", path.display()))?
    } else {
        toml::from_str(&string)
            .with_context(|| format!("Failed to load toml config file from {}", path.display()))?
    };

    Ok(Some(res))
}

#[derive(Debug)]
pub struct EnvConfig {
    cache: CacheConfigs,
}

fn config_from_env() -> Result<EnvConfig> {
    // ======= AWS =======
    let s3 = env::var("SCCACHE_BUCKET").ok().map(|bucket| {
        let region = env::var("SCCACHE_REGION").ok();
        let no_credentials = env::var("SCCACHE_S3_NO_CREDENTIALS").ok().is_some();
        let use_ssl = env::var("SCCACHE_S3_USE_SSL")
            .ok()
            .map(|value| value != "off");
        let endpoint = env::var("SCCACHE_ENDPOINT").ok();
        let key_prefix = env::var("SCCACHE_S3_KEY_PREFIX")
            .ok()
            .as_ref()
            .map(|s| s.trim_end_matches('/'))
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned() + "/")
            .unwrap_or_default();

        S3CacheConfig {
            bucket,
            region,
            no_credentials,
            key_prefix,
            endpoint,
            use_ssl,
        }
    });
    if s3.as_ref().map(|s3| s3.no_credentials).unwrap_or_default()
        && (env::var_os("AWS_ACCESS_KEY_ID").is_some()
            || env::var_os("AWS_SECRET_ACCESS_KEY").is_some())
    {
        bail!("If setting S3 credentials, SCCACHE_S3_NO_CREDENTIALS must not be set.");
    }

    // ======= redis =======
    let redis = env::var("SCCACHE_REDIS")
        .ok()
        .map(|url| RedisCacheConfig { url });

    // ======= memcached =======
    let memcached = env::var("SCCACHE_MEMCACHED")
        .ok()
        .map(|url| MemcachedCacheConfig { url });

    // ======= GCP/GCS =======
    if (env::var("SCCACHE_GCS_CREDENTIALS_URL").is_ok()
        || env::var("SCCACHE_GCS_OAUTH_URL").is_ok()
        || env::var("SCCACHE_GCS_KEY_PATH").is_ok())
        && env::var("SCCACHE_GCS_BUCKET").is_err()
    {
        bail!(
            "If setting GCS credentials, SCCACHE_GCS_BUCKET and an auth mechanism need to be set."
        );
    }

    let gcs = env::var("SCCACHE_GCS_BUCKET").ok().map(|bucket| {
        let key_prefix = env::var("SCCACHE_GCS_KEY_PREFIX")
            .ok()
            .as_ref()
            .map(|s| s.trim_end_matches('/'))
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
            .to_owned();



        if env::var("SCCACHE_GCS_OAUTH_URL").is_ok() {
            warn!("SCCACHE_GCS_OAUTH_URL has been deprecated");
            warn!("if you intend to use vm metadata for auth, please set correct service account intead");
        }

        let credential_url = env::var("SCCACHE_GCS_CREDENTIALS_URL").ok();

        let cred_path = env::var("SCCACHE_GCS_KEY_PATH").ok();
        let service_account = env::var("SCCACHE_GCS_SERVICE_ACCOUNT").ok();

        let rw_mode = match env::var("SCCACHE_GCS_RW_MODE").as_ref().map(String::as_str) {
            Ok("READ_ONLY") => GCSCacheRWMode::ReadOnly,
            Ok("READ_WRITE") => GCSCacheRWMode::ReadWrite,
            // TODO: unsure if these should warn during the configuration loading
            // or at the time when they're actually used to connect to GCS
            Ok(_) => {
                warn!("Invalid SCCACHE_GCS_RW_MODE-- defaulting to READ_ONLY.");
                GCSCacheRWMode::ReadOnly
            }
            _ => {
                warn!("No SCCACHE_GCS_RW_MODE specified-- defaulting to READ_ONLY.");
                GCSCacheRWMode::ReadOnly
            }
        };

        GCSCacheConfig {
            bucket,
            key_prefix,
            cred_path,
            service_account,
            rw_mode,
            credential_url,
        }
    });

    // ======= GHA =======
    let gha = if let Ok(version) = env::var("SCCACHE_GHA_VERSION") {
        Some(GHACacheConfig { version })
    } else {
        None
    };

    // ======= Azure =======
    let azure = if let (Ok(connection_string), Ok(container)) = (
        env::var("SCCACHE_AZURE_CONNECTION_STRING"),
        env::var("SCCACHE_AZURE_BLOB_CONTAINER"),
    ) {
        let key_prefix = env::var("SCCACHE_AZURE_KEY_PREFIX")
            .ok()
            .as_ref()
            .map(|s| s.trim_end_matches('/'))
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
            .to_owned();
        Some(AzureCacheConfig {
            connection_string,
            container,
            key_prefix,
        })
    } else {
        None
    };

    // ======= Local =======
    let disk_dir = env::var_os("SCCACHE_DIR").map(PathBuf::from);
    let disk_sz = env::var("SCCACHE_CACHE_SIZE")
        .ok()
        .and_then(|v| parse_size(&v));

    let disk = if disk_dir.is_some() || disk_sz.is_some() {
        Some(DiskCacheConfig {
            dir: disk_dir.unwrap_or_else(default_disk_cache_dir),
            size: disk_sz.unwrap_or_else(default_disk_cache_size),
        })
    } else {
        None
    };

    let cache = CacheConfigs {
        azure,
        disk,
        gcs,
        gha,
        memcached,
        redis,
        s3,
    };

    Ok(EnvConfig { cache })
}

// The directories crate changed the location of `config_dir` on macos in version 3,
// so we also check the config in `preference_dir` (new in that version), which
// corresponds to the old location, for compatibility with older setups.
fn config_file(env_var: &str, leaf: &str) -> PathBuf {
    if let Some(env_value) = env::var_os(env_var) {
        return env_value.into();
    }
    let dirs =
        ProjectDirs::from("", ORGANIZATION, APP_NAME).expect("Unable to get config directory");
    // If the new location exists, use that.
    let path = dirs.config_dir().join(leaf);
    if path.exists() {
        return path;
    }
    // If the old location exists, use that.
    let path = dirs.preference_dir().join(leaf);
    if path.exists() {
        return path;
    }
    // Otherwise, use the new location.
    dirs.config_dir().join(leaf)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub cache: Option<CacheType>,
    pub fallback_cache: DiskCacheConfig,
    pub dist: DistConfig,
    pub server_startup_timeout: Option<std::time::Duration>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let env_conf = config_from_env()?;

        let file_conf_path = config_file("SCCACHE_CONF", "config");
        let file_conf = try_read_config_file(&file_conf_path)
            .context("Failed to load config file")?
            .unwrap_or_default();

        Ok(Self::from_env_and_file_configs(env_conf, file_conf))
    }

    fn from_env_and_file_configs(env_conf: EnvConfig, file_conf: FileConfig) -> Self {
        let mut conf_caches: CacheConfigs = Default::default();

        let FileConfig {
            cache,
            dist,
            server_startup_timeout_ms,
        } = file_conf;
        conf_caches.merge(cache);

        let server_startup_timeout =
            server_startup_timeout_ms.map(std::time::Duration::from_millis);

        let EnvConfig { cache } = env_conf;
        conf_caches.merge(cache);

        let (caches, fallback_cache) = conf_caches.into_fallback();
        Self {
            cache: caches,
            fallback_cache,
            dist,
            server_startup_timeout,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct CachedDistConfig {
    pub auth_tokens: HashMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct CachedFileConfig {
    pub dist: CachedDistConfig,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CachedConfig(());

impl CachedConfig {
    pub fn load() -> Result<Self> {
        let mut cached_file_config = CACHED_CONFIG.lock().unwrap();

        if cached_file_config.is_none() {
            let cfg = Self::load_file_config().context("Unable to initialise cached config")?;
            *cached_file_config = Some(cfg)
        }
        Ok(CachedConfig(()))
    }
    pub fn reload() -> Result<Self> {
        {
            let mut cached_file_config = CACHED_CONFIG.lock().unwrap();
            *cached_file_config = None;
        };
        Self::load()
    }
    pub fn with<F: FnOnce(&CachedFileConfig) -> T, T>(&self, f: F) -> T {
        let cached_file_config = CACHED_CONFIG.lock().unwrap();
        let cached_file_config = cached_file_config.as_ref().unwrap();

        f(cached_file_config)
    }
    pub fn with_mut<F: FnOnce(&mut CachedFileConfig)>(&self, f: F) -> Result<()> {
        let mut cached_file_config = CACHED_CONFIG.lock().unwrap();
        let cached_file_config = cached_file_config.as_mut().unwrap();

        let mut new_config = cached_file_config.clone();
        f(&mut new_config);
        Self::save_file_config(&new_config)?;
        *cached_file_config = new_config;
        Ok(())
    }

    fn file_config_path() -> PathBuf {
        config_file("SCCACHE_CACHED_CONF", "cached-config")
    }
    fn load_file_config() -> Result<CachedFileConfig> {
        let file_conf_path = &*CACHED_CONFIG_PATH;

        if !file_conf_path.exists() {
            let file_conf_dir = file_conf_path
                .parent()
                .expect("Cached conf file has no parent directory");
            if !file_conf_dir.is_dir() {
                fs::create_dir_all(file_conf_dir)
                    .context("Failed to create dir to hold cached config")?
            }
            Self::save_file_config(&Default::default()).with_context(|| {
                format!(
                    "Unable to create cached config file at {}",
                    file_conf_path.display()
                )
            })?
        }
        try_read_config_file(file_conf_path)
            .context("Failed to load cached config file")?
            .with_context(|| format!("Failed to load from {}", file_conf_path.display()))
    }
    fn save_file_config(c: &CachedFileConfig) -> Result<()> {
        let file_conf_path = &*CACHED_CONFIG_PATH;
        let mut file = File::create(file_conf_path).context("Could not open config for writing")?;
        file.write_all(&toml::to_vec(c).unwrap())
            .map_err(Into::into)
    }
}

#[cfg(feature = "dist-server")]
pub mod scheduler {
    use std::net::SocketAddr;
    use std::path::Path;

    use crate::errors::*;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type")]
    #[serde(deny_unknown_fields)]
    pub enum ClientAuth {
        #[serde(rename = "DANGEROUSLY_INSECURE")]
        Insecure,
        #[serde(rename = "token")]
        Token { token: String },
        #[serde(rename = "jwt_validate")]
        JwtValidate {
            audience: String,
            issuer: String,
            jwks_url: String,
        },
        #[serde(rename = "mozilla")]
        Mozilla { required_groups: Vec<String> },
        #[serde(rename = "proxy_token")]
        ProxyToken {
            url: String,
            cache_secs: Option<u64>,
        },
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type")]
    #[serde(deny_unknown_fields)]
    pub enum ServerAuth {
        #[serde(rename = "DANGEROUSLY_INSECURE")]
        Insecure,
        #[serde(rename = "jwt_hs256")]
        JwtHS256 { secret_key: String },
        #[serde(rename = "token")]
        Token { token: String },
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Config {
        pub public_addr: SocketAddr,
        pub client_auth: ClientAuth,
        pub server_auth: ServerAuth,
    }

    pub fn from_path(conf_path: &Path) -> Result<Option<Config>> {
        super::try_read_config_file(conf_path).context("Failed to load scheduler config file")
    }
}

#[cfg(feature = "dist-server")]
pub mod server {
    use super::HTTPUrl;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};

    use crate::errors::*;

    const TEN_GIGS: u64 = 10 * 1024 * 1024 * 1024;
    fn default_toolchain_cache_size() -> u64 {
        TEN_GIGS
    }

    const DEFAULT_POT_CLONE_FROM: &str = "sccache-template";
    const DEFAULT_POT_FS_ROOT: &str = "/opt/pot";
    const DEFAULT_POT_CMD: &str = "pot";
    const DEFAULT_POT_CLONE_ARGS: &[&str] = &["-i", "lo0|127.0.0.2"];

    fn default_pot_clone_from() -> String {
        DEFAULT_POT_CLONE_FROM.to_string()
    }

    fn default_pot_fs_root() -> PathBuf {
        DEFAULT_POT_FS_ROOT.into()
    }

    fn default_pot_cmd() -> PathBuf {
        DEFAULT_POT_CMD.into()
    }

    fn default_pot_clone_args() -> Vec<String> {
        DEFAULT_POT_CLONE_ARGS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type")]
    #[serde(deny_unknown_fields)]
    pub enum BuilderType {
        #[serde(rename = "docker")]
        Docker,
        #[serde(rename = "overlay")]
        Overlay {
            build_dir: PathBuf,
            bwrap_path: PathBuf,
        },
        #[serde(rename = "pot")]
        Pot {
            #[serde(default = "default_pot_fs_root")]
            pot_fs_root: PathBuf,
            #[serde(default = "default_pot_clone_from")]
            clone_from: String,
            #[serde(default = "default_pot_cmd")]
            pot_cmd: PathBuf,
            #[serde(default = "default_pot_clone_args")]
            pot_clone_args: Vec<String>,
        },
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type")]
    #[serde(deny_unknown_fields)]
    pub enum SchedulerAuth {
        #[serde(rename = "DANGEROUSLY_INSECURE")]
        Insecure,
        #[serde(rename = "jwt_token")]
        JwtToken { token: String },
        #[serde(rename = "token")]
        Token { token: String },
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Config {
        pub builder: BuilderType,
        pub cache_dir: PathBuf,
        pub public_addr: SocketAddr,
        pub scheduler_url: HTTPUrl,
        pub scheduler_auth: SchedulerAuth,
        #[serde(default = "default_toolchain_cache_size")]
        pub toolchain_cache_size: u64,
    }

    pub fn from_path(conf_path: &Path) -> Result<Option<Config>> {
        super::try_read_config_file(conf_path).context("Failed to load server config file")
    }
}

#[test]
fn test_parse_size() {
    assert_eq!(None, parse_size(""));
    assert_eq!(None, parse_size("100"));
    assert_eq!(Some(2048), parse_size("2K"));
    assert_eq!(Some(10 * 1024 * 1024), parse_size("10M"));
    assert_eq!(Some(TEN_GIGS), parse_size("10G"));
    assert_eq!(Some(1024 * TEN_GIGS), parse_size("10T"));
}

#[test]
fn config_overrides() {
    let env_conf = EnvConfig {
        cache: CacheConfigs {
            azure: Some(AzureCacheConfig {
                connection_string: String::new(),
                container: String::new(),
                key_prefix: String::new(),
            }),
            disk: Some(DiskCacheConfig {
                dir: "/env-cache".into(),
                size: 5,
            }),
            redis: Some(RedisCacheConfig {
                url: "myotherredisurl".to_owned(),
            }),
            ..Default::default()
        },
    };

    let file_conf = FileConfig {
        cache: CacheConfigs {
            disk: Some(DiskCacheConfig {
                dir: "/file-cache".into(),
                size: 15,
            }),
            memcached: Some(MemcachedCacheConfig {
                url: "memurl".to_owned(),
            }),
            redis: Some(RedisCacheConfig {
                url: "myredisurl".to_owned(),
            }),
            ..Default::default()
        },
        dist: Default::default(),
        server_startup_timeout_ms: None,
    };

    assert_eq!(
        Config::from_env_and_file_configs(env_conf, file_conf),
        Config {
            cache: Some(CacheType::Redis(RedisCacheConfig {
                url: "myotherredisurl".to_owned()
            }),),
            fallback_cache: DiskCacheConfig {
                dir: "/env-cache".into(),
                size: 5,
            },
            dist: Default::default(),
            server_startup_timeout: None,
        }
    );
}

#[test]
#[serial]
fn test_s3_no_credentials() {
    env::set_var("SCCACHE_S3_NO_CREDENTIALS", "1");
    env::set_var("SCCACHE_BUCKET", "my-bucket");
    env::set_var("AWS_ACCESS_KEY_ID", "aws-access-key-id");
    env::set_var("AWS_SECRET_ACCESS_KEY", "aws-secret-access-key");

    let error = config_from_env().unwrap_err();
    assert_eq!(
        "If setting S3 credentials, SCCACHE_S3_NO_CREDENTIALS must not be set.",
        error.to_string()
    );

    env::remove_var("SCCACHE_S3_NO_CREDENTIALS");
    env::remove_var("SCCACHE_BUCKET");
    env::remove_var("AWS_ACCESS_KEY_ID");
    env::remove_var("AWS_SECRET_ACCESS_KEY");
}

#[test]
fn test_gcs_service_account() {
    env::set_var("SCCACHE_GCS_BUCKET", "my-bucket");
    env::set_var("SCCACHE_GCS_SERVICE_ACCOUNT", "my@example.com");
    env::set_var("SCCACHE_GCS_RW_MODE", "READ_WRITE");

    let env_cfg = config_from_env().unwrap();
    match env_cfg.cache.gcs {
        Some(GCSCacheConfig {
            ref bucket,
            service_account,
            rw_mode,
            ..
        }) => {
            assert_eq!(bucket, "my-bucket");
            assert_eq!(service_account, Some("my@example.com".to_string()));
            assert_eq!(rw_mode, GCSCacheRWMode::ReadWrite);
        }
        None => unreachable!(),
    };

    env::remove_var("SCCACHE_GCS_BUCKET");
    env::remove_var("SCCACHE_GCS_SERVICE_ACCOUNT");
    env::remove_var("SCCACHE_GCS_RW_MODE");
}

#[test]
fn full_toml_parse() {
    const CONFIG_STR: &str = r#"
server_startup_timeout_ms = 10000

[dist]
# where to find the scheduler
scheduler_url = "http://1.2.3.4:10600"
# a set of prepackaged toolchains
toolchains = []
# the maximum size of the toolchain cache in bytes
toolchain_cache_size = 5368709120
cache_dir = "/home/user/.cache/sccache-dist-client"

[dist.auth]
type = "token"
token = "secrettoken"


#[cache.azure]
# does not work as it appears

[cache.disk]
dir = "/tmp/.cache/sccache"
size = 7516192768 # 7 GiBytes

[cache.gcs]
rw_mode = "READ_ONLY"
# rw_mode = "READ_WRITE"
cred_path = "/psst/secret/cred"
bucket = "bucket"
key_prefix = "prefix"
service_account = "example_service_account"

[cache.gha]
version = "sccache"

[cache.memcached]
url = "..."

[cache.redis]
url = "redis://user:passwd@1.2.3.4:6379/1"

[cache.s3]
bucket = "name"
region = "us-east-2"
endpoint = "s3-us-east-1.amazonaws.com"
use_ssl = true
key_prefix = "s3prefix"
no_credentials = true
"#;

    let file_config: FileConfig = toml::from_str(CONFIG_STR).expect("Is valid toml.");
    assert_eq!(
        file_config,
        FileConfig {
            cache: CacheConfigs {
                azure: None, // TODO not sure how to represent a unit struct in TOML Some(AzureCacheConfig),
                disk: Some(DiskCacheConfig {
                    dir: PathBuf::from("/tmp/.cache/sccache"),
                    size: 7 * 1024 * 1024 * 1024,
                }),
                gcs: Some(GCSCacheConfig {
                    bucket: "bucket".to_owned(),
                    cred_path: Some("/psst/secret/cred".to_string()),
                    service_account: Some("example_service_account".to_string()),
                    rw_mode: GCSCacheRWMode::ReadOnly,
                    key_prefix: "prefix".into(),
                    credential_url: None,
                }),
                gha: Some(GHACacheConfig {
                    version: "sccache".to_string()
                }),
                redis: Some(RedisCacheConfig {
                    url: "redis://user:passwd@1.2.3.4:6379/1".to_owned(),
                }),
                memcached: Some(MemcachedCacheConfig {
                    url: "...".to_owned(),
                }),
                s3: Some(S3CacheConfig {
                    bucket: "name".to_owned(),
                    region: Some("us-east-2".to_owned()),
                    endpoint: Some("s3-us-east-1.amazonaws.com".to_owned()),
                    use_ssl: Some(true),
                    key_prefix: "s3prefix".into(),
                    no_credentials: true,
                }),
            },
            dist: DistConfig {
                auth: DistAuth::Token {
                    token: "secrettoken".to_owned()
                },
                #[cfg(any(feature = "dist-client", feature = "dist-server"))]
                scheduler_url: Some(
                    parse_http_url("http://1.2.3.4:10600")
                        .map(|url| { HTTPUrl::from_url(url) })
                        .expect("Scheduler url must be valid url str")
                ),
                #[cfg(not(any(feature = "dist-client", feature = "dist-server")))]
                scheduler_url: Some("http://1.2.3.4:10600".to_owned()),
                cache_dir: PathBuf::from("/home/user/.cache/sccache-dist-client"),
                toolchains: vec![],
                toolchain_cache_size: 5368709120,
                rewrite_includes_only: false,
            },
            server_startup_timeout_ms: Some(10000),
        }
    )
}
