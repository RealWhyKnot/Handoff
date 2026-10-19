use std::fs;
use std::path::Path;

use crate::dispatch::HandlerError;

use super::fs::{err, local_path};

const SECRET_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".kube",
    ".password-store",
];

const SECRET_FILES: &[&str] = &[
    "consolehost_history.txt",
    "visual studio code host_history.txt",
    ".git-credentials",
    ".netrc",
    "_netrc",
    ".pgpass",
    "pgpass.conf",
    ".my.cnf",
    ".npmrc",
    ".pypirc",
    ".boto",
    ".s3cfg",
    ".dockercfg",
    ".vault-token",
    "login data",
    "login data for account",
    "cookies",
    "extension cookies",
    "web data",
    "account web data",
    "local state",
    "ya passman data",
    "ya credit cards",
    "cookies.txt",
    "logins.json",
    "logins-backup.json",
    "key3.db",
    "key4.db",
    "cookies.sqlite",
    "signons.sqlite",
    "signedinuser.json",
    "sessionstore.jsonlz4",
    "sitemanager.xml",
    "recentservers.xml",
    "winscp.ini",
    "confcons.xml",
    "credentials.tfrc.json",
    "application_default_credentials.json",
    "access_tokens.db",
    "credentials.db",
    "c.pwd",
    "launcher_accounts.json",
    "launcher_accounts_microsoft_store.json",
    "wallet.dat",
    "unattend.xml",
    "autounattend.xml",
    "unattended.xml",
    "sysprep.inf",
    "sysprep.xml",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_ecdsa_sk",
    "id_ed25519_sk",
    "hiberfil.sys",
    "pagefile.sys",
];

const SQLITE_SIDE_FILES: &[&str] = &["-journal", "-wal", "-shm"];

const SECRET_PREFIXES: &[&str] = &[
    "ntuser.dat",
    "usrclass.dat",
    "swapfile.sys",
    "ssh_host_",
    "ssfn",
    "launcher_msa_credentials",
];

const SECRET_EXTENSIONS: &[&str] = &[
    ".kdbx",
    ".kdb",
    ".keyx",
    ".pfx",
    ".p12",
    ".ppk",
    ".jks",
    ".keystore",
    ".psafe3",
    ".dpapi",
    ".key",
    ".pem",
    ".ovpn",
];

const ENV_TEMPLATES: &[&str] = &["example", "sample", "template", "dist"];

const DOCKER_PLAIN_FILES: &[&str] = &["daemon.json", "features.json"];

const SECRET_TREES: &[&str] = &[
    "/windows/system32/config/",
    "/windows/system32/configstore/",
    "/windows/system32/microsoft/protect/",
    "/windows/repair/",
    "/windows/ntds/",
    "/programdata/microsoft/crypto/",
    "/programdata/microsoft/vault/",
    "/programdata/microsoft/wlansvc/profiles/",
    "/users/all users/microsoft/crypto/",
    "/users/all users/microsoft/vault/",
    "/appdata/roaming/microsoft/credentials/",
    "/appdata/local/microsoft/credentials/",
    "/appdata/roaming/microsoft/protect/",
    "/appdata/roaming/microsoft/vault/",
    "/appdata/local/microsoft/vault/",
    "/appdata/roaming/microsoft/crypto/",
    "/appdata/roaming/microsoft/systemcertificates/my/",
    "/appdata/local/microsoft/ngc/",
    "/appdata/local/microsoft/tokenbroker/",
    "/appdata/local/microsoft/identitycache/",
    "/appdata/local/microsoft/oneauth/",
    "/appdata/local/.identityservice/",
    "/appdata/roaming/discord/local storage/",
    "/local storage/",
    "/indexeddb/",
    "/telegram desktop/tdata/",
    "/appdata/roaming/signal/config.json/",
    "/appdata/roaming/signal/sql/",
    "/sessionstore-backups/",
    "/.config/gh/",
    "/appdata/roaming/github cli/",
    "/github-copilot/hosts.json/",
    "/github-copilot/apps.json/",
    "/gcloud/legacy_credentials/",
    "/.cargo/credentials/",
    "/.cargo/credentials.toml/",
    "/.m2/settings-security.xml/",
    "/composer/auth.json/",
    "/appdata/roaming/bitwarden/data.json/",
    "/appdata/local/1password/data/",
    "/appdata/roaming/dashlane/profiles/",
    "/appdata/local/steam/local.vdf/",
    "/exodus/exodus.wallet/",
    "/electrum/wallets/",
];

pub(crate) fn normalized_for_match(path: &str) -> String {
    let lower = path.replace('/', "\\").to_lowercase();
    let lower = lower
        .strip_prefix("\\\\?\\")
        .or_else(|| lower.strip_prefix("\\??\\"))
        .unwrap_or(&lower)
        .to_string();
    let rest = match lower.as_bytes() {
        [d, b':', ..] if d.is_ascii_alphabetic() => &lower[2..],
        _ => lower.as_str(),
    };
    let mut out = String::with_capacity(rest.len() + 1);
    for seg in rest.split('\\').filter(|s| !s.is_empty()) {
        out.push('/');
        out.push_str(seg.trim_end_matches([' ', '.']));
    }
    out.push('/');
    out
}

type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

fn os_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn env_base(env: Env, key: &str) -> Option<String> {
    let value = env(key)?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let base = normalized_for_match(value);
    (base != "/").then_some(base)
}

fn segments_of(norm: &str) -> Vec<&str> {
    norm.split('/').filter(|s| !s.is_empty()).collect()
}

fn system_root_trees() -> Vec<String> {
    let Some(base) = env_base(&os_env, "SystemRoot") else {
        return Vec::new();
    };
    ["system32/config/", "repair/", "system32/microsoft/protect/"]
        .iter()
        .map(|t| format!("{base}{t}"))
        .collect()
}

fn is_secret_name(name: &str) -> bool {
    let base = SQLITE_SIDE_FILES
        .iter()
        .find_map(|s| name.strip_suffix(s))
        .unwrap_or(name);
    SECRET_FILES.contains(&base)
        || SECRET_PREFIXES.iter().any(|p| name.starts_with(p))
        || SECRET_EXTENSIONS.iter().any(|e| name.ends_with(e))
        || is_env_file(name)
}

fn is_env_file(name: &str) -> bool {
    if name == ".env" {
        return true;
    }
    let Some(rest) = name.strip_prefix(".env.") else {
        return false;
    };
    let last = rest.rsplit('.').next().unwrap_or(rest);
    !ENV_TEMPLATES.contains(&last)
}

fn is_docker_secret(segments: &[&str]) -> bool {
    let Some(i) = segments.iter().position(|s| *s == ".docker") else {
        return false;
    };
    match &segments[i + 1..] {
        [name] => !DOCKER_PLAIN_FILES.contains(name),
        _ => true,
    }
}

pub(crate) fn is_secret_path(path: &str) -> bool {
    let norm = normalized_for_match(path);
    let segments = segments_of(&norm);
    if segments.iter().any(|s| SECRET_DIRS.contains(s)) || is_docker_secret(&segments) {
        return true;
    }
    if segments.last().is_some_and(|name| is_secret_name(name)) {
        return true;
    }
    SECRET_TREES.iter().any(|t| norm.contains(t))
        || system_root_trees()
            .iter()
            .any(|t| norm.starts_with(t.as_str()))
}

pub(crate) fn guard_read_path(kind: &str, path: &str) -> Result<(), HandlerError> {
    let refuse = || {
        err(format!(
            "{kind}: won't read {path} because it can hold passwords or keys"
        ))
    };
    if is_secret_path(path) {
        return Err(refuse());
    }
    if let Ok(real) = fs::canonicalize(path) {
        let real = real.to_string_lossy().into_owned();
        if real.starts_with("\\\\?\\UNC\\") {
            return Err(err(format!(
                "{kind}: {path} points to a network location, which is not allowed"
            )));
        }
        if is_secret_path(&real) {
            return Err(refuse());
        }
    }
    Ok(())
}

pub(crate) fn guard_write_path(kind: &str, p: &str) -> Result<(), HandlerError> {
    guard_mutable_path(kind, p, false)
}

pub(crate) fn guard_delete_path(p: &str) -> Result<(), HandlerError> {
    let kind = "fs.delete";
    local_path(kind, p)?;
    let real = resolved_path(kind, p)?;
    if is_protected_root(p) || real.as_deref().is_some_and(is_protected_root) {
        return Err(err(format!(
            "fs.delete: won't delete {p} because it's a whole drive or a main Windows or user folder"
        )));
    }
    guard_mutable_path(kind, p, true)
}

const SYSTEM_TREES: &[&str] = &[
    "/windows/system32/",
    "/windows/syswow64/",
    "/windows/sysnative/",
    "/windows/tasks/",
    "/program files/",
    "/program files (x86)/",
];

const SYSTEM_ROOT_SUBTREES: &[&str] = &["system32/", "syswow64/", "sysnative/", "tasks/"];

const STARTUP_TREE: &str = "/start menu/programs/startup/";

const HANDOFF_TREE: &str = "/whyknot/handoff/";

const AUTORUN_TREES: &[&str] = &[
    "/appdata/local/microsoft/windowsapps/",
    "/appdata/roaming/microsoft/word/startup/",
    "/appdata/roaming/microsoft/excel/xlstart/",
    "/appdata/roaming/microsoft/templates/",
    "/appdata/roaming/microsoft/addins/",
];

const SHELL_STARTUP_FILES: &[&str] = &[".bashrc", ".bash_profile", ".bash_login", ".profile"];

struct Rules {
    system: Vec<String>,
    ssh: Vec<String>,
}

fn rules(env: Env) -> Rules {
    let mut system: Vec<String> = SYSTEM_TREES.iter().map(|t| t.to_string()).collect();
    for key in ["SystemRoot", "windir"] {
        if let Some(base) = env_base(env, key) {
            system.extend(SYSTEM_ROOT_SUBTREES.iter().map(|t| format!("{base}{t}")));
        }
    }
    for key in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        system.extend(env_base(env, key));
    }
    let mut ssh = vec!["/programdata/ssh/".to_string()];
    for key in ["ProgramData", "ALLUSERSPROFILE"] {
        if let Some(base) = env_base(env, key) {
            ssh.push(format!("{base}ssh/"));
        }
    }
    Rules { system, ssh }
}

fn guard_mutable_path(kind: &str, p: &str, deleting: bool) -> Result<(), HandlerError> {
    local_path(kind, p)?;
    let rules = rules(&os_env);
    check_mutable(&rules, kind, p, p, deleting)?;
    if let Some(real) = resolved_path(kind, p)? {
        check_mutable(&rules, kind, p, &real, deleting)?;
    }
    Ok(())
}

fn check_mutable(
    rules: &Rules,
    kind: &str,
    shown: &str,
    p: &str,
    deleting: bool,
) -> Result<(), HandlerError> {
    let norm = normalized_for_match(p);
    if rules.system.iter().any(|t| norm.starts_with(t.as_str())) {
        return Err(err(format!(
            "{kind}: won't change {shown} because Windows and installed programs need it"
        )));
    }
    let segments = segments_of(&norm);
    if segments.contains(&".ssh") || rules.ssh.iter().any(|t| norm.starts_with(t.as_str())) {
        return Err(err(format!(
            "{kind}: won't change {shown} because SSH keys and settings live there"
        )));
    }
    if norm.contains(STARTUP_TREE) && !(deleting && !norm.ends_with(STARTUP_TREE)) {
        return Err(err(format!(
            "{kind}: won't change {shown} because Windows runs what's in the Startup folder at sign-in"
        )));
    }
    if is_powershell_profile(&segments) {
        return Err(err(format!(
            "{kind}: won't change {shown} because PowerShell runs this file every time it starts"
        )));
    }
    if norm.contains(HANDOFF_TREE) {
        return Err(err(format!(
            "{kind}: won't change {shown} because Handoff keeps its settings and logs there"
        )));
    }
    if !deleting && runs_on_its_own(&norm, &segments) {
        return Err(err(format!(
            "{kind}: won't change {shown} because files there can run on their own later"
        )));
    }
    if is_secret_path(p) {
        return Err(err(format!(
            "{kind}: won't change {shown} because it can hold passwords or keys"
        )));
    }
    Ok(())
}

fn is_powershell_profile(segments: &[&str]) -> bool {
    let [.., dir, name] = segments else {
        return false;
    };
    matches!(*dir, "windowspowershell" | "powershell")
        && (*name == "profile.ps1" || name.ends_with("_profile.ps1"))
}

fn runs_on_its_own(norm: &str, segments: &[&str]) -> bool {
    AUTORUN_TREES.iter().any(|t| norm.contains(t))
        || segments
            .windows(2)
            .any(|w| matches!(w[0], "windowspowershell" | "powershell") && w[1] == "modules")
        || segments
            .last()
            .is_some_and(|name| SHELL_STARTUP_FILES.contains(name))
}

fn resolved_path(kind: &str, p: &str) -> Result<Option<String>, HandlerError> {
    let mut cur = Path::new(p);
    let mut tail = Vec::new();
    let real = loop {
        match fs::canonicalize(cur) {
            Ok(real) => break real,
            Err(_) if fs::symlink_metadata(cur).is_ok() => {
                return Err(err(format!(
                    "{kind}: can't tell where {p} leads, so it won't be changed"
                )));
            }
            Err(_) => {}
        }
        let (Some(name), Some(parent)) = (cur.file_name(), cur.parent()) else {
            return Ok(None);
        };
        tail.push(name);
        cur = parent;
    };
    let mut real = real;
    for name in tail.iter().rev() {
        real.push(name);
    }
    let real = real.to_string_lossy().into_owned();
    if real.starts_with("\\\\?\\UNC\\") {
        return Err(err(format!(
            "{kind}: {p} points to a network location, which is not allowed"
        )));
    }
    Ok(Some(real))
}

const PROTECTED_ROOTS: &[&str] = &[
    "/",
    "/users/",
    "/programdata/",
    "/windows/",
    "/program files/",
    "/program files (x86)/",
];

fn protected_roots(env: Env) -> Vec<String> {
    let mut roots: Vec<String> = PROTECTED_ROOTS.iter().map(|r| r.to_string()).collect();
    for key in [
        "SystemRoot",
        "windir",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
        "ALLUSERSPROFILE",
        "PUBLIC",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        roots.extend(env_base(env, key));
    }
    for key in ["USERPROFILE", "PUBLIC"] {
        let dir = env(key).unwrap_or_default();
        if let Some(parent) = Path::new(dir.trim()).parent() {
            let parent = normalized_for_match(&parent.to_string_lossy());
            if parent != "/" {
                roots.push(parent);
            }
        }
    }
    if let Some(home) = env_base(env, "USERPROFILE") {
        roots.push(format!("{home}appdata/"));
    }
    roots
}

fn is_protected_root(p: &str) -> bool {
    let norm = normalized_for_match(p);
    norm == "/" || protected_roots(&os_env).contains(&norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| {
            vars.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.to_string())
        }
    }

    fn refusal(rules: &Rules, kind: &str, path: &str, deleting: bool) -> String {
        check_mutable(rules, kind, path, path, deleting)
            .err()
            .map(|e| e.message)
            .unwrap_or_default()
    }

    #[test]
    fn other_browsers_and_their_side_files_are_secret() {
        for p in [
            "C:\\Users\\bob\\AppData\\Local\\BraveSoftware\\Brave-Browser\\User Data\\Default\\Login Data",
            "C:\\Users\\bob\\AppData\\Roaming\\Opera Software\\Opera Stable\\Default\\Network\\Cookies",
            "C:\\Users\\bob\\AppData\\Local\\Vivaldi\\User Data\\Profile 1\\Web Data",
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\Edge\\User Data\\Default\\Login Data For Account",
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Account Web Data",
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Login Data-journal",
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Network\\Cookies-wal",
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Extension Cookies",
            "C:\\Users\\bob\\AppData\\Local\\Yandex\\YandexBrowser\\User Data\\Default\\Ya Passman Data",
            "C:\\Users\\bob\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\x.default\\logins-backup.json",
            "C:\\Users\\bob\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\x.default\\signedInUser.json",
            "C:\\Users\\bob\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\x.default\\sessionstore.jsonlz4",
            "C:\\Users\\bob\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\x.default\\sessionstore-backups\\recovery.jsonlz4",
            "C:\\Users\\bob\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\x.default\\cookies.sqlite-wal",
            "C:\\Users\\bob\\Downloads\\cookies.txt",
        ] {
            assert!(is_secret_path(p), "{p} was allowed");
        }
        for p in [
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Preferences",
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Crashpad\\reports\\a.dmp",
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\Edge\\User Data\\Default\\History",
            "C:\\Users\\bob\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\x.default\\prefs.js",
            "C:\\Users\\bob\\AppData\\Roaming\\Mozilla\\Firefox\\Crash Reports\\pending\\a.dmp",
            "C:\\Users\\bob\\AppData\\Local\\Packages\\App_123\\LocalState\\settings.dat",
            "C:\\Users\\bob\\Documents\\cookies recipe.txt",
        ] {
            assert!(!is_secret_path(p), "{p} was blocked");
        }
    }

    #[test]
    fn saved_logins_from_other_tools_are_secret() {
        for p in [
            "C:\\Users\\bob\\AppData\\Roaming\\FileZilla\\sitemanager.xml",
            "C:\\Users\\bob\\AppData\\Roaming\\FileZilla\\recentservers.xml",
            "C:\\Users\\bob\\AppData\\Roaming\\WinSCP.ini",
            "C:\\Users\\bob\\AppData\\Roaming\\mRemoteNG\\confCons.xml",
            "C:\\Users\\bob\\AppData\\Roaming\\gcloud\\credentials.db",
            "C:\\Users\\bob\\AppData\\Roaming\\gcloud\\access_tokens.db",
            "C:\\Users\\bob\\AppData\\Roaming\\gcloud\\application_default_credentials.json",
            "C:\\Users\\bob\\AppData\\Roaming\\gcloud\\legacy_credentials\\bob@x.com\\adc.json",
            "C:\\Users\\bob\\AppData\\Roaming\\terraform.d\\credentials.tfrc.json",
            "C:\\Users\\bob\\AppData\\Roaming\\GitHub CLI\\hosts.yml",
            "C:\\Users\\bob\\AppData\\Local\\github-copilot\\apps.json",
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\TokenBroker\\Cache\\a.tbres",
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\OneAuth\\accounts\\a",
            "C:\\Users\\bob\\AppData\\Local\\.IdentityService\\msal.cache",
            "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Microsoft\\Ngc\\a\\b.dat",
            "C:\\Users\\bob\\AppData\\Roaming\\Bitwarden\\data.json",
            "C:\\Users\\bob\\AppData\\Local\\1Password\\data\\1password.sqlite",
            "C:\\Users\\bob\\AppData\\Roaming\\Dashlane\\profiles\\bob\\a.aes",
            "C:\\Users\\bob\\AppData\\Roaming\\Signal\\config.json",
            "C:\\Users\\bob\\AppData\\Roaming\\Signal\\sql\\db.sqlite",
            "C:\\Users\\bob\\AppData\\Local\\Steam\\local.vdf",
            "C:\\Program Files (x86)\\Steam\\ssfn1234567890",
            "C:\\Users\\bob\\AppData\\Roaming\\.minecraft\\launcher_accounts.json",
            "C:\\Users\\bob\\AppData\\Roaming\\.minecraft\\launcher_msa_credentials.bin",
            "C:\\Users\\bob\\AppData\\Roaming\\JetBrains\\Rider2024.1\\c.pwd",
            "C:\\Users\\bob\\AppData\\Roaming\\Bitcoin\\wallets\\main\\wallet.dat",
            "C:\\Users\\bob\\AppData\\Roaming\\Exodus\\exodus.wallet\\seed.seco",
            "C:\\Users\\bob\\AppData\\Roaming\\Electrum\\wallets\\default_wallet",
            "C:\\Users\\bob\\.cargo\\credentials.toml",
            "C:\\Users\\bob\\.m2\\settings-security.xml",
            "C:\\Users\\bob\\AppData\\Roaming\\Composer\\auth.json",
            "C:\\Users\\bob\\AppData\\Roaming\\postgresql\\pgpass.conf",
            "C:\\Users\\bob\\.my.cnf",
            "C:\\Users\\bob\\.boto",
            "C:\\Users\\bob\\.vault-token",
            "C:\\Users\\bob\\code\\app\\.env",
            "C:\\Users\\bob\\code\\app\\.env.production",
            "C:\\Users\\bob\\code\\app\\.env.local",
            "C:\\Users\\bob\\code\\app\\release.jks",
            "C:\\Users\\bob\\code\\app\\debug.keystore",
            "C:\\Users\\bob\\Documents\\old.kdb",
            "C:\\Users\\bob\\Documents\\vault.keyx",
            "C:\\Users\\bob\\Documents\\safe.psafe3",
            "C:\\Program Files\\WireGuard\\Data\\Configurations\\wg0.conf.dpapi",
            "C:\\Windows\\Panther\\Unattend\\unattended.xml",
            "C:\\Windows\\System32\\Sysprep\\sysprep.xml",
        ] {
            assert!(is_secret_path(p), "{p} was allowed");
        }
        for p in [
            "C:\\Users\\bob\\AppData\\Roaming\\FileZilla\\filezilla.xml",
            "C:\\Users\\bob\\AppData\\Roaming\\gcloud\\logs\\2026.10.19\\a.log",
            "C:\\Users\\bob\\AppData\\Roaming\\gcloud\\configurations\\config_default",
            "C:\\Users\\bob\\AppData\\Roaming\\Signal\\logs\\main.log",
            "C:\\Users\\bob\\AppData\\Roaming\\Bitwarden\\logs\\app.log",
            "C:\\Program Files (x86)\\Steam\\logs\\content_log.txt",
            "C:\\Users\\bob\\AppData\\Roaming\\.minecraft\\logs\\latest.log",
            "C:\\Users\\bob\\.cargo\\config.toml",
            "C:\\Users\\bob\\code\\app\\.env.example",
            "C:\\Users\\bob\\code\\app\\.env.local.sample",
            "C:\\Users\\bob\\code\\app\\.env.template",
            "C:\\Users\\bob\\code\\app\\env.py",
            "C:\\Users\\bob\\Documents\\wallet.txt",
            "C:\\Program Files\\WireGuard\\log.bin",
            "C:\\Windows\\Panther\\setupact.log",
        ] {
            assert!(!is_secret_path(p), "{p} was blocked");
        }
    }

    #[test]
    fn docker_settings_are_readable_but_its_logins_are_not() {
        for p in [
            "C:\\Users\\bob\\.docker\\config.json",
            "C:\\Users\\bob\\.docker\\key.pem",
            "C:\\Users\\bob\\.docker\\contexts\\meta\\abc\\meta.json",
            "C:\\Users\\bob\\.docker\\machine\\certs\\key.pem",
            "C:\\Users\\bob\\.docker",
        ] {
            assert!(is_secret_path(p), "{p} was allowed");
        }
        for p in [
            "C:\\Users\\bob\\.docker\\daemon.json",
            "C:\\Users\\bob\\.DOCKER\\features.json",
            "C:\\Users\\bob\\AppData\\Local\\Docker\\log\\vm\\dockerd.log",
        ] {
            assert!(!is_secret_path(p), "{p} was blocked");
        }
    }

    #[test]
    fn history_keys_and_browser_storage_are_secret() {
        for p in [
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Windows\\PowerShell\\PSReadLine\\ConsoleHost_history.txt",
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Windows\\PowerShell\\PSReadLine\\Visual Studio Code Host_history.txt",
            "C:\\Users\\bob\\certs\\server.key",
            "C:\\Users\\bob\\certs\\server.pem",
            "C:\\Users\\bob\\OpenVPN\\config\\work.ovpn",
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\Edge\\User Data\\Default\\Local Storage\\leveldb\\000003.log",
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\IndexedDB\\x.leveldb\\CURRENT",
            "C:\\Users\\bob\\AppData\\Roaming\\Slack\\Local Storage\\leveldb\\LOG",
        ] {
            assert!(is_secret_path(p), "{p} was allowed");
        }
        for p in [
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Windows\\PowerShell\\PSReadLine\\notes.txt",
            "C:\\Users\\bob\\certs\\server.crt",
            "C:\\Users\\bob\\Documents\\keyboard.txt",
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Preferences",
        ] {
            assert!(!is_secret_path(p), "{p} was blocked");
        }
    }

    #[test]
    fn crash_dumps_and_logs_stay_readable() {
        for p in [
            "C:\\Windows\\MEMORY.DMP",
            "C:\\Windows\\Minidump\\101926-12345-01.dmp",
            "C:\\Windows\\LiveKernelReports\\WATCHDOG\\a.dmp",
            "C:\\Users\\bob\\AppData\\Local\\CrashDumps\\app.exe.1234.dmp",
            "C:\\Windows\\System32\\winevt\\Logs\\System.evtx",
            "C:\\Windows\\Logs\\CBS\\CBS.log",
            "C:\\Windows\\INF\\setupapi.dev.log",
            "C:\\ProgramData\\Microsoft\\Windows\\WER\\ReportArchive\\a\\Report.wer",
            "C:\\Users\\bob\\Downloads\\setup.msi",
        ] {
            assert!(!is_secret_path(p), "{p} was blocked");
        }
    }

    #[test]
    fn system_folders_come_from_the_environment() {
        let env = fake_env(&[
            ("SystemRoot", "E:\\WinNT"),
            ("ProgramFiles", "F:\\Apps"),
            ("ProgramFiles(x86)", "F:\\Apps86\\"),
            ("ProgramData", "G:\\Shared Data"),
        ]);
        let rules = rules(&env);
        for (path, why) in [
            (
                "E:\\WinNT\\System32\\drivers\\etc\\hosts",
                "installed programs need it",
            ),
            ("E:\\WINNT\\SysWOW64\\x.dll", "installed programs need it"),
            ("E:\\WinNT\\Tasks\\evil.job", "installed programs need it"),
            ("F:\\Apps\\Tool\\x.dll", "installed programs need it"),
            ("F:\\Apps86.\\Tool\\x.dll", "installed programs need it"),
            ("G:\\Shared Data\\ssh\\sshd_config", "SSH keys"),
            ("C:\\Windows\\Tasks\\evil.job", "installed programs need it"),
            (
                "C:\\Windows\\Sysnative\\x.dll",
                "installed programs need it",
            ),
        ] {
            for deleting in [false, true] {
                let msg = refusal(&rules, "fs.upload", path, deleting);
                assert!(msg.contains(why), "{path}: {msg:?}");
            }
        }
        for path in [
            "E:\\WinNT\\Temp\\x.txt",
            "E:\\WinNT\\SoftwareDistribution\\Download\\x",
            "F:\\AppsExtra\\x.txt",
            "G:\\Shared Data\\Vendor\\settings.ini",
            "G:\\Shared Data\\sshd-backup\\notes.txt",
        ] {
            assert_eq!(refusal(&rules, "fs.upload", path, false), "", "{path}");
        }
    }

    #[test]
    fn a_root_drive_in_the_environment_is_ignored() {
        let env = fake_env(&[("ProgramFiles", "C:\\"), ("SystemRoot", "  ")]);
        let rules = rules(&env);
        assert_eq!(
            refusal(&rules, "fs.upload", "C:\\Users\\bob\\x.txt", false),
            ""
        );
        assert_eq!(
            refusal(&rules, "fs.upload", "C:\\Windows\\Temp\\x.txt", false),
            ""
        );
    }

    #[test]
    fn places_that_run_code_later_refuse_new_files_but_allow_cleanup() {
        let rules = rules(&|_: &str| None);
        for path in [
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\WindowsApps\\python.exe",
            "C:\\Users\\bob\\Documents\\WindowsPowerShell\\Modules\\PSReadLine\\PSReadLine.psm1",
            "C:\\Users\\bob\\OneDrive\\Documents\\PowerShell\\Modules\\Evil",
            "D:\\MyDocs\\WindowsPowerShell\\Modules\\x\\x.psd1",
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Word\\STARTUP\\a.dotm",
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Excel\\XLSTART\\a.xlsm",
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Templates\\Normal.dotm",
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\AddIns\\a.xlam",
            "C:\\Users\\bob\\.bashrc",
            "C:\\Users\\bob\\.bash_profile",
            "C:\\Users\\bob\\.profile",
        ] {
            for kind in ["fs.upload", "fs.write", "fs.mkdir"] {
                let msg = refusal(&rules, kind, path, false);
                assert_eq!(
                    msg,
                    format!("{kind}: won't change {path} because files there can run on their own later")
                );
            }
            assert_eq!(refusal(&rules, "fs.delete", path, true), "", "{path}");
        }
        for path in [
            "C:\\Users\\bob\\Documents\\WindowsPowerShell\\Scripts\\fix.ps1",
            "C:\\Users\\bob\\Documents\\Modules\\notes.txt",
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\WindowsAppsBackup\\x",
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Word\\recovery.asd",
            "C:\\Users\\bob\\Documents\\bashrc-notes.txt",
            "C:\\Users\\bob\\Documents\\profile.txt",
        ] {
            assert_eq!(refusal(&rules, "fs.upload", path, false), "", "{path}");
        }
    }

    #[test]
    fn handoff_keeps_its_own_settings_and_audit_log() {
        let rules = rules(&|_: &str| None);
        for path in [
            "C:\\ProgramData\\whyknot\\handoff\\audit\\2026-10-19.jsonl",
            "C:\\ProgramData\\WhyKnot\\Handoff\\audit",
            "C:\\Users\\bob\\AppData\\Roaming\\whyknot\\handoff\\config.json",
            "C:\\Users\\bob\\AppData\\Local\\whyknot\\handoff\\logs\\handoff.log",
        ] {
            for (kind, deleting) in [
                ("fs.upload", false),
                ("fs.mkdir", false),
                ("fs.delete", true),
            ] {
                let msg = refusal(&rules, kind, path, deleting);
                assert_eq!(
                    msg,
                    format!("{kind}: won't change {path} because Handoff keeps its settings and logs there")
                );
            }
        }
        for path in [
            "C:\\ProgramData\\whyknot\\other\\x.txt",
            "C:\\Users\\bob\\Documents\\handoff\\notes.txt",
        ] {
            assert_eq!(refusal(&rules, "fs.upload", path, false), "", "{path}");
        }
    }

    #[test]
    fn whole_user_and_program_data_folders_are_protected_roots() {
        let env = fake_env(&[
            ("USERPROFILE", "E:\\Homes\\bob"),
            ("PUBLIC", "E:\\Homes\\Public"),
            ("APPDATA", "E:\\Homes\\bob\\AppData\\Roaming"),
            ("LOCALAPPDATA", "E:\\Homes\\bob\\AppData\\Local"),
            ("ProgramData", "G:\\Shared Data"),
        ]);
        let roots = protected_roots(&env);
        for path in [
            "E:\\Homes",
            "E:\\Homes\\bob",
            "E:\\Homes\\Public\\",
            "E:\\Homes\\bob\\AppData",
            "E:\\Homes\\bob\\AppData\\Roaming",
            "E:\\Homes\\bob\\AppData\\Local.",
            "G:\\Shared Data",
            "C:\\Users",
            "C:\\ProgramData",
            "C:\\Windows",
        ] {
            assert!(roots.contains(&normalized_for_match(path)), "{path}");
        }
        for path in [
            "E:\\Homes\\bob\\AppData\\Local\\Temp",
            "E:\\Homes\\bob\\Documents\\old",
            "G:\\Shared Data\\Vendor",
            "C:\\Users\\bob\\Downloads",
            "C:\\Windows\\Temp",
        ] {
            assert!(!roots.contains(&normalized_for_match(path)), "{path}");
        }
        for path in ["C:\\Users", "C:\\ProgramData", "C:\\Users\\"] {
            let out = guard_delete_path(path);
            assert!(
                out.as_ref()
                    .is_err_and(|e| e.message.contains("won't delete")),
                "{path}: {out:?}"
            );
        }
    }

    #[test]
    fn new_secrets_are_refused_for_writes_too() {
        for path in [
            "C:\\Users\\bob\\code\\app\\.env",
            "C:\\Users\\bob\\AppData\\Roaming\\FileZilla\\sitemanager.xml",
            "C:\\Users\\bob\\.docker\\config.json",
        ] {
            let msg = guard_write_path("fs.write", path)
                .err()
                .map(|e| e.message)
                .unwrap_or_default();
            assert_eq!(
                msg,
                format!("fs.write: won't change {path} because it can hold passwords or keys")
            );
        }
        assert!(guard_write_path("fs.write", "C:\\Users\\bob\\.docker\\daemon.json").is_ok());
    }

    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
    }

    #[cfg(windows)]
    #[test]
    fn chained_junctions_do_not_get_around_the_ssh_write_block() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("home").join(".ssh");
        fs::create_dir_all(&ssh).unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        junction(&first, &ssh);
        junction(&second, &first);
        for target in [
            second.join("authorized_keys"),
            second.join("new").join("deeper").join("authorized_keys"),
            second.clone(),
        ] {
            let target = target.to_string_lossy().into_owned();
            for kind in ["fs.upload", "fs.write", "fs.mkdir"] {
                let out = guard_write_path(kind, &target);
                assert!(
                    out.as_ref().is_err_and(|e| e.message.contains("SSH keys")),
                    "{kind} {target}: {out:?}"
                );
            }
            let out = guard_delete_path(&target);
            assert!(
                out.as_ref().is_err_and(|e| e.message.contains("SSH keys")),
                "delete {target}: {out:?}"
            );
        }
        assert!(!ssh.join("new").exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_does_not_get_around_the_module_folder_block() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir
            .path()
            .join("Documents")
            .join("WindowsPowerShell")
            .join("Modules");
        fs::create_dir_all(&modules).unwrap();
        let link = dir.path().join("innocent");
        junction(&link, &modules);
        let target = link.join("PSReadLine").join("PSReadLine.psm1");
        let target = target.to_string_lossy().into_owned();
        let out = guard_write_path("fs.upload", &target);
        assert!(
            out.as_ref()
                .is_err_and(|e| e.message.contains("can run on their own later")),
            "{out:?}"
        );
        assert!(guard_delete_path(&target).is_ok());
    }
}
