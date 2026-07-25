//! INI configuration loading, validation, filtering, and path resolution.

use configparser::ini::Ini;
use directories::BaseDirs;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

const GLOBAL_KEYS: &[&str] = &["accounts"];
const ACCOUNT_KEYS: &[&str] = &[
    "maildir",
    "user",
    "clientid",
    "tenant",
    "folderseparator",
    "folderinclude",
    "folderexclude",
];

/// Filesystem locations used by the application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    /// Main INI configuration file.
    pub config_file: PathBuf,
    /// SQLite synchronization-state database.
    pub state_database: PathBuf,
}

impl AppPaths {
    /// Build application paths from resolved XDG configuration and state roots.
    pub fn build_from_roots(config_root: &Path, state_root: &Path) -> Self {
        Self {
            config_file: config_root.join("nochange/nochange.conf"),
            state_database: state_root.join("nochange/state.sqlite3"),
        }
    }

    /// Resolve XDG paths, using their conventional home-directory fallbacks.
    pub fn discover() -> Result<Self, ConfigError> {
        let home_dir = BaseDirs::new().map(|base_dirs| base_dirs.home_dir().to_path_buf());
        Self::build_from_optional_home(home_dir.as_deref())
    }

    fn build_from_optional_home(home_dir: Option<&Path>) -> Result<Self, ConfigError> {
        let home_dir = home_dir.ok_or(ConfigError::HomeDirectoryUnavailable)?;
        Ok(Self::build_from_environment(
            env::var_os("XDG_CONFIG_HOME").as_deref(),
            env::var_os("XDG_STATE_HOME").as_deref(),
            home_dir,
        ))
    }

    /// Build application paths from optional XDG values and a known home directory.
    pub fn build_from_environment(
        config_home: Option<&OsStr>,
        state_home: Option<&OsStr>,
        home_dir: &Path,
    ) -> Self {
        let config_root = config_home
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".config"));
        let state_root = state_home
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".local/state"));
        Self::build_from_roots(&config_root, &state_root)
    }
}

/// Folder selection policy for an account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FolderFilter {
    /// Select every folder.
    #[default]
    All,
    /// Select these folders and their descendants.
    Include(Vec<String>),
    /// Select everything except these folders and their descendants.
    Exclude(Vec<String>),
}

impl FolderFilter {
    fn is_selected(&self, remote_path: &str) -> bool {
        let normalized_path = normalize_remote_path(remote_path);
        match self {
            Self::All => true,
            Self::Include(paths) => paths
                .iter()
                .any(|filter| is_path_in_subtree(&normalized_path, filter)),
            Self::Exclude(paths) => !paths
                .iter()
                .any(|filter| is_path_in_subtree(&normalized_path, filter)),
        }
    }
}

/// Validated configuration for one Microsoft 365 account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountConfig {
    /// Name used to select this account on the command line.
    pub name: String,
    /// Canonical absolute root containing this account's Maildirs.
    pub maildir: PathBuf,
    /// Expected Microsoft 365 user identity.
    pub user: String,
    /// Microsoft Entra public-client application identifier.
    pub client_id: String,
    /// Microsoft Entra tenant name or identifier.
    pub tenant: String,
    /// Character used to flatten the remote folder hierarchy.
    pub folder_separator: String,
    /// Remote folder selection policy.
    pub folder_filter: FolderFilter,
}

impl AccountConfig {
    /// Determine whether a complete remote folder path is selected.
    pub fn is_folder_selected(&self, remote_path: &str) -> bool {
        self.folder_filter.is_selected(remote_path)
    }
}

/// Account-selection request from a CLI operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountSelection<'a> {
    /// Select every configured account in configuration order.
    All,
    /// Select exactly one account by name.
    Named(&'a str),
}

/// Fully validated application configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    accounts: Vec<AccountConfig>,
}

impl AppConfig {
    /// Load and validate an INI file, using `home_dir` to expand `~` paths.
    pub fn load_from(path: &Path, home_dir: &Path) -> Result<Self, ConfigError> {
        let mut ini = Ini::new();
        let values = ini.load(path).map_err(|message| ConfigError::Parse {
            path: path.to_path_buf(),
            message,
        })?;
        let global = values
            .get("global")
            .ok_or_else(|| ConfigError::MissingSection("global".into()))?;
        validate_keys("global", global.keys().map(String::as_str), GLOBAL_KEYS)?;

        let account_names = parse_account_names(get_required(global, "global", "accounts")?)?;
        validate_sections(values.keys().map(String::as_str), &account_names)?;

        let config_parent = path.parent().unwrap_or(Path::new("."));
        let mut accounts = Vec::with_capacity(account_names.len());
        for account_name in account_names {
            let section_name = account_name.to_lowercase();
            let section = values
                .get(&section_name)
                .ok_or_else(|| ConfigError::MissingSection(account_name.clone()))?;
            if section.contains_key("clientsecret") {
                return Err(ConfigError::ClientSecret(account_name));
            }
            validate_keys(
                &account_name,
                section.keys().map(String::as_str),
                ACCOUNT_KEYS,
            )?;

            let maildir_value = get_required(section, &account_name, "maildir")?;
            let maildir =
                resolve_maildir_path(maildir_value, home_dir, config_parent, &account_name)?;
            let user = get_required(section, &account_name, "user")?.to_owned();
            let client_id = get_required(section, &account_name, "clientid")?.to_owned();
            let tenant = get_optional(section, "tenant")
                .unwrap_or("organizations")
                .to_owned();
            let folder_separator = get_optional(section, "folderseparator")
                .unwrap_or(".")
                .to_owned();
            validate_separator(&account_name, &folder_separator)?;
            let folder_filter = build_folder_filter(section, &account_name)?;

            accounts.push(AccountConfig {
                name: account_name,
                maildir,
                user,
                client_id,
                tenant,
                folder_separator,
                folder_filter,
            });
        }
        validate_maildir_roots(&accounts)?;
        Ok(Self { accounts })
    }

    /// Return configured account names in synchronization order.
    pub fn account_names(&self) -> Vec<&str> {
        self.accounts
            .iter()
            .map(|account| account.name.as_str())
            .collect()
    }

    /// Look up an account by its configured name.
    pub fn get_account(&self, name: &str) -> Option<&AccountConfig> {
        self.accounts.iter().find(|account| account.name == name)
    }

    /// Resolve an all-accounts or single-account request.
    pub fn get_selected_accounts(
        &self,
        selection: AccountSelection<'_>,
    ) -> Result<Vec<&AccountConfig>, ConfigError> {
        match selection {
            AccountSelection::All => Ok(self.accounts.iter().collect()),
            AccountSelection::Named(name) => self
                .get_account(name)
                .map(|account| vec![account])
                .ok_or_else(|| ConfigError::UnknownAccount(name.to_owned())),
        }
    }
}

/// Failures encountered while reading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The INI file could not be read or parsed.
    #[error("could not load configuration {path}: {message}")]
    Parse {
        /// Configuration path that failed.
        path: PathBuf,
        /// Parser or filesystem diagnostic.
        message: String,
    },
    /// A required INI section is absent.
    #[error("missing required configuration section [{0}]")]
    MissingSection(String),
    /// A required key is absent or empty.
    #[error("missing required key '{key}' in section [{section}]")]
    MissingKey {
        /// Section containing the missing key.
        section: String,
        /// Required key.
        key: String,
    },
    /// An unknown key was found.
    #[error("unknown key '{key}' in section [{section}]")]
    UnknownKey {
        /// Section containing the unknown key.
        section: String,
        /// Unsupported key.
        key: String,
    },
    /// An undeclared section was found.
    #[error("unknown configuration section [{0}]")]
    UnknownSection(String),
    /// The account list repeats a name.
    #[error("duplicate account '{0}' in [global] accounts")]
    DuplicateAccount(String),
    /// A public-client configuration attempted to provide a client secret.
    #[error("account '{0}' contains forbidden key 'clientsecret'")]
    ClientSecret(String),
    /// Folder selection modes were both configured.
    #[error("account '{0}' cannot set both folderinclude and folderexclude")]
    ConflictingFolderFilters(String),
    /// The folder separator cannot safely appear in Maildir paths.
    #[error("account '{account}' has unsafe folder separator '{separator}'")]
    UnsafeFolderSeparator {
        /// Account with the invalid setting.
        account: String,
        /// Rejected separator.
        separator: String,
    },
    /// A configured filesystem path is invalid.
    #[error("invalid maildir path for account '{account}': {message}")]
    InvalidPath {
        /// Account with the invalid path.
        account: String,
        /// Actionable path diagnostic.
        message: String,
    },
    /// Two account roots are identical or nested.
    #[error("maildir roots for accounts '{first}' and '{second}' overlap")]
    MaildirCollision {
        /// First conflicting account.
        first: String,
        /// Second conflicting account.
        second: String,
    },
    /// A CLI operation selected an account that is not configured.
    #[error("unknown account '{0}'")]
    UnknownAccount(String),
    /// A home directory is required but could not be discovered.
    #[error("could not determine the user's home directory")]
    HomeDirectoryUnavailable,
    /// The process working directory could not be resolved for a relative path.
    #[error("could not resolve an absolute configuration path: {0}")]
    WorkingDirectory(#[from] std::io::Error),
}

fn get_required<'a>(
    section: &'a std::collections::HashMap<String, Option<String>>,
    section_name: &str,
    key: &str,
) -> Result<&'a str, ConfigError> {
    get_optional(section, key).ok_or_else(|| ConfigError::MissingKey {
        section: section_name.to_owned(),
        key: key.to_owned(),
    })
}

fn get_optional<'a>(
    section: &'a std::collections::HashMap<String, Option<String>>,
    key: &str,
) -> Option<&'a str> {
    section
        .get(key)
        .and_then(Option::as_deref)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn validate_keys<'a>(
    section: &str,
    keys: impl Iterator<Item = &'a str>,
    allowed: &[&str],
) -> Result<(), ConfigError> {
    for key in keys {
        if !allowed.contains(&key) {
            return Err(ConfigError::UnknownKey {
                section: section.to_owned(),
                key: key.to_owned(),
            });
        }
    }
    Ok(())
}

fn parse_account_names(value: &str) -> Result<Vec<String>, ConfigError> {
    let mut names = Vec::new();
    let mut normalized_names = HashSet::new();
    for raw_name in value.split(',') {
        let name = raw_name.trim();
        if name.is_empty() {
            return Err(ConfigError::MissingKey {
                section: "global".into(),
                key: "accounts".into(),
            });
        }
        if !normalized_names.insert(name.to_lowercase()) {
            return Err(ConfigError::DuplicateAccount(name.to_owned()));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

fn validate_sections<'a>(
    sections: impl Iterator<Item = &'a str>,
    account_names: &[String],
) -> Result<(), ConfigError> {
    let allowed: HashSet<String> = account_names
        .iter()
        .map(|name| name.to_lowercase())
        .chain(["global".to_owned(), "default".to_owned()])
        .collect();
    for section in sections {
        if !allowed.contains(section) {
            return Err(ConfigError::UnknownSection(section.to_owned()));
        }
    }
    Ok(())
}

fn build_folder_filter(
    section: &std::collections::HashMap<String, Option<String>>,
    account: &str,
) -> Result<FolderFilter, ConfigError> {
    let include = get_optional(section, "folderinclude");
    let exclude = get_optional(section, "folderexclude");
    match (include, exclude) {
        (Some(_), Some(_)) => Err(ConfigError::ConflictingFolderFilters(account.to_owned())),
        (Some(value), None) => Ok(FolderFilter::Include(parse_folder_paths(value))),
        (None, Some(value)) => Ok(FolderFilter::Exclude(parse_folder_paths(value))),
        (None, None) => Ok(FolderFilter::All),
    }
}

fn parse_folder_paths(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(normalize_remote_path)
        .filter(|path| !path.is_empty())
        .collect()
}

fn normalize_remote_path(path: &str) -> String {
    path.split('/')
        .map(str::trim)
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>()
        .join("/")
        .to_lowercase()
}

fn is_path_in_subtree(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

fn validate_separator(account: &str, separator: &str) -> Result<(), ConfigError> {
    let mut characters = separator.chars();
    let valid = characters
        .next()
        .is_some_and(|character| !character.is_control() && !matches!(character, '/' | '\\' | ':'))
        && characters.next().is_none();
    if valid {
        Ok(())
    } else {
        Err(ConfigError::UnsafeFolderSeparator {
            account: account.to_owned(),
            separator: separator.to_owned(),
        })
    }
}

fn resolve_maildir_path(
    configured: &str,
    home_dir: &Path,
    config_parent: &Path,
    account: &str,
) -> Result<PathBuf, ConfigError> {
    let configured_path = Path::new(configured);
    let expanded = if configured_path == Path::new("~") {
        home_dir.to_path_buf()
    } else if let Ok(remainder) = configured_path.strip_prefix("~") {
        home_dir.join(remainder)
    } else if configured.starts_with('~') {
        return Err(ConfigError::InvalidPath {
            account: account.to_owned(),
            message: "only '~' and '~/' home expansions are supported".into(),
        });
    } else if configured_path.is_absolute() {
        configured_path.to_path_buf()
    } else {
        config_parent.join(configured_path)
    };
    let absolute = std::path::absolute(expanded)?;
    canonicalize_maildir_path(&absolute, account)
}

fn canonicalize_maildir_path(path: &Path, account: &str) -> Result<PathBuf, ConfigError> {
    canonicalize_nonexistent(&normalize_path(path)).map_err(|error| ConfigError::InvalidPath {
        account: account.to_owned(),
        message: error.to_string(),
    })
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn canonicalize_nonexistent(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut existing = path;
    let mut missing: Vec<OsString> = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(file_name), Some(parent)) => {
                missing.push(file_name.to_os_string());
                existing = parent;
            }
            _ => return fs::canonicalize(existing),
        }
    }
    let mut canonical = fs::canonicalize(existing)?;
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn validate_maildir_roots(accounts: &[AccountConfig]) -> Result<(), ConfigError> {
    for (index, first) in accounts.iter().enumerate() {
        for second in &accounts[index + 1..] {
            if first.maildir.starts_with(&second.maildir)
                || second.maildir.starts_with(&first.maildir)
            {
                return Err(ConfigError::MaildirCollision {
                    first: first.name.clone(),
                    second: second.name.clone(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AppPaths, canonicalize_maildir_path, normalize_path, resolve_maildir_path};
    use std::env;
    use std::path::Path;

    #[test]
    fn resolves_paths_from_relative_base_directories() {
        let resolved = resolve_maildir_path(
            "mail",
            Path::new("home"),
            Path::new("configuration"),
            "work",
        )
        .expect("relative base should resolve from the process directory");

        assert_eq!(
            resolved,
            env::current_dir()
                .expect("process directory should be available")
                .canonicalize()
                .expect("process directory should be canonicalizable")
                .join("configuration/mail")
        );
    }

    #[test]
    fn reports_paths_with_no_canonicalizable_ancestor() {
        assert!(canonicalize_maildir_path(Path::new(""), "work").is_err());
    }

    #[test]
    fn normalizes_an_explicit_current_directory_component() {
        assert_eq!(normalize_path(Path::new(".")), Path::new(""));
    }

    #[test]
    fn reports_an_unavailable_home_directory() {
        assert!(AppPaths::build_from_optional_home(None).is_err());
    }
}
