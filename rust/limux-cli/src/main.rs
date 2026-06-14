use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use limux_control::socket_path::{resolve_socket_path, SocketMode};
use limux_protocol::{V2Request, V2Response};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

mod agent_hooks;

const CLI_STATE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const CLI_STATE_LOCK_RETRY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdFormat {
    Refs,
    Both,
    Uuids,
}

impl IdFormat {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "refs" => Ok(Self::Refs),
            "both" => Ok(Self::Both),
            "uuids" => Ok(Self::Uuids),
            _ => bail!("--id-format must be one of refs|both|uuids"),
        }
    }
}

#[derive(Debug, Clone)]
struct GlobalOptions {
    socket: Option<PathBuf>,
    socket_mode: SocketMode,
    json_output: bool,
    id_format: IdFormat,
    request: Option<String>,
    pretty: bool,
    command_args: Vec<String>,
}

#[derive(Debug)]
enum CommandOutput {
    Text(String),
    Json(Value),
}

struct Client {
    socket: PathBuf,
    seq: u64,
}

impl Client {
    fn new(socket: PathBuf) -> Self {
        Self { socket, seq: 0 }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.seq = self.seq.saturating_add(1);
        let request = V2Request {
            id: Some(Value::String(format!("cli-{}", self.seq))),
            method: method.to_string(),
            params,
        };
        self.send_request(request).await
    }

    async fn send_request(&self, request: V2Request) -> Result<Value> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("failed to connect to socket {}", self.socket.display()))?;
        let (reader_half, mut writer_half) = stream.into_split();

        let mut payload = serde_json::to_string(&request).context("failed to encode request")?;
        payload.push('\n');

        writer_half
            .write_all(payload.as_bytes())
            .await
            .context("failed to write request")?;
        writer_half
            .flush()
            .await
            .context("failed to flush request")?;

        let mut reader = BufReader::new(reader_half);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .context("failed to read response")?;

        if line.trim().is_empty() {
            bail!("server returned an empty response");
        }

        let response: V2Response =
            serde_json::from_str(line.trim()).context("response was not valid v2 JSON")?;

        if response.ok {
            Ok(response.result.unwrap_or_else(|| json!({})))
        } else {
            let err = response
                .error
                .ok_or_else(|| anyhow!("server returned !ok without error payload"))?;
            if err.code == -32004 {
                bail!("not_found: {}", err.message);
            }
            bail!("{}: {}", err.code, err.message);
        }
    }
}

fn parse_global_args() -> Result<GlobalOptions> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let mut socket: Option<PathBuf> = None;
    let mut socket_mode = SocketMode::Runtime;
    let mut json_output = false;
    let mut id_format = IdFormat::Refs;
    let mut request: Option<String> = None;
    let mut pretty = false;

    let mut command_start = 0usize;
    while command_start < args.len() {
        let arg = args[command_start].clone();
        if !arg.starts_with('-') {
            break;
        }
        match arg.as_str() {
            "--socket" => {
                let value = args
                    .get(command_start + 1)
                    .ok_or_else(|| anyhow!("--socket requires a value"))?;
                socket = Some(PathBuf::from(value));
                command_start += 2;
            }
            "--socket-mode" => {
                let value = args
                    .get(command_start + 1)
                    .ok_or_else(|| anyhow!("--socket-mode requires runtime|debug"))?;
                socket_mode = match value.as_str() {
                    "runtime" => SocketMode::Runtime,
                    "debug" => SocketMode::Debug,
                    _ => bail!("--socket-mode must be runtime or debug"),
                };
                command_start += 2;
            }
            "--json" => {
                json_output = true;
                command_start += 1;
            }
            "--id-format" => {
                let value = args
                    .get(command_start + 1)
                    .ok_or_else(|| anyhow!("--id-format requires refs|both|uuids"))?;
                id_format = IdFormat::parse(value)?;
                command_start += 2;
            }
            "--request" => {
                let value = args
                    .get(command_start + 1)
                    .ok_or_else(|| anyhow!("--request requires a JSON value"))?;
                request = Some(value.clone());
                command_start += 2;
            }
            "--pretty" => {
                pretty = true;
                command_start += 1;
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => break,
        }
    }

    let command_args = args.split_off(command_start);

    Ok(GlobalOptions {
        socket,
        socket_mode,
        json_output,
        id_format,
        request,
        pretty,
        command_args,
    })
}

fn print_help() {
    println!(
        concat!(
            "limux CLI\n\n",
            "Usage: limux [--socket <path>] [--json] [--id-format refs|both|uuids] <command> [args...]\n",
            "       limux\n\n",
            "Running `limux` with no arguments launches the GTK app.\n\n",
            "Common commands:\n",
            "  identify [--workspace <id|ref>] [--surface <id|ref>]\n",
            "  list-panels [--workspace <id|ref>]\n",
            "  list-panes [--workspace <id|ref>]\n",
            "  list-workspaces\n",
            "  surface-health [--workspace <id|ref>]\n",
            "  send [--workspace <id|ref>] [--surface <id|ref>] <text>\n",
            "  send-key [--workspace <id|ref>] [--surface <id|ref>] <key>\n",
            "  new-workspace [--cwd <path>] [--command <text>]\n",
            "  commands [list|run <name>] [--project <path>] [--config <path>]\n",
            "  run-command [--project <path>] [--config <path>] [--cwd <path>] [--name <workspace>] <name>\n",
            "  ssh [--cwd <path>] [--name <workspace-name>] [--] <ssh-args...>\n",
            "  close-workspace --workspace <id|ref>\n",
            "  sidebar-state --workspace <id|ref>\n",
            "  new-surface [--workspace <id|ref>]\n",
            "  new-pane [--workspace <id|ref>] [--pane <id|ref>] [--surface <id|ref>] [--direction <left|right|up|down>] [--type <terminal|browser>] [--command <text>] [--url <url>]\n",
            "      Live GTK self-spawn currently supports terminal panes only; browser panes remain deferred.\n",
            "  rename-workspace [--workspace <id|ref>] <title>\n",
            "  rename-window [--workspace <id|ref>] <title>\n",
            "  rename-tab [--workspace <id|ref>] [--tab <id|ref>] <title>\n",
            "  read-screen [--workspace <id|ref>] [--surface <id|ref>] [--scrollback] [--lines <n>]\n",
            "  capture-pane (alias of read-screen)\n",
            "  tab-action --action <name> [--workspace <id|ref>] [--tab <id|ref>] [--title <text>] [--url <url>]\n",
            "  browser [--surface <id|ref>|<surface>] <subcommand> ...\n",
            "      browser profiles [--browser <name>] [--include-missing]\n",
            "      browser import-cookies --file <path> [--format auto|json|netscape]\n",
            "  list-notifications [--unread]\n",
            "  clear-notifications [--id <notification-id>]\n",
            "  jump-notification [--id <notification-id>]\n\n",
            "Agent integrations:\n",
            "  notify [--workspace <id|ref>] [--subtitle <text>] [--body <text>] <title>\n",
            "  hooks setup [agent] | hooks uninstall [agent] | hooks <agent> <event>\n",
            "  claude-hook | opencode-hook | gemini-hook --event <name> [--subtitle <text>] [--body <text>] [--title <text>]\n",
            "  agent-team [--agents codex,claude[,opencode,gemini]] [--cwd <path>] [--no-launch] [--dry-run]\n",
            "      Splits the active workspace into one pane per agent (caller's pane stays\n",
            "      as the orchestrator on the left, peers stack down the right), launches\n",
            "      each CLI in its pane, and writes AGENTS.md describing the <agent-msg>\n",
            "      XML protocol so peers can talk via\n",
            "      `limux send --surface <peer-surface-id> <envelope>`.\n"
        )
    );
}

fn should_launch_host(opts: &GlobalOptions) -> bool {
    opts.command_args.is_empty()
        && opts.request.is_none()
        && opts.socket.is_none()
        && opts.socket_mode == SocketMode::Runtime
        && !opts.json_output
        && !opts.pretty
        && opts.id_format == IdFormat::Refs
}

fn host_binary_candidates(exe: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(bin_dir) = exe.parent() {
        if let Some(prefix) = bin_dir.parent() {
            candidates.push(prefix.join("libexec/limux/limux-host"));
        }

        let sibling_host = bin_dir.join("limux-host");
        if sibling_host != exe {
            candidates.push(sibling_host);
        }

        let sibling_dev_host = bin_dir.join("limux");
        if sibling_dev_host != exe {
            candidates.push(sibling_dev_host);
        }
    }

    candidates
}

fn resolve_host_binary() -> Result<PathBuf> {
    if let Ok(raw) = env::var("LIMUX_HOST_BIN") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Ok(path);
        }
    }

    let exe = env::current_exe().context("failed to resolve current executable")?;
    host_binary_candidates(&exe)
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            anyhow!(
                "could not find limux host binary; expected limux-host next to the installed CLI"
            )
        })
}

fn launch_host() -> Result<()> {
    let host = resolve_host_binary()?;
    let err = Command::new(&host)
        .spawn()
        .with_context(|| format!("failed to launch {}", host.display()))?
        .wait()
        .with_context(|| format!("failed to wait for {}", host.display()))?;
    if err.success() {
        Ok(())
    } else {
        bail!("{} exited with {}", host.display(), err)
    }
}

fn get_string(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(raw) = value.get(*key) {
            match raw {
                Value::String(s) if !s.is_empty() => return Some(s.clone()),
                Value::Number(n) => return Some(n.to_string()),
                _ => {}
            }
        }
    }
    None
}

fn handle_from_payload(value: &Value, id_key: &str, ref_key: &str) -> String {
    get_string(value, &[ref_key])
        .or_else(|| get_string(value, &[id_key]))
        .unwrap_or_default()
}

fn apply_id_format(value: &mut Value, id_format: IdFormat) {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in &keys {
                if key.ends_with("_id") {
                    let prefix = key.trim_end_matches("_id");
                    let ref_key = format!("{}_ref", prefix);
                    match id_format {
                        IdFormat::Refs => {
                            if map.contains_key(&ref_key) {
                                map.remove(key);
                            }
                        }
                        IdFormat::Uuids => {
                            if map.contains_key(key) {
                                map.remove(&ref_key);
                            }
                        }
                        IdFormat::Both => {}
                    }
                }
            }

            match id_format {
                IdFormat::Refs => {
                    if map.contains_key("ref") {
                        map.remove("id");
                    }
                }
                IdFormat::Uuids => {
                    if map.contains_key("id") {
                        map.remove("ref");
                    }
                }
                IdFormat::Both => {}
            }

            let child_keys: Vec<String> = map.keys().cloned().collect();
            for key in child_keys {
                if let Some(child) = map.get_mut(&key) {
                    apply_id_format(child, id_format);
                }
            }
        }
        Value::Array(list) => {
            for item in list {
                apply_id_format(item, id_format);
            }
        }
        _ => {}
    }
}

fn parse_opt(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find_map(|w| {
        if w[0] == name {
            Some(w[1].clone())
        } else {
            None
        }
    })
}

fn parse_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserCookieImportFormat {
    Auto,
    Json,
    Netscape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserCookieImportRow {
    name: String,
    value: String,
    domain: Option<String>,
    path: Option<String>,
    http_only: bool,
    secure: bool,
    expires_unix: Option<i64>,
}

impl BrowserCookieImportRow {
    fn to_import_json(&self) -> Value {
        json!({
            "name": self.name.clone(),
            "value": self.value.clone(),
            "domain": self.domain.clone(),
            "path": self.path.clone(),
            "http_only": self.http_only,
            "secure": self.secure,
            "expires_unix": self.expires_unix,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserProfileDiscoveryDirs {
    home_dir: Option<PathBuf>,
    config_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserProfileFamily {
    Chromium,
    Firefox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserProfileCandidate {
    id: &'static str,
    name: &'static str,
    family: BrowserProfileFamily,
    root: PathBuf,
}

fn browser_profile_discovery_dirs() -> BrowserProfileDiscoveryDirs {
    BrowserProfileDiscoveryDirs {
        home_dir: dirs::home_dir(),
        config_dir: dirs::config_dir(),
    }
}

fn browser_profile_candidates_in(
    dirs: &BrowserProfileDiscoveryDirs,
) -> Vec<BrowserProfileCandidate> {
    let mut candidates = Vec::new();
    if let Some(config_dir) = &dirs.config_dir {
        for (id, name, relative) in [
            ("google-chrome", "Google Chrome", "google-chrome"),
            (
                "google-chrome-beta",
                "Google Chrome Beta",
                "google-chrome-beta",
            ),
            (
                "google-chrome-unstable",
                "Google Chrome Unstable",
                "google-chrome-unstable",
            ),
            ("chromium", "Chromium", "chromium"),
            ("brave", "Brave", "BraveSoftware/Brave-Browser"),
            ("microsoft-edge", "Microsoft Edge", "microsoft-edge"),
            (
                "microsoft-edge-beta",
                "Microsoft Edge Beta",
                "microsoft-edge-beta",
            ),
            ("vivaldi", "Vivaldi", "vivaldi"),
        ] {
            candidates.push(BrowserProfileCandidate {
                id,
                name,
                family: BrowserProfileFamily::Chromium,
                root: config_dir.join(relative),
            });
        }
    }
    if let Some(home_dir) = &dirs.home_dir {
        candidates.push(BrowserProfileCandidate {
            id: "firefox",
            name: "Firefox",
            family: BrowserProfileFamily::Firefox,
            root: home_dir.join(".mozilla/firefox"),
        });
    }
    candidates
}

fn browser_filter_matches(candidate: &BrowserProfileCandidate, filter: Option<&str>) -> bool {
    let Some(filter) = filter.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let filter = filter.to_ascii_lowercase();
    candidate.id.contains(&filter)
        || candidate.name.to_ascii_lowercase().contains(&filter)
        || (filter == "chrome" && candidate.id.starts_with("google-chrome"))
        || (filter == "edge" && candidate.id.starts_with("microsoft-edge"))
}

fn path_state(path: PathBuf) -> Value {
    json!({
        "path": path.display().to_string(),
        "exists": path.exists(),
    })
}

fn browser_missing_candidate_payload(candidate: &BrowserProfileCandidate) -> Value {
    json!({
        "browser": candidate.id,
        "browser_name": candidate.name,
        "profile_root": candidate.root.display().to_string(),
        "reason": "profile root not found",
    })
}

fn chromium_profile_sort_key(path: &Path) -> (u8, String) {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let rank = if name == "Default" {
        0
    } else if name.starts_with("Profile ") {
        1
    } else {
        2
    };
    (rank, name)
}

fn looks_like_chromium_profile(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    name == "Default"
        || name.starts_with("Profile ")
        || name == "Guest Profile"
        || path.join("Preferences").exists()
        || path.join("Network/Cookies").exists()
        || path.join("History").exists()
}

fn discover_chromium_profiles(candidate: &BrowserProfileCandidate) -> Vec<Value> {
    let mut profile_paths: Vec<PathBuf> = fs::read_dir(&candidate.root)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(|entry| entry.ok()))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && looks_like_chromium_profile(path))
        .collect();
    profile_paths.sort_by_key(|path| chromium_profile_sort_key(path));

    profile_paths
        .into_iter()
        .map(|profile_path| {
            let profile_name = profile_path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string();
            let cookie_store = profile_path.join("Network/Cookies");
            json!({
                "browser": candidate.id,
                "browser_name": candidate.name,
                "profile": profile_name,
                "profile_path": profile_path.display().to_string(),
                "profile_root": candidate.root.display().to_string(),
                "family": "chromium",
                "cookie_store": path_state(cookie_store),
                "legacy_cookie_store": path_state(profile_path.join("Cookies")),
                "history_store": path_state(profile_path.join("History")),
                "session_store": path_state(profile_path.join("Sessions")),
            })
        })
        .collect()
}

fn parse_firefox_profiles_ini(root: &Path, raw: &str) -> Vec<(String, PathBuf)> {
    let mut profiles = Vec::new();
    let mut section = String::new();
    let mut values = BTreeMap::<String, String>::new();

    fn flush_firefox_profile(
        profiles: &mut Vec<(String, PathBuf)>,
        root: &Path,
        section: &str,
        values: &BTreeMap<String, String>,
    ) {
        if !section.starts_with("Profile") {
            return;
        }
        let Some(raw_path) = values.get("Path").filter(|value| !value.is_empty()) else {
            return;
        };
        let is_relative = values.get("IsRelative").map(String::as_str).unwrap_or("1") != "0";
        let path = if is_relative {
            root.join(raw_path)
        } else {
            PathBuf::from(raw_path)
        };
        let name = values
            .get("Name")
            .cloned()
            .or_else(|| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| raw_path.to_string());
        profiles.push((name, path));
    }

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            flush_firefox_profile(&mut profiles, root, &section, &values);
            section = line
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
            values.clear();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            values.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    flush_firefox_profile(&mut profiles, root, &section, &values);
    profiles
}

fn discover_firefox_profiles(candidate: &BrowserProfileCandidate) -> Vec<Value> {
    let mut profiles = if let Ok(raw) = fs::read_to_string(candidate.root.join("profiles.ini")) {
        parse_firefox_profiles_ini(&candidate.root, &raw)
    } else {
        Vec::new()
    };

    if profiles.is_empty() {
        profiles = fs::read_dir(&candidate.root)
            .ok()
            .into_iter()
            .flat_map(|entries| entries.filter_map(|entry| entry.ok()))
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .filter(|path| {
                path.join("cookies.sqlite").exists() || path.join("places.sqlite").exists()
            })
            .map(|path| {
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_string();
                (name, path)
            })
            .collect();
    }

    profiles.sort_by(|left, right| left.0.cmp(&right.0));
    profiles
        .into_iter()
        .map(|(profile_name, profile_path)| {
            json!({
                "browser": candidate.id,
                "browser_name": candidate.name,
                "profile": profile_name,
                "profile_path": profile_path.display().to_string(),
                "profile_root": candidate.root.display().to_string(),
                "family": "firefox",
                "cookie_store": path_state(profile_path.join("cookies.sqlite")),
                "history_store": path_state(profile_path.join("places.sqlite")),
                "session_store": path_state(profile_path.join("sessionstore.jsonlz4")),
                "session_backups": path_state(profile_path.join("sessionstore-backups")),
            })
        })
        .collect()
}

fn browser_profiles_payload(filter: Option<&str>, include_missing: bool) -> Value {
    browser_profiles_payload_in(&browser_profile_discovery_dirs(), filter, include_missing)
}

fn browser_profiles_payload_in(
    dirs: &BrowserProfileDiscoveryDirs,
    filter: Option<&str>,
    include_missing: bool,
) -> Value {
    let mut profiles = Vec::new();
    let mut missing_browsers = Vec::new();

    for candidate in browser_profile_candidates_in(dirs)
        .into_iter()
        .filter(|candidate| browser_filter_matches(candidate, filter))
    {
        if !candidate.root.exists() {
            if include_missing {
                missing_browsers.push(browser_missing_candidate_payload(&candidate));
            }
            continue;
        }
        let before = profiles.len();
        match candidate.family {
            BrowserProfileFamily::Chromium => {
                profiles.extend(discover_chromium_profiles(&candidate))
            }
            BrowserProfileFamily::Firefox => profiles.extend(discover_firefox_profiles(&candidate)),
        }
        if include_missing && before == profiles.len() {
            missing_browsers.push(json!({
                "browser": candidate.id,
                "browser_name": candidate.name,
                "profile_root": candidate.root.display().to_string(),
                "reason": "profile root exists but no profiles were found",
            }));
        }
    }

    json!({
        "ok": true,
        "profile_count": profiles.len(),
        "profiles": profiles,
        "missing_browsers": missing_browsers,
        "note": "Profile discovery reports local store paths only; cookie database extraction/decryption, history import, and session import remain open cmux-parity work.",
    })
}

impl BrowserCookieImportFormat {
    fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.unwrap_or("auto") {
            "auto" => Ok(Self::Auto),
            "json" => Ok(Self::Json),
            "netscape" | "cookies.txt" | "txt" => Ok(Self::Netscape),
            other => {
                bail!("browser import-cookies --format must be auto|json|netscape, got {other}")
            }
        }
    }

    fn resolve_for_path(self, path: &Path) -> Self {
        match self {
            Self::Auto => match path.extension().and_then(|ext| ext.to_str()) {
                Some(ext) if ext.eq_ignore_ascii_case("json") => Self::Json,
                _ => Self::Netscape,
            },
            other => other,
        }
    }
}

fn is_unknown_method_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("-32601")
        || message.contains("unknown method")
        || message.contains("method not found")
}

fn load_browser_cookie_import_file(
    path: &Path,
    format: BrowserCookieImportFormat,
) -> Result<Vec<BrowserCookieImportRow>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read browser cookie file {}", path.display()))?;
    match format.resolve_for_path(path) {
        BrowserCookieImportFormat::Json => parse_browser_cookie_import_json(&raw, path),
        BrowserCookieImportFormat::Netscape => parse_browser_cookie_import_netscape(&raw, path),
        BrowserCookieImportFormat::Auto => unreachable!("auto format should resolve"),
    }
}

fn parse_browser_cookie_import_json(raw: &str, path: &Path) -> Result<Vec<BrowserCookieImportRow>> {
    let value: Value = serde_json::from_str(raw)
        .with_context(|| format!("browser cookie file {} is not valid JSON", path.display()))?;
    let rows = match &value {
        Value::Array(rows) => rows,
        Value::Object(map) => map
            .get("cookies")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                anyhow!("JSON cookie import expects an array or object with cookies[]")
            })?,
        _ => bail!("JSON cookie import expects an array or object with cookies[]"),
    };

    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            json_cookie_import_row(row).with_context(|| format!("cookies[{index}]"))
        })
        .collect()
}

fn get_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        let value = value.get(*key)?;
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|raw| i64::try_from(raw).ok()))
            .or_else(|| value.as_f64().map(|raw| raw as i64))
            .or_else(|| value.as_str().and_then(|raw| raw.parse::<i64>().ok()))
    })
}

fn json_cookie_import_row(value: &Value) -> Result<BrowserCookieImportRow> {
    let map = value
        .as_object()
        .ok_or_else(|| anyhow!("cookie row must be an object"))?;
    let name =
        get_string(value, &["name", "key"]).ok_or_else(|| anyhow!("cookie row is missing name"))?;
    let cookie_value =
        get_string(value, &["value"]).ok_or_else(|| anyhow!("cookie row is missing value"))?;
    let domain = get_string(value, &["domain", "host"]);
    let path = get_string(value, &["path"]);
    let http_only = map
        .get("httpOnly")
        .or_else(|| map.get("http_only"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let secure = map.get("secure").and_then(Value::as_bool).unwrap_or(false);
    let expires_unix = get_i64(
        value,
        &[
            "expires_unix",
            "expires",
            "expiry",
            "expiration",
            "expirationDate",
            "expiration_date",
        ],
    )
    .filter(|value| *value > 0);

    Ok(BrowserCookieImportRow {
        name,
        value: cookie_value,
        domain,
        path,
        http_only,
        secure,
        expires_unix,
    })
}

fn parse_browser_cookie_import_netscape(
    raw: &str,
    path: &Path,
) -> Result<Vec<BrowserCookieImportRow>> {
    let mut rows = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (http_only, body) = if let Some(rest) = trimmed.strip_prefix("#HttpOnly_") {
            (true, rest)
        } else if trimmed.starts_with('#') {
            continue;
        } else {
            (false, trimmed)
        };

        let parts = body.split('\t').collect::<Vec<_>>();
        if parts.len() < 7 {
            bail!(
                "{}:{} is not a valid Netscape cookie row",
                path.display(),
                line_number
            );
        }
        let name = parts[5].trim();
        let value = parts[6].trim();
        if name.is_empty() {
            bail!(
                "{}:{} has an empty cookie name",
                path.display(),
                line_number
            );
        }
        rows.push(BrowserCookieImportRow {
            name: name.to_string(),
            value: value.to_string(),
            domain: Some(parts[0].trim().to_string()).filter(|value| !value.is_empty()),
            path: Some(parts[2].trim().to_string()).filter(|value| !value.is_empty()),
            http_only,
            secure: parts[3].eq_ignore_ascii_case("TRUE"),
            expires_unix: parts[4]
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|value| *value > 0),
        });
    }
    Ok(rows)
}

fn positional_arg(args: &[String], index: usize) -> Option<String> {
    let mut position = 0usize;
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == "--agent" {
            skip = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if position == index {
            return Some(arg.clone());
        }
        position += 1;
    }
    None
}

fn trailing_title(args: &[String]) -> Option<String> {
    let mut filtered: Vec<String> = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == "--workspace"
            || arg == "--tab"
            || arg == "--surface"
            || arg == "--pane"
            || arg == "--target-pane"
            || arg == "--action"
            || arg == "--title"
            || arg == "--url"
            || arg == "--cwd"
            || arg == "--command"
            || arg == "--direction"
            || arg == "--type"
            || arg == "--lines"
            || arg == "--timeout"
            || arg == "--timeout-ms"
            || arg == "--name"
            || arg == "--out"
            || arg == "--subtitle"
            || arg == "--body"
            || arg == "--message"
            || arg == "--event"
            || arg == "--agents"
            || arg == "--selector"
            || arg == "--text"
            || arg == "--attr"
            || arg == "--property"
            || arg == "--value"
            || arg == "--amount"
            || arg == "--unset"
            || arg == "--project"
            || arg == "--config"
        {
            skip = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        filtered.push(arg.clone());
    }
    if filtered.is_empty() {
        None
    } else {
        Some(filtered.join(" "))
    }
}

fn wait_signal_path(name: &str) -> PathBuf {
    let sanitized: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    PathBuf::from(format!("/tmp/limux-wait-for-{}.sig", sanitized))
}

const PROJECT_COMMAND_CONFIG_FILES: [&str; 2] = ["cmux.json", "limux.json"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectCommandDefinition {
    name: String,
    label: String,
    command: String,
    cwd: Option<String>,
    source: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectCommandWorkspaceRequest {
    command_name: String,
    workspace_name: String,
    command: String,
    cwd: String,
    config: PathBuf,
}

fn nonempty_trimmed(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn json_string_for_keys(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match map.get(*key) {
        Some(Value::String(value)) => nonempty_trimmed(value),
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    })
}

fn command_string_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => nonempty_trimmed(value),
        Value::Array(items) => {
            let mut args = Vec::new();
            for item in items {
                let Value::String(value) = item else {
                    return None;
                };
                args.push(shell_single_quote(value));
            }
            if args.is_empty() {
                None
            } else {
                Some(args.join(" "))
            }
        }
        Value::Object(map) => ["command", "cmd", "run", "script", "shell"]
            .iter()
            .find_map(|key| map.get(*key).and_then(command_string_from_value)),
        _ => None,
    }
}

fn parse_project_command_entry(
    name_hint: Option<&str>,
    value: &Value,
    source: &Path,
) -> Result<ProjectCommandDefinition> {
    if let Value::String(command) = value {
        let name = name_hint
            .and_then(nonempty_trimmed)
            .ok_or_else(|| anyhow!("string project commands require a map key name"))?;
        let command = nonempty_trimmed(command)
            .ok_or_else(|| anyhow!("project command `{name}` has an empty command"))?;
        return Ok(ProjectCommandDefinition {
            label: name.clone(),
            name,
            command,
            cwd: None,
            source: source.to_path_buf(),
        });
    }

    let Value::Object(map) = value else {
        bail!("project command entries must be strings or objects");
    };

    let command = command_string_from_value(value).ok_or_else(|| {
        anyhow!(
            "project command `{}` is missing command/cmd/run/script/shell",
            name_hint.unwrap_or("<unnamed>")
        )
    })?;
    let name = json_string_for_keys(map, &["id", "name", "key"])
        .or_else(|| name_hint.and_then(nonempty_trimmed))
        .or_else(|| json_string_for_keys(map, &["label", "title"]))
        .ok_or_else(|| anyhow!("project command object is missing a name or id"))?;
    let label =
        json_string_for_keys(map, &["label", "title", "name"]).unwrap_or_else(|| name.clone());
    let cwd = json_string_for_keys(map, &["cwd", "working_directory", "workingDir"]);

    Ok(ProjectCommandDefinition {
        name,
        label,
        command,
        cwd,
        source: source.to_path_buf(),
    })
}

fn parse_project_commands_from_value(
    value: &Value,
    source: &Path,
) -> Result<Vec<ProjectCommandDefinition>> {
    let root = value
        .as_object()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", source.display()))?;
    let commands = root
        .get("commands")
        .or_else(|| root.get("customCommands"))
        .ok_or_else(|| {
            anyhow!(
                "{} does not contain a commands object or array",
                source.display()
            )
        })?;

    let mut parsed = Vec::new();
    match commands {
        Value::Object(map) => {
            for (name, value) in map {
                parsed.push(parse_project_command_entry(Some(name), value, source)?);
            }
        }
        Value::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                parsed.push(
                    parse_project_command_entry(None, value, source)
                        .with_context(|| format!("invalid project command at commands[{index}]"))?,
                );
            }
        }
        _ => bail!("{} commands must be an object or array", source.display()),
    }
    Ok(parsed)
}

fn load_project_commands_from_path(path: &Path) -> Result<Vec<ProjectCommandDefinition>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read project command config {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw).with_context(|| {
        format!(
            "project command config {} is not valid JSON",
            path.display()
        )
    })?;
    parse_project_commands_from_value(&value, path)
}

fn find_project_command_config_in(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };

    loop {
        for file_name in PROJECT_COMMAND_CONFIG_FILES {
            let candidate = current.join(file_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if !current.pop() {
            return None;
        }
    }
}

fn project_command_config_path(args: &[String]) -> Result<PathBuf> {
    if let Some(raw) = parse_opt(args, "--config") {
        return Ok(PathBuf::from(raw));
    }
    let start = parse_opt(args, "--project")
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(env::current_dir)
        .context("failed to resolve current directory")?;
    find_project_command_config_in(&start).ok_or_else(|| {
        anyhow!(
            "no cmux.json or limux.json found from {} upward",
            start.display()
        )
    })
}

fn project_command_arg(args: &[String]) -> Option<String> {
    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];
        if matches!(arg.as_str(), "--project" | "--config" | "--cwd" | "--name") {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return nonempty_trimmed(arg);
    }
    None
}

fn resolved_project_command_cwd(
    definition: &ProjectCommandDefinition,
    override_cwd: Option<String>,
) -> String {
    let raw = override_cwd
        .or_else(|| definition.cwd.clone())
        .unwrap_or_else(|| {
            definition
                .source
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_string_lossy()
                .to_string()
        });
    let path = PathBuf::from(&raw);
    if path.is_absolute() {
        raw
    } else {
        definition
            .source
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
            .to_string_lossy()
            .to_string()
    }
}

fn project_commands_payload(config: &Path, commands: &[ProjectCommandDefinition]) -> Value {
    let rows = commands
        .iter()
        .map(|command| {
            json!({
                "name": command.name.clone(),
                "label": command.label.clone(),
                "command": command.command.clone(),
                "cwd": resolved_project_command_cwd(command, None),
                "config": config.to_string_lossy().to_string(),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "config": config.to_string_lossy().to_string(),
        "commands": rows,
    })
}

fn project_commands_text(payload: &Value) -> String {
    let Some(commands) = payload.get("commands").and_then(Value::as_array) else {
        return "none".to_string();
    };
    if commands.is_empty() {
        return "none".to_string();
    }
    commands
        .iter()
        .map(|command| {
            let name = get_string(command, &["name"]).unwrap_or_else(|| "<unnamed>".to_string());
            let label = get_string(command, &["label"]).unwrap_or_else(|| name.clone());
            let cwd = get_string(command, &["cwd"]).unwrap_or_else(|| "none".to_string());
            let body = get_string(command, &["command"]).unwrap_or_default();
            format!("name={name} label={label} cwd={cwd} command={body}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn load_project_commands_for_args(
    args: &[String],
) -> Result<(PathBuf, Vec<ProjectCommandDefinition>)> {
    let config = project_command_config_path(args)?;
    let commands = load_project_commands_from_path(&config)?;
    Ok((config, commands))
}

fn build_project_command_workspace_request(
    args: &[String],
) -> Result<ProjectCommandWorkspaceRequest> {
    let command_name =
        project_command_arg(args).ok_or_else(|| anyhow!("run-command requires a command name"))?;
    let (config, commands) = load_project_commands_for_args(args)?;
    let definition = commands
        .iter()
        .find(|command| command.name == command_name || command.label == command_name)
        .ok_or_else(|| {
            anyhow!(
                "project command `{command_name}` not found in {}",
                config.display()
            )
        })?;
    let workspace_name = parse_opt(args, "--name")
        .and_then(|value| nonempty_trimmed(&value))
        .unwrap_or_else(|| definition.label.clone());
    let cwd = resolved_project_command_cwd(definition, parse_opt(args, "--cwd"));
    Ok(ProjectCommandWorkspaceRequest {
        command_name: definition.name.clone(),
        workspace_name,
        command: definition.command.clone(),
        cwd,
        config,
    })
}

fn read_json_map(path: &str) -> BTreeMap<String, String> {
    let raw = fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str::<BTreeMap<String, String>>(&raw).unwrap_or_default()
}

fn write_json_map(path: &Path, map: &BTreeMap<String, String>) -> Result<()> {
    let encoded = serde_json::to_string_pretty(map).context("failed to encode json map")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), nonce));
    fs::write(&tmp, encoded).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn socket_state_namespace(socket: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    socket.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn cli_state_dir(socket: &Path) -> PathBuf {
    env::temp_dir()
        .join("limux-cli")
        .join(socket_state_namespace(socket))
}

fn cli_state_path(socket: &Path, kind: &str) -> PathBuf {
    cli_state_dir(socket).join(format!("{kind}.json"))
}

fn cli_state_lock_path(socket: &Path, kind: &str) -> PathBuf {
    cli_state_dir(socket).join(format!("{kind}.lock"))
}

struct CliStateLock {
    path: PathBuf,
}

impl Drop for CliStateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_cli_state_lock(socket: &Path, kind: &str) -> Result<CliStateLock> {
    let dir = cli_state_dir(socket);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let lock_path = cli_state_lock_path(socket, kind);
    let deadline = Instant::now() + CLI_STATE_LOCK_TIMEOUT;
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => return Ok(CliStateLock { path: lock_path }),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    bail!("timed out acquiring CLI state lock {}", lock_path.display());
                }
                std::thread::sleep(CLI_STATE_LOCK_RETRY);
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed to create CLI state lock {}", lock_path.display())
                });
            }
        }
    }
}

fn with_locked_json_map<T, F>(socket: &Path, kind: &str, update: F) -> Result<T>
where
    F: FnOnce(&mut BTreeMap<String, String>, &Path) -> Result<T>,
{
    let _lock = acquire_cli_state_lock(socket, kind)?;
    let path = cli_state_path(socket, kind);
    let path_str = path.to_string_lossy().to_string();
    let mut map = read_json_map(&path_str);
    update(&mut map, &path)
}

async fn resolve_current_workspace(client: &mut Client) -> Result<String> {
    let current = client.call("workspace.current", json!({})).await?;
    get_string(&current, &["workspace_id", "workspace_ref"])
        .ok_or_else(|| anyhow!("workspace.current returned no workspace handle"))
}

async fn call_in_workspace_scope(
    client: &mut Client,
    workspace: Option<String>,
    method: &str,
    params: Value,
) -> Result<Value> {
    if let Some(target) = workspace {
        let mut map = match params {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            _ => bail!("{method} requires object params for workspace-scoped calls"),
        };
        map.entry("workspace_id".to_string())
            .or_insert(Value::String(target));
        return client.call(method, Value::Object(map)).await;
    }
    client.call(method, params).await
}

async fn browser_call(
    client: &mut Client,
    surface: Option<String>,
    method: &str,
    mut params: Map<String, Value>,
) -> Result<Value> {
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }
    client.call(method, Value::Object(params)).await
}

async fn selected_surface_for_pane(
    client: &mut Client,
    workspace: Option<String>,
    pane_id: &str,
) -> Result<String> {
    let payload = call_in_workspace_scope(
        client,
        workspace,
        "pane.surfaces",
        json!({ "pane_id": pane_id }),
    )
    .await?;
    let rows = payload
        .get("surfaces")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("pane.surfaces returned no surfaces"))?;

    for row in rows {
        let focused = row.get("focused").and_then(Value::as_bool).unwrap_or(false)
            || row
                .get("selected")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        if focused {
            let handle = handle_from_payload(row, "surface_id", "surface_ref");
            if !handle.is_empty() {
                return Ok(handle);
            }
        }
    }

    let first = rows
        .first()
        .ok_or_else(|| anyhow!("pane has no surfaces"))?;
    let handle = handle_from_payload(first, "surface_id", "surface_ref");
    if handle.is_empty() {
        bail!("pane.surfaces returned an empty surface handle");
    }
    Ok(handle)
}

async fn run_identify(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace");
    let surface = parse_opt(args, "--surface");
    let no_caller = parse_flag(args, "--no-caller");

    let mut params = Map::new();
    if workspace.is_some() || surface.is_some() {
        let mut caller = Map::new();
        if let Some(workspace) = workspace {
            caller.insert("workspace_id".to_string(), Value::String(workspace));
        }
        if let Some(surface) = surface {
            caller.insert("surface_id".to_string(), Value::String(surface));
        }
        params.insert("caller".to_string(), Value::Object(caller));
    }

    let mut payload = client
        .call("system.identify", Value::Object(params))
        .await?;
    if no_caller {
        if let Some(map) = payload.as_object_mut() {
            map.remove("caller");
        }
    }
    Ok(payload)
}

async fn run_list(client: &mut Client, command: &str, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| env::var("LIMUX_WORKSPACE_ID").ok())
        .filter(|value| !value.trim().is_empty());
    let params = if let Some(workspace) = workspace.as_ref() {
        json!({ "workspace_id": workspace })
    } else {
        json!({})
    };
    let method = match command {
        "list-panels" => "surface.list",
        "list-panes" => "pane.list",
        "list-workspaces" => "workspace.list",
        "surface-health" => "surface.health",
        _ => bail!("unsupported list command"),
    };
    let mut payload = client.call(method, params).await?;
    if let Some(workspace) = workspace.as_ref() {
        if let Some(map) = payload.as_object_mut() {
            if workspace.contains(':') {
                map.entry("workspace_ref".to_string())
                    .or_insert_with(|| Value::String(workspace.clone()));
            } else {
                map.entry("workspace_id".to_string())
                    .or_insert_with(|| Value::String(workspace.clone()));
            }
        }
    }
    Ok(payload)
}

fn render_list_text(command: &str, payload: &Value) -> String {
    match command {
        "list-panels" => {
            let rows = payload
                .get("surfaces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "No surfaces".to_string();
            }
            rows.iter()
                .map(|row| {
                    let handle = handle_from_payload(row, "surface_id", "surface_ref");
                    let title = get_string(row, &["title"]).unwrap_or_default();
                    format!("{} {}", handle, title)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "list-panes" => {
            let rows = payload
                .get("panes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "No panes".to_string();
            }
            rows.iter()
                .map(|row| {
                    let handle = handle_from_payload(row, "pane_id", "pane_ref");
                    let count = row
                        .get("surface_count")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    format!("{} surfaces={}", handle, count)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "list-workspaces" => {
            let rows = payload
                .get("workspaces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "No workspaces".to_string();
            }
            rows.iter()
                .map(|row| {
                    let handle = handle_from_payload(row, "workspace_id", "workspace_ref");
                    let title = get_string(row, &["title", "name"]).unwrap_or_default();
                    let selected = row
                        .get("selected")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if selected {
                        format!("* {} {}", handle, title)
                    } else {
                        format!("  {} {}", handle, title)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "surface-health" => {
            let rows = payload
                .get("surfaces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "No surfaces".to_string();
            }
            rows.iter()
                .map(|row| {
                    let handle = handle_from_payload(row, "surface_id", "surface_ref");
                    let healthy = row.get("healthy").and_then(Value::as_bool).unwrap_or(true);
                    format!("{} healthy={}", handle, healthy)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => "".to_string(),
    }
}

async fn run_send(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| env::var("LIMUX_WORKSPACE_ID").ok())
        .filter(|s| !s.is_empty());
    let surface = parse_opt(args, "--surface").filter(|s| !s.is_empty());

    let text = trailing_title(args).ok_or_else(|| anyhow!("send requires text"))?;

    let mut params = Map::new();
    params.insert("text".to_string(), Value::String(text));
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }

    call_in_workspace_scope(
        client,
        workspace,
        "surface.send_text",
        Value::Object(params),
    )
    .await
}

async fn run_send_key(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| env::var("LIMUX_WORKSPACE_ID").ok())
        .filter(|s| !s.is_empty());
    let surface = parse_opt(args, "--surface").filter(|s| !s.is_empty());
    let key = trailing_title(args).ok_or_else(|| anyhow!("send-key requires key"))?;

    let mut params = Map::new();
    params.insert("key".to_string(), Value::String(key));
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }

    call_in_workspace_scope(client, workspace, "surface.send_key", Value::Object(params)).await
}

async fn run_list_notifications(client: &mut Client, args: &[String]) -> Result<Value> {
    let mut params = Map::new();
    if parse_flag(args, "--unread") || parse_flag(args, "--unread-only") {
        params.insert("unread_only".to_string(), Value::Bool(true));
    }
    client
        .call("notification.list", Value::Object(params))
        .await
}

async fn run_clear_notifications(client: &mut Client, args: &[String]) -> Result<Value> {
    let mut params = Map::new();
    if let Some(id) = parse_opt(args, "--id").or_else(|| parse_opt(args, "--notification-id")) {
        let id = id
            .parse::<u64>()
            .map_err(|_| anyhow!("notification id must be a non-negative integer"))?;
        params.insert("id".to_string(), Value::Number(id.into()));
    }
    client
        .call("notification.clear", Value::Object(params))
        .await
}

async fn run_jump_notification(client: &mut Client, args: &[String]) -> Result<Value> {
    let mut params = Map::new();
    if let Some(id) = parse_opt(args, "--id").or_else(|| parse_opt(args, "--notification-id")) {
        let id = id
            .parse::<u64>()
            .map_err(|_| anyhow!("notification id must be a non-negative integer"))?;
        params.insert("id".to_string(), Value::Number(id.into()));
    }
    client
        .call("notification.jump", Value::Object(params))
        .await
}

fn notification_rows_text(payload: &Value) -> String {
    let rows = payload
        .get("notifications")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return "none".to_string();
    }

    rows.iter()
        .map(notification_row_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn notification_row_text(row: &Value) -> String {
    let id = row
        .get("id")
        .and_then(Value::as_u64)
        .map(|id| id.to_string())
        .or_else(|| get_string(row, &["id"]))
        .unwrap_or_else(|| "?".to_string());
    let unread = row.get("unread").and_then(Value::as_bool).unwrap_or(false);
    let workspace =
        get_string(row, &["workspace_ref", "workspace_id"]).unwrap_or_else(|| "none".to_string());
    let surface =
        get_string(row, &["surface_ref", "surface_id"]).unwrap_or_else(|| "none".to_string());
    let message = get_string(row, &["message", "title", "body"]).unwrap_or_default();
    format!(
        "id={} unread={} workspace={} surface={} message={}",
        id, unread, workspace, surface, message
    )
}

fn notification_jump_text(payload: &Value) -> String {
    payload
        .get("notification")
        .map(notification_row_text)
        .unwrap_or_else(|| "OK".to_string())
}

/// `limux notify` — post a notification into the sidebar + toast overlay.
///
/// Usage:
///   limux notify [--workspace <id|ref>] [--subtitle <text>] [--body <text>] <title>
///   limux notify --title "..." --subtitle "..." --body "..."
///
/// Mirrors the `cmux notify` shape (title / subtitle / body). Title is
/// required; subtitle and body are optional. Falls back to the current
/// workspace via LIMUX_WORKSPACE_ID when --workspace isn't given.
async fn run_notify(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| env::var("LIMUX_WORKSPACE_ID").ok())
        .filter(|s| !s.is_empty());

    // Title can be provided either via --title or as the trailing positional
    // (matching `limux send`'s ergonomics).
    let title = parse_opt(args, "--title")
        .or_else(|| trailing_title(args))
        .ok_or_else(|| anyhow!("notify requires a title"))?;

    let subtitle = parse_opt(args, "--subtitle").unwrap_or_default();
    let body = parse_opt(args, "--body")
        .or_else(|| parse_opt(args, "--message"))
        .unwrap_or_default();

    let mut params = Map::new();
    params.insert("title".to_string(), Value::String(title));
    if !subtitle.is_empty() {
        params.insert("subtitle".to_string(), Value::String(subtitle));
    }
    if !body.is_empty() {
        params.insert("body".to_string(), Value::String(body));
    }

    call_in_workspace_scope(
        client,
        workspace,
        "notification.create",
        Value::Object(params),
    )
    .await
}

// ---------------------------------------------------------------------------
// Agent hooks (claude-hook / opencode-hook / gemini-hook)
// ---------------------------------------------------------------------------
//
// These subcommands read a JSON hook event from stdin and translate it into
// a `notify` (and, eventually, log / progress) call so the GUI reflects
// agent activity in real time. Designed for direct wiring into Claude Code,
// OpenCode, and Gemini CLI's hook settings.
//
// Claude Code stdin schema (what we rely on):
//   {
//     "session_id": "...",
//     "transcript_path": "...",
//     "cwd": "...",
//     "hook_event_name": "Notification" | "Stop" | "SessionStart" | ...,
//     "message": "agent is waiting for input",     // Notification only
//     "tool_name": "...", "tool_input": {...},     // PreToolUse/PostToolUse
//     "tool_response": {...},                       // PostToolUse
//     "prompt": "..."                               // UserPromptSubmit
//   }
//
// OpenCode and Gemini use slightly different names; we fall back gracefully
// when fields are missing.

/// Pull a string field from the hook JSON, trying multiple keys.
fn hook_str<'a>(payload: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| payload.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn parse_hook_event(args: &[String], payload: &Value) -> String {
    parse_opt(args, "--event")
        .or_else(|| trailing_title(args))
        .or_else(|| hook_str(payload, &["hook_event_name", "event"]).map(str::to_owned))
        .unwrap_or_else(|| "event".to_string())
}

/// Run an agent hook: read JSON from stdin, synthesize a notification.
///
/// Args:
///   [event_name] — optional positional, e.g. "Notification", "Stop".
///                  If omitted, we read `hook_event_name` from the JSON.
async fn run_agent_hook(
    client: &mut Client,
    agent: agent_hooks::AgentKind,
    args: &[String],
) -> Result<Value> {
    use std::io::Read;

    // Read stdin (hook JSON). If stdin is empty or not JSON, treat as
    // minimal event so we still post *something*.
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let raw = raw.trim();
    let payload: Value = if raw.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(raw).unwrap_or_else(|_| json!({ "raw": raw }))
    };

    // Explicit --event or positional event beats the JSON field.
    let event = parse_hook_event(args, &payload);

    // Build a human-friendly title + body depending on event + agent.
    let agent_label = agent.label();
    persist_agent_hook_session(agent, args, &payload, &event)?;
    let (title, body) = match event.as_str() {
        "Notification" => (
            format!("{agent_label} needs you"),
            hook_str(&payload, &["message", "notification"])
                .unwrap_or("waiting for input")
                .to_owned(),
        ),
        "Stop" | "SubagentStop" => (
            format!("{agent_label} finished"),
            hook_str(&payload, &["message", "reason"])
                .unwrap_or("task complete")
                .to_owned(),
        ),
        "SessionStart" => (
            format!("{agent_label} session started"),
            hook_str(&payload, &["cwd", "source"])
                .unwrap_or("")
                .to_owned(),
        ),
        "SessionEnd" => (
            format!("{agent_label} session ended"),
            hook_str(&payload, &["reason"]).unwrap_or("").to_owned(),
        ),
        "PreToolUse" | "PostToolUse" => (
            format!(
                "{agent_label}: {}",
                hook_str(&payload, &["tool_name"]).unwrap_or("tool")
            ),
            hook_str(&payload, &["tool_input", "summary"])
                .unwrap_or("")
                .to_owned(),
        ),
        "UserPromptSubmit" => (
            format!("{agent_label}: new prompt"),
            hook_str(&payload, &["prompt"])
                .unwrap_or("")
                .chars()
                .take(120)
                .collect(),
        ),
        other => (
            format!("{agent_label}: {other}"),
            hook_str(&payload, &["message", "summary"])
                .unwrap_or("")
                .to_owned(),
        ),
    };

    let subtitle = hook_str(&payload, &["session_id"])
        .map(|s| {
            // Show only a short prefix of the session id to keep sidebar tidy.
            s.chars().take(8).collect::<String>()
        })
        .unwrap_or_default();

    let workspace = parse_opt(args, "--workspace")
        .or_else(|| env::var("LIMUX_WORKSPACE_ID").ok())
        .filter(|s| !s.is_empty());

    let mut params = Map::new();
    params.insert("title".to_string(), Value::String(title));
    if !subtitle.is_empty() {
        params.insert("subtitle".to_string(), Value::String(subtitle));
    }
    if !body.is_empty() {
        params.insert("body".to_string(), Value::String(body));
    }

    let _ = call_in_workspace_scope(
        client,
        workspace,
        "notification.create",
        Value::Object(params),
    )
    .await;

    Ok(agent_hook_output(&event, &payload))
}

fn agent_hook_output(event: &str, payload: &Value) -> Value {
    let canonical_event = canonical_hook_event_name(event);
    let mut output = Map::new();
    output.insert("continue".to_string(), Value::Bool(true));
    output.insert("suppressOutput".to_string(), Value::Bool(false));

    if matches!(canonical_event, Some("SessionStart" | "UserPromptSubmit")) {
        let mut specific = Map::new();
        specific.insert(
            "hookEventName".to_string(),
            Value::String(
                canonical_event
                    .expect("matched canonical event")
                    .to_string(),
            ),
        );
        if let Some(context) = hook_additional_context(payload) {
            specific.insert("additionalContext".to_string(), Value::String(context));
        }
        output.insert("hookSpecificOutput".to_string(), Value::Object(specific));
    }

    Value::Object(output)
}

fn canonical_hook_event_name(event: &str) -> Option<&'static str> {
    match event {
        "SessionStart" | "session-start" => Some("SessionStart"),
        "UserPromptSubmit" | "prompt-submit" => Some("UserPromptSubmit"),
        "Stop" | "stop" | "Notification" => Some("Stop"),
        "SessionEnd" | "session-end" => None,
        "Cleanup" | "cleanup" | "restore-exit" => None,
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentHookPersistenceAction {
    Upsert,
    Preserve,
    Remove,
}

fn agent_hook_persistence_action(event: &str) -> AgentHookPersistenceAction {
    match event {
        "Cleanup" | "cleanup" | "restore-exit" => AgentHookPersistenceAction::Remove,
        "SessionEnd" | "session-end" => AgentHookPersistenceAction::Preserve,
        _ => AgentHookPersistenceAction::Upsert,
    }
}

fn hook_additional_context(payload: &Value) -> Option<String> {
    hook_str(payload, &["additional_context", "additionalContext"])
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
}

fn persist_agent_hook_session(
    agent: agent_hooks::AgentKind,
    args: &[String],
    payload: &Value,
    event: &str,
) -> Result<()> {
    let Some(session_id) = hook_session_id(payload) else {
        write_agent_hook_debug(
            agent,
            event,
            "skip_missing_session_id",
            &json!({
                "payload_keys": payload_keys(payload),
                "has_claude_code_session_env": limux_env_value("CLAUDE_CODE_SESSION_ID").is_some(),
                "has_claude_session_env": limux_env_value("CLAUDE_SESSION_ID").is_some(),
            }),
        );
        return Ok(());
    };

    let store = agent_hooks::AgentHookSessionStore::new(agent);
    match agent_hook_persistence_action(event) {
        AgentHookPersistenceAction::Remove => {
            let result = store.remove(&session_id);
            if result.is_ok() {
                write_agent_hook_debug(
                    agent,
                    event,
                    "removed",
                    &json!({
                        "session_id": session_id,
                        "payload_keys": payload_keys(payload),
                    }),
                );
            }
            return result;
        }
        AgentHookPersistenceAction::Preserve => {
            write_agent_hook_debug(
                agent,
                event,
                "preserved",
                &json!({
                    "session_id": session_id,
                    "payload_keys": payload_keys(payload),
                }),
            );
            return Ok(());
        }
        AgentHookPersistenceAction::Upsert => {}
    }

    let workspace_id = parse_opt(args, "--workspace")
        .or_else(|| limux_env_value("LIMUX_WORKSPACE_ID"))
        .filter(|value| !value.trim().is_empty());
    let surface_id = parse_opt(args, "--surface")
        .or_else(|| limux_env_value("LIMUX_SURFACE_ID"))
        .filter(|value| !value.trim().is_empty());
    let (Some(workspace_id), Some(surface_id)) = (workspace_id, surface_id) else {
        write_agent_hook_debug(
            agent,
            event,
            "skip_missing_limux_target",
            &json!({
                "session_id": session_id,
                "has_workspace_arg": parse_opt(args, "--workspace").is_some(),
                "has_surface_arg": parse_opt(args, "--surface").is_some(),
                "has_workspace_env": limux_env_value("LIMUX_WORKSPACE_ID").is_some(),
                "has_surface_env": limux_env_value("LIMUX_SURFACE_ID").is_some(),
                "payload_keys": payload_keys(payload),
            }),
        );
        return Ok(());
    };

    let existing = store.lookup(&session_id)?;
    let cwd = hook_str(payload, &["cwd", "working_directory", "directory"])
        .map(str::to_string)
        .or_else(|| existing.as_ref().and_then(|record| record.cwd.clone()));
    let pid = hook_str(payload, &["pid"])
        .and_then(|value| value.parse::<u32>().ok())
        .or_else(|| agent_ancestor_pid(agent))
        .or_else(|| existing.as_ref().and_then(|record| record.pid));
    let launch_command = agent_hooks::launch_record_from_env(agent, cwd.as_deref()).or_else(|| {
        existing
            .as_ref()
            .and_then(|record| record.launch_command.clone())
    });

    let record = agent_hooks::AgentHookSessionRecord {
        session_id,
        workspace_id,
        surface_id,
        cwd,
        pid,
        launch_command,
        updated_at: agent_hooks::now_seconds(),
    };
    let result = store.upsert(record);
    if result.is_ok() {
        write_agent_hook_debug(
            agent,
            event,
            "upserted",
            &json!({
                "payload_keys": payload_keys(payload),
            }),
        );
    }
    result
}

fn hook_session_id(payload: &Value) -> Option<String> {
    hook_str(payload, &["session_id", "sessionId", "sessionID"])
        .map(str::to_string)
        .or_else(|| limux_env_value("CLAUDE_CODE_SESSION_ID"))
        .or_else(|| limux_env_value("CLAUDE_SESSION_ID"))
        .or_else(|| hook_session_id_from_transcript(payload))
        .filter(|value| !value.trim().is_empty())
}

fn hook_session_id_from_transcript(payload: &Value) -> Option<String> {
    let transcript = hook_str(
        payload,
        &["transcript_path", "transcriptPath", "transcript"],
    )?;
    Path::new(transcript)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

fn payload_keys(payload: &Value) -> Vec<String> {
    payload
        .as_object()
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

fn write_agent_hook_debug(
    agent: agent_hooks::AgentKind,
    event: &str,
    outcome: &str,
    details: &Value,
) {
    let Some(dir) = agent_hook_debug_dir() else {
        return;
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("agent-hook-debug.jsonl");
    let line = json!({
        "time": agent_hooks::now_seconds(),
        "agent": agent.store_name(),
        "event": event,
        "outcome": outcome,
        "details": details,
    });
    if let Ok(mut encoded) = serde_json::to_vec(&line) {
        encoded.push(b'\n');
        let _ = append_debug_line(&path, &encoded);
    }
}

fn agent_hook_debug_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("LIMUX_AGENT_HOOK_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::state_dir()
        .map(|dir| dir.join("limux"))
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state/limux")))
}

fn append_debug_line(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to append {}", path.display()))
}

fn limux_env_value(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| ancestor_env_value(name))
}

#[cfg(target_os = "linux")]
fn agent_ancestor_pid(agent: agent_hooks::AgentKind) -> Option<u32> {
    let needle = agent.store_name();
    let mut pid = std::process::id();
    for _ in 0..8 {
        let parent = proc_parent_pid(pid)?;
        if parent <= 1 || parent == pid {
            return None;
        }
        if proc_identity_contains(parent, needle) {
            return Some(parent);
        }
        pid = parent;
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn agent_ancestor_pid(_agent: agent_hooks::AgentKind) -> Option<u32> {
    None
}

#[cfg(target_os = "linux")]
fn proc_identity_contains(pid: u32, needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    proc_cmdline(pid)
        .or_else(|| fs::read_to_string(format!("/proc/{pid}/comm")).ok())
        .map(|value| value.to_ascii_lowercase().contains(&needle))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn proc_cmdline(pid: u32) -> Option<String> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let parts = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .filter_map(|part| std::str::from_utf8(part).ok())
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" "))
}

#[cfg(target_os = "linux")]
fn ancestor_env_value(name: &str) -> Option<String> {
    let mut pid = std::process::id();
    for _ in 0..8 {
        let parent = proc_parent_pid(pid)?;
        if parent <= 1 || parent == pid {
            return None;
        }
        if let Some(value) = proc_env_value(parent, name) {
            return Some(value);
        }
        pid = parent;
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn ancestor_env_value(_name: &str) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn proc_parent_pid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_proc_stat_parent_pid(&stat)
}

#[cfg(target_os = "linux")]
fn parse_proc_stat_parent_pid(stat: &str) -> Option<u32> {
    let close = stat.rfind(')')?;
    let mut fields = stat.get(close + 2..)?.split_whitespace();
    fields.next()?;
    fields.next()?.parse().ok()
}

#[cfg(target_os = "linux")]
fn proc_env_value(pid: u32, name: &str) -> Option<String> {
    let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
    env_value_from_environ(&environ, name)
}

fn env_value_from_environ(environ: &[u8], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    environ
        .split(|byte| *byte == 0)
        .filter_map(|part| std::str::from_utf8(part).ok())
        .find_map(|entry| entry.strip_prefix(&prefix).map(str::to_string))
        .filter(|value| !value.trim().is_empty())
}

async fn run_hooks_command(
    client: &mut Client,
    args: &[String],
    json_output: bool,
) -> Result<CommandOutput> {
    let Some(first) = args.first().map(String::as_str) else {
        bail!(
            "Usage: limux hooks setup [agent]|uninstall [agent]|<agent> install|uninstall|<event>"
        );
    };

    match first {
        "setup" | "install" => {
            let target = parse_opt(args, "--agent").or_else(|| positional_arg(args, 1));
            let installed = install_hook_targets(target.as_deref())?;
            return hooks_summary_output("installed", installed, json_output);
        }
        "uninstall" => {
            let target = parse_opt(args, "--agent").or_else(|| positional_arg(args, 1));
            let changed = uninstall_hook_targets(target.as_deref())?;
            return hooks_summary_output("uninstalled", changed, json_output);
        }
        _ => {}
    }

    let agent = agent_hooks::AgentKind::from_hook_name(first)
        .ok_or_else(|| anyhow!("unknown hooks target: {first}"))?;
    let rest = &args[1..];
    match rest.first().map(String::as_str) {
        Some("install") => {
            install_hook_target(agent)?;
            hooks_summary_output(
                "installed",
                vec![agent.store_name().to_string()],
                json_output,
            )
        }
        Some("uninstall") => {
            uninstall_hook_target(agent)?;
            hooks_summary_output(
                "uninstalled",
                vec![agent.store_name().to_string()],
                json_output,
            )
        }
        _ => {
            let payload = run_agent_hook(client, agent, rest).await?;
            if json_output {
                Ok(CommandOutput::Json(payload))
            } else {
                Ok(CommandOutput::Text("OK".to_string()))
            }
        }
    }
}

fn hooks_summary_output(
    action: &str,
    agents: Vec<String>,
    json_output: bool,
) -> Result<CommandOutput> {
    if json_output {
        Ok(CommandOutput::Json(json!({
            "action": action,
            "agents": agents,
        })))
    } else {
        Ok(CommandOutput::Text(format!(
            "OK {action}: {}",
            if agents.is_empty() {
                "none".to_string()
            } else {
                agents.join(", ")
            }
        )))
    }
}

fn install_hook_targets(target: Option<&str>) -> Result<Vec<String>> {
    let agents = target
        .map(|name| {
            agent_hooks::AgentKind::from_hook_name(name)
                .ok_or_else(|| anyhow!("unknown hooks target: {name}"))
                .map(|agent| vec![agent])
        })
        .transpose()?
        .unwrap_or_else(default_hook_targets);

    let mut installed = Vec::new();
    for agent in agents {
        install_hook_target(agent)?;
        installed.push(agent.store_name().to_string());
    }
    Ok(installed)
}

fn uninstall_hook_targets(target: Option<&str>) -> Result<Vec<String>> {
    let agents = target
        .map(|name| {
            agent_hooks::AgentKind::from_hook_name(name)
                .ok_or_else(|| anyhow!("unknown hooks target: {name}"))
                .map(|agent| vec![agent])
        })
        .transpose()?
        .unwrap_or_else(default_hook_targets);

    let mut changed = Vec::new();
    for agent in agents {
        uninstall_hook_target(agent)?;
        changed.push(agent.store_name().to_string());
    }
    Ok(changed)
}

fn default_hook_targets() -> Vec<agent_hooks::AgentKind> {
    vec![
        agent_hooks::AgentKind::Codex,
        agent_hooks::AgentKind::Claude,
        agent_hooks::AgentKind::Gemini,
    ]
}

fn install_hook_target(agent: agent_hooks::AgentKind) -> Result<()> {
    match agent {
        agent_hooks::AgentKind::Codex => install_json_hooks(
            &codex_hooks_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("UserPromptSubmit", "prompt-submit"),
                ("Stop", "stop"),
            ],
        ),
        agent_hooks::AgentKind::Claude => install_json_hooks(
            &claude_settings_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("UserPromptSubmit", "prompt-submit"),
                ("Stop", "stop"),
                ("Notification", "stop"),
                ("SessionEnd", "session-end"),
            ],
        ),
        agent_hooks::AgentKind::OpenCode => install_opencode_plugin(),
        agent_hooks::AgentKind::Gemini => install_json_hooks(
            &gemini_settings_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("BeforeAgent", "prompt-submit"),
                ("AfterAgent", "stop"),
                ("SessionEnd", "session-end"),
            ],
        ),
    }
}

fn uninstall_hook_target(agent: agent_hooks::AgentKind) -> Result<()> {
    match agent {
        agent_hooks::AgentKind::Codex => uninstall_json_hooks(&codex_hooks_path(), agent),
        agent_hooks::AgentKind::Claude => uninstall_json_hooks(&claude_settings_path(), agent),
        agent_hooks::AgentKind::OpenCode => {
            let path = opencode_plugin_path();
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
            opencode_config_unregister_plugin()
        }
        agent_hooks::AgentKind::Gemini => uninstall_json_hooks(&gemini_settings_path(), agent),
    }
}

fn install_json_hooks(
    path: &Path,
    agent: agent_hooks::AgentKind,
    events: &[(&str, &str)],
) -> Result<()> {
    let mut root = read_json_object(path)?;
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} has non-object hooks field", path.display()))?;
    let marker = hook_marker(agent);
    for value in hooks.values_mut() {
        if let Some(entries) = value.as_array_mut() {
            entries.retain(|entry| !json_value_contains(entry, marker));
        }
    }
    hooks.retain(|_, value| {
        value
            .as_array()
            .map(|entries| !entries.is_empty())
            .unwrap_or(true)
    });

    for (agent_event, limux_event) in events {
        let entries = hooks
            .entry((*agent_event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let entries = entries
            .as_array_mut()
            .ok_or_else(|| anyhow!("{} hook {agent_event} is not an array", path.display()))?;
        entries.retain(|entry| !json_value_contains(entry, marker));
        let mut entry = json!({
            "hooks": [{
                "type": "command",
                "command": hook_command(agent, limux_event)?,
                "statusMessage": format!("Limux {} session restore", agent.label()),
                "timeout": hook_timeout(agent)
            }]
        });
        if matches!(agent, agent_hooks::AgentKind::Claude) {
            entry["matcher"] = Value::String("*".to_string());
        }
        entries.push(entry);
    }

    write_json_object(path, &root)
}

fn hook_timeout(agent: agent_hooks::AgentKind) -> u64 {
    match agent {
        agent_hooks::AgentKind::Claude => 5,
        agent_hooks::AgentKind::Codex | agent_hooks::AgentKind::Gemini => 5000,
        agent_hooks::AgentKind::OpenCode => 0,
    }
}

fn uninstall_json_hooks(path: &Path, agent: agent_hooks::AgentKind) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut root = read_json_object(path)?;
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        let marker = hook_marker(agent);
        for value in hooks.values_mut() {
            if let Some(entries) = value.as_array_mut() {
                entries.retain(|entry| !json_value_contains(entry, marker));
            }
        }
        hooks.retain(|_, value| {
            value
                .as_array()
                .map(|entries| !entries.is_empty())
                .unwrap_or(true)
        });
    }
    write_json_object(path, &root)
}

fn install_opencode_plugin() -> Result<()> {
    let path = opencode_plugin_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, opencode_plugin_source()?).context("failed to write OpenCode plugin")?;
    opencode_config_register_plugin(&path)
}

fn opencode_config_register_plugin(plugin_path: &Path) -> Result<()> {
    let config_path = opencode_config_path();
    let mut root = read_json_object(&config_path)?;
    let plugins = root
        .entry("plugin".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let plugins = plugins
        .as_array_mut()
        .ok_or_else(|| anyhow!("{} has non-array plugin field", config_path.display()))?;
    let plugin_str = plugin_path.to_string_lossy().into_owned();
    if !plugins.iter().any(|v| v.as_str() == Some(&plugin_str)) {
        plugins.push(Value::String(plugin_str));
    }
    write_json_object(&config_path, &root)
}

fn opencode_config_unregister_plugin() -> Result<()> {
    let config_path = opencode_config_path();
    if !config_path.exists() {
        return Ok(());
    }
    let plugin_path = opencode_plugin_path();
    let plugin_str = plugin_path.to_string_lossy().into_owned();
    let mut root = read_json_object(&config_path)?;
    if let Some(plugins) = root.get_mut("plugin").and_then(Value::as_array_mut) {
        plugins.retain(|v| v.as_str() != Some(&plugin_str));
    }
    write_json_object(&config_path, &root)
}

fn hook_command(agent: agent_hooks::AgentKind, event: &str) -> Result<String> {
    let disable_var = format!(
        "LIMUX_{}_HOOKS_DISABLED",
        agent.store_name().to_ascii_uppercase()
    );
    let limux_command = hook_cli_command()?;
    Ok(format!(
        "[ \"${{{disable_var}:-}}\" != \"1\" ] && {limux_command} --json hooks {} {} || echo '{{\"continue\":true,\"suppressOutput\":false}}'",
        agent.store_name(),
        event
    ))
}

fn hook_cli_command() -> Result<String> {
    let exe = env::current_exe().context("failed to resolve current executable")?;
    let file_name = exe
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if file_name == "limux-cli" {
        return Ok(shell_single_quote(&exe.to_string_lossy()));
    }
    Ok("limux".to_string())
}

fn opencode_plugin_cli_command() -> Result<String> {
    let exe = env::current_exe().context("failed to resolve current executable")?;
    let file_name = exe
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if file_name == "limux-cli" {
        return Ok(exe.to_string_lossy().to_string());
    }
    Ok("limux".to_string())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn hook_marker(agent: agent_hooks::AgentKind) -> &'static str {
    match agent {
        agent_hooks::AgentKind::Claude => "hooks claude",
        agent_hooks::AgentKind::Codex => "hooks codex",
        agent_hooks::AgentKind::OpenCode => "hooks opencode",
        agent_hooks::AgentKind::Gemini => "hooks gemini",
    }
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", path.display()))
}

fn write_json_object(path: &Path, object: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let encoded = serde_json::to_vec_pretty(object).context("failed to encode hook config")?;
    fs::write(&temp, encoded).with_context(|| format!("failed to write {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("failed to replace {}", path.display()))
}

fn json_value_contains(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(value) => value.contains(needle),
        Value::Array(values) => values
            .iter()
            .any(|value| json_value_contains(value, needle)),
        Value::Object(map) => map.values().any(|value| json_value_contains(value, needle)),
        _ => false,
    }
}

fn codex_hooks_path() -> PathBuf {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
        .join("hooks.json")
}

fn claude_settings_path() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .unwrap_or_else(|| PathBuf::from(".claude"))
        .join("settings.json")
}

fn gemini_settings_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gemini/settings.json")
}

fn opencode_config_dir() -> PathBuf {
    env::var_os("OPENCODE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config/opencode")))
        .unwrap_or_else(|| PathBuf::from(".config/opencode"))
}

fn opencode_plugin_path() -> PathBuf {
    opencode_config_dir().join("plugins/limux-session.js")
}

fn opencode_config_path() -> PathBuf {
    opencode_config_dir().join("config.json")
}

fn opencode_plugin_source() -> Result<String> {
    opencode_plugin_source_with_command(&opencode_plugin_cli_command()?)
}

fn opencode_plugin_source_with_command(limux_command: &str) -> Result<String> {
    let limux_command_json =
        serde_json::to_string(limux_command).context("failed to encode OpenCode hook command")?;
    Ok(
        r#"// limux-opencode-session-plugin v2
// Installed by `limux hooks opencode install`. Do not edit manually.

import { spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";

const LIMUX_COMMAND = __LIMUX_COMMAND__;

function debug(outcome, details = {}) {
  if (process.env.LIMUX_OPENCODE_HOOK_DEBUG !== "1" && outcome !== "spawn_failed") return;
  try {
    const dir = process.env.LIMUX_AGENT_HOOK_STATE_DIR || (process.env.XDG_STATE_HOME ? join(process.env.XDG_STATE_HOME, "limux") : join(process.env.HOME || ".", ".local/state/limux"));
    mkdirSync(dir, { recursive: true });
    appendFileSync(join(dir, "opencode-plugin-debug.jsonl"), JSON.stringify({
      time: Date.now() / 1000,
      outcome,
      details
    }) + "\n");
  } catch (_) {}
}

function firstString(...values) {
  for (const value of values) {
    if (typeof value === "string" && value.trim().length > 0) return value.trim();
  }
  return null;
}

function props(event) {
  return (event && typeof event === "object" && event.properties) || {};
}

function data(event) {
  return (event && typeof event === "object" && event.data) || {};
}

function info(event) {
  const p = props(event);
  const d = data(event);
  return (p.info && typeof p.info === "object" && p.info) || (d.info && typeof d.info === "object" && d.info) || {};
}

function eventType(event) {
  const raw = firstString(event && event.type, event && event.name);
  if (!raw) return null;
  if (raw === "sync") return firstString(event && event.name);
  return raw.endsWith(".1") ? raw.slice(0, -2) : raw;
}

function sessionId(event) {
  const p = props(event);
  const d = data(event);
  const i = info(event);
  return firstString(p.sessionID, p.sessionId, p.session_id, d.sessionID, d.sessionId, d.session_id, i.id, event && event.sessionID, event && event.sessionId);
}

function cwd(ctx, event) {
  const p = props(event);
  const d = data(event);
  const i = info(event);
  return firstString(p.cwd, p.directory, d.cwd, d.directory, i.directory, i.path, ctx && ctx.directory, process.cwd());
}

function launchExecutable() {
  return firstString(process.env.LIMUX_OPENCODE_EXECUTABLE, "opencode");
}

function send(kind, ctx, event) {
  if (process.env.LIMUX_OPENCODE_HOOKS_DISABLED === "1") {
    debug("skip_disabled", { kind });
    return;
  }
  if (!process.env.LIMUX_SURFACE_ID) {
    debug("skip_missing_surface", { kind, type: eventType(event), hasWorkspace: !!process.env.LIMUX_WORKSPACE_ID });
    return;
  }
  const sid = sessionId(event);
  if (!sid) {
    debug("skip_missing_session", { kind, type: eventType(event), keys: Object.keys(event || {}) });
    return;
  }
  const type = eventType(event);
  const payload = {
    session_id: sid,
    cwd: cwd(ctx, event),
    hook_event_name: type,
    event: type
  };
  try {
    const command = process.env.LIMUX_BIN || LIMUX_COMMAND;
    const result = spawnSync(command, ["hooks", "opencode", kind], {
      input: JSON.stringify(payload),
      encoding: "utf8",
      stdio: ["pipe", "ignore", "ignore"],
      timeout: 5000,
      env: {
        ...process.env,
        LIMUX_AGENT_LAUNCH_ARGV: launchExecutable(),
        LIMUX_AGENT_LAUNCH_EXECUTABLE: launchExecutable(),
        LIMUX_AGENT_LAUNCH_CWD: cwd(ctx, event)
      }
    });
    debug("spawned", { kind, type, status: result.status, error: result.error && String(result.error), command });
  } catch (error) {
    debug("spawn_failed", { kind, type, error: String(error) });
  }
}

const limuxSessionRestore = async (ctx) => {
  debug("plugin_started", { directory: ctx && ctx.directory, hasSurface: !!process.env.LIMUX_SURFACE_ID, hasWorkspace: !!process.env.LIMUX_WORKSPACE_ID });
  return {
    event: async ({ event }) => {
    const type = eventType(event);
    debug("event", { type, rawType: event && event.type, rawName: event && event.name });
    if (!type) return;
    if (type === "session.created") send("session-start", ctx, event);
    if (type === "session.idle" || type === "session.updated" || type === "session.status" || type === "session.compacted") send("prompt-submit", ctx, event);
    if (type === "session.error") send("session-end", ctx, event);
    if (type === "session.deleted") send("cleanup", ctx, event);
    }
  };
};

export const LimuxSessionRestore = limuxSessionRestore;
export default limuxSessionRestore;
"#
        .replace("__LIMUX_COMMAND__", &limux_command_json),
    )
}

fn list_project_commands(args: &[String]) -> Result<Value> {
    let (config, commands) = load_project_commands_for_args(args)?;
    Ok(project_commands_payload(&config, &commands))
}

async fn run_project_command(client: &mut Client, args: &[String]) -> Result<Value> {
    let request = build_project_command_workspace_request(args)?;
    let mut params = Map::new();
    params.insert("name".to_string(), Value::String(request.workspace_name));
    params.insert("command".to_string(), Value::String(request.command));
    params.insert("cwd".to_string(), Value::String(request.cwd));

    let mut created = client
        .call("workspace.create", Value::Object(params))
        .await
        .context("project command workspace.create failed")?;
    if let Some(map) = created.as_object_mut() {
        map.insert(
            "project_command".to_string(),
            Value::String(request.command_name),
        );
        map.insert(
            "project_command_config".to_string(),
            Value::String(request.config.to_string_lossy().to_string()),
        );
    }
    Ok(created)
}

async fn run_project_commands_command(
    client: &mut Client,
    args: &[String],
    json_output: bool,
) -> Result<CommandOutput> {
    let subcommand = args.first().map(String::as_str);
    if matches!(subcommand, Some("run")) {
        let payload = run_project_command(client, &args[1..]).await?;
        if json_output {
            Ok(CommandOutput::Json(payload))
        } else {
            let handle = handle_from_payload(&payload, "workspace_id", "workspace_ref");
            let command = get_string(&payload, &["project_command"]).unwrap_or_default();
            Ok(CommandOutput::Text(format!(
                "OK {handle} command={command}"
            )))
        }
    } else if subcommand.is_none()
        || matches!(subcommand, Some("list"))
        || parse_flag(args, "--list")
    {
        let payload = list_project_commands(args)?;
        if json_output {
            Ok(CommandOutput::Json(payload))
        } else {
            Ok(CommandOutput::Text(project_commands_text(&payload)))
        }
    } else {
        bail!("commands supports `list` or `run <name>`")
    }
}

async fn run_new_workspace(client: &mut Client, args: &[String]) -> Result<Value> {
    let cwd = parse_opt(args, "--cwd");
    let command = parse_opt(args, "--command");
    let original = resolve_current_workspace(client).await?;

    let mut params = Map::new();
    if let Some(cwd_value) = cwd.as_ref() {
        params.insert("cwd".to_string(), Value::String(cwd_value.clone()));
    }
    if let Some(command) = command.clone() {
        params.insert("command".to_string(), Value::String(command));
    }

    let created = client
        .call("workspace.create", Value::Object(params))
        .await
        .context("workspace.create failed")?;

    let _ = client
        .call("workspace.select", json!({ "workspace_id": original }))
        .await;

    Ok(created)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SshWorkspaceRequest {
    name: String,
    cwd: Option<String>,
    command: String,
    target: String,
}

fn ssh_option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-b" | "-c"
            | "-D"
            | "-E"
            | "-e"
            | "-F"
            | "-I"
            | "-i"
            | "-J"
            | "-L"
            | "-l"
            | "-m"
            | "-O"
            | "-o"
            | "-p"
            | "-Q"
            | "-R"
            | "-S"
            | "-W"
            | "-w"
    )
}

fn ssh_command_args(args: &[String]) -> Vec<String> {
    let mut ssh_args = Vec::new();
    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--cwd" || arg == "--name" {
            index += 2;
            continue;
        }
        if arg == "--" {
            ssh_args.extend(args[index + 1..].iter().cloned());
            break;
        }
        ssh_args.push(arg.clone());
        index += 1;
    }
    ssh_args
}

fn ssh_workspace_target(ssh_args: &[String]) -> Option<String> {
    let mut index = 0usize;
    while index < ssh_args.len() {
        let arg = &ssh_args[index];
        if arg == "--" {
            return ssh_args.get(index + 1).cloned();
        }
        if arg.starts_with('-') && arg != "-" {
            if ssh_option_takes_value(arg.as_str()) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        return Some(arg.clone());
    }
    None
}

fn ssh_workspace_command(ssh_args: &[String]) -> String {
    let args = ssh_args
        .iter()
        .map(|arg| shell_single_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!("ssh {args}")
}

fn build_ssh_workspace_request(args: &[String]) -> Result<SshWorkspaceRequest> {
    let cwd = parse_opt(args, "--cwd");
    let explicit_name = parse_opt(args, "--name").filter(|value| !value.trim().is_empty());
    let ssh_args = ssh_command_args(args);
    if ssh_args.is_empty() {
        bail!("ssh requires a remote target");
    }
    let target =
        ssh_workspace_target(&ssh_args).ok_or_else(|| anyhow!("ssh requires a remote target"))?;
    let name = explicit_name.unwrap_or_else(|| format!("ssh:{target}"));
    Ok(SshWorkspaceRequest {
        name,
        cwd,
        command: ssh_workspace_command(&ssh_args),
        target,
    })
}

async fn run_ssh_workspace(client: &mut Client, args: &[String]) -> Result<Value> {
    let request = build_ssh_workspace_request(args)?;
    let SshWorkspaceRequest {
        name,
        cwd,
        command,
        target,
    } = request;
    let mut params = Map::new();
    params.insert("name".to_string(), Value::String(name));
    params.insert("command".to_string(), Value::String(command));
    if let Some(cwd) = cwd {
        params.insert("cwd".to_string(), Value::String(cwd));
    }
    let mut created = client
        .call("workspace.create", Value::Object(params))
        .await
        .context("ssh workspace.create failed")?;
    if let Some(map) = created.as_object_mut() {
        map.insert("ssh_target".to_string(), Value::String(target));
    }
    Ok(created)
}

// ---------------------------------------------------------------------------
// `limux agent-team` — spin up a multi-agent collaboration workspace.
// ---------------------------------------------------------------------------
//
// Creates ONE workspace and one pane per requested agent (codex / claude /
// opencode / gemini), launches each agent's CLI in its pane, captures the
// pane/surface IDs, and seeds an AGENTS.md in the shared cwd describing the
// XML-tagged message protocol and the peer directory so agents can message
// each other.
//
// The protocol (codified in AGENTS.md):
//   To send a message to a peer, run from any terminal:
//     limux send --surface <peer-surface-id> \\
//       $'<agent-msg from="<me>" to="<peer>" ts="<iso-8601>">\\n...\\n</agent-msg>\\n'
//
// Peers read their own terminals normally — the text appears at the prompt.
// Each agent should watch for <agent-msg from="..."> blocks and reply with
// the same envelope targeted back.

/// Built-in agent launcher commands. Chosen to match the CLIs the user
/// actually has installed (see README); the launch command is what gets
/// typed into the new workspace's terminal, so it also works as a fallback
/// shell command if the CLI isn't in PATH yet.
fn agent_launch_command(agent: &str) -> Option<(&'static str, String)> {
    match agent.to_lowercase().as_str() {
        "codex" => Some(("codex", "codex".to_string())),
        "claude" | "claude-code" => Some(("claude", "claude".to_string())),
        "opencode" => Some(("opencode", "opencode".to_string())),
        "gemini" | "gemini-cli" => Some(("gemini", "gemini".to_string())),
        _ => None,
    }
}

async fn run_agent_team(client: &mut Client, args: &[String]) -> Result<Value> {
    // Parse --agents codex,claude (default: codex,claude).
    let agents_raw = parse_opt(args, "--agents").unwrap_or_else(|| "codex,claude".to_string());
    let agents: Vec<String> = agents_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if agents.is_empty() {
        bail!("agent-team: --agents is empty");
    }

    let cwd = parse_opt(args, "--cwd")
        .or_else(|| {
            env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
        .ok_or_else(|| anyhow!("agent-team: could not resolve --cwd"))?;

    // Optional: skip launching the CLIs (useful when the user wants to open
    // the agents manually) — still splits the panes + writes AGENTS.md.
    let no_launch = args.iter().any(|a| a == "--no-launch");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    // Resolve the agent list up front so --dry-run can build a deterministic
    // peer table without touching the host.
    let resolved: Vec<(String, &'static str, String)> = agents
        .iter()
        .filter_map(|agent| {
            agent_launch_command(agent).map(|(name, launch)| (agent.clone(), name, launch))
        })
        .collect();
    for agent in &agents {
        if agent_launch_command(agent).is_none() {
            eprintln!("agent-team: unknown agent '{agent}', skipping");
        }
    }
    if resolved.is_empty() {
        bail!("agent-team: no valid agents spawned");
    }

    let agents_md_path = std::path::Path::new(&cwd).join("AGENTS.md");

    if dry_run {
        let peers: Vec<(String, String, String, String)> = resolved
            .iter()
            .enumerate()
            .map(|(i, (_, name, launch))| {
                (
                    name.to_string(),
                    format!("<dry-run-pane-{i}>"),
                    format!("<dry-run-surface-{name}>"),
                    launch.clone(),
                )
            })
            .collect();
        let body = build_agents_md(
            &peers,
            &cwd,
            "<active-workspace>",
            "<dry-run-workspace>",
            "<dry-run-orchestrator>",
        );
        if let Err(err) = std::fs::write(&agents_md_path, body) {
            eprintln!(
                "agent-team: failed to write {}: {err}",
                agents_md_path.display()
            );
        }
        return Ok(json!({
            "ok": true,
            "cwd": cwd,
            "workspace_name": "<active-workspace>",
            "workspace_id": Value::Null,
            "orchestrator_surface_id": Value::Null,
            "agents_md": agents_md_path.to_string_lossy(),
            "dry_run": true,
            "no_launch": no_launch,
            "peers": peers
                .iter()
                .map(|(name, pane, surface, launch)| {
                    json!({
                        "agent": name,
                        "pane_id": pane,
                        "surface_id": surface,
                        "launch_command": launch,
                    })
                })
                .collect::<Vec<_>>(),
        }));
    }

    // 1. Resolve the orchestrator's workspace + pane. Prefer LIMUX_* env (set
    //    in every limux-spawned terminal) and fall back to the host's active
    //    focus so callers from a regular shell still work.
    let orchestrator_workspace = env::var("LIMUX_WORKSPACE_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let orchestrator_surface_env = env::var("LIMUX_SURFACE_ID").ok().filter(|s| !s.is_empty());
    let orchestrator_pane_env = env::var("LIMUX_PANE_ID").ok().filter(|s| !s.is_empty());

    let workspace_id = match orchestrator_workspace.clone() {
        Some(id) => id,
        None => resolve_current_workspace(client)
            .await
            .context("agent-team: could not resolve active workspace; run from inside a limux pane or pass --workspace")?,
    };

    // 2. Discover the orchestrator pane's surface_id. If env didn't tell us,
    //    use the focused/first surface in the workspace.
    let surfaces = client
        .call(
            "surface.list",
            json!({ "workspace_id": workspace_id.clone() }),
        )
        .await
        .context("surface.list failed for active workspace")?;
    let surface_rows = surfaces
        .get("surfaces")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if surface_rows.is_empty() {
        bail!("agent-team: active workspace has no surfaces");
    }
    let orchestrator_surface = orchestrator_surface_env.clone().unwrap_or_else(|| {
        surface_rows
            .iter()
            .find(|row| row.get("focused").and_then(Value::as_bool) == Some(true))
            .and_then(|row| get_string(row, &["surface_id"]))
            .or_else(|| get_string(&surface_rows[0], &["surface_id"]))
            .unwrap_or_default()
    });
    if orchestrator_surface.is_empty() {
        bail!("agent-team: could not determine orchestrator surface");
    }
    let orchestrator_pane = orchestrator_pane_env.unwrap_or_else(|| {
        surface_rows
            .iter()
            .find(|row| {
                get_string(row, &["surface_id"]).as_deref() == Some(orchestrator_surface.as_str())
            })
            .and_then(|row| get_string(row, &["pane_id"]))
            .unwrap_or_default()
    });

    // 3. Workspace name (for AGENTS.md header) — best-effort lookup.
    let workspace_name = client
        .call("workspace.list", json!({}))
        .await
        .ok()
        .and_then(|v| v.get("workspaces").and_then(Value::as_array).cloned())
        .and_then(|rows| {
            rows.into_iter().find(|row| {
                get_string(row, &["workspace_id", "id"]).as_deref() == Some(workspace_id.as_str())
            })
        })
        .and_then(|row| get_string(&row, &["name", "title"]))
        .unwrap_or_else(|| "active workspace".to_string());

    // 4. Split a pane per agent. Layout: agent[0] splits RIGHT of orchestrator,
    //    each subsequent agent splits DOWN of the previous agent — orchestrator
    //    keeps its full height on the left, peers stack top-to-bottom on the right.
    let mut peers: Vec<(String, String, String, String)> = Vec::new();
    let mut parent_surface = orchestrator_surface.clone();

    for (i, (_alias, name, launch)) in resolved.iter().enumerate() {
        let direction = if i == 0 { "right" } else { "down" };

        let mut params = Map::new();
        params.insert(
            "workspace_id".to_string(),
            Value::String(workspace_id.clone()),
        );
        params.insert(
            "surface_id".to_string(),
            Value::String(parent_surface.clone()),
        );
        params.insert(
            "direction".to_string(),
            Value::String(direction.to_string()),
        );
        params.insert("type".to_string(), Value::String("terminal".to_string()));
        if !no_launch {
            params.insert("command".to_string(), Value::String(launch.clone()));
        }

        let created = client
            .call("pane.create", Value::Object(params))
            .await
            .with_context(|| format!("pane.create failed for agent '{name}'"))?;
        let pane_id = get_string(&created, &["pane_id"])
            .ok_or_else(|| anyhow!("agent-team: pane.create for '{name}' returned no pane_id"))?;
        let surface_id = get_string(&created, &["surface_id"]).ok_or_else(|| {
            anyhow!("agent-team: pane.create for '{name}' returned no surface_id")
        })?;

        parent_surface = surface_id.clone();
        peers.push((name.to_string(), pane_id, surface_id, launch.clone()));
    }

    // 5. Write AGENTS.md into the shared cwd, clobbering any existing file.
    let body = build_agents_md(
        &peers,
        &cwd,
        &workspace_name,
        &workspace_id,
        &orchestrator_surface,
    );
    if let Err(err) = std::fs::write(&agents_md_path, body) {
        eprintln!(
            "agent-team: failed to write {}: {err}",
            agents_md_path.display()
        );
    }

    Ok(json!({
        "ok": true,
        "cwd": cwd,
        "workspace_name": workspace_name,
        "workspace_id": workspace_id,
        "orchestrator_pane_id": orchestrator_pane,
        "orchestrator_surface_id": orchestrator_surface,
        "agents_md": agents_md_path.to_string_lossy(),
        "dry_run": false,
        "no_launch": no_launch,
        "peers": peers
            .iter()
            .map(|(name, pane, surface, launch)| {
                json!({
                    "agent": name,
                    "pane_id": pane,
                    "surface_id": surface,
                    "launch_command": launch,
                })
            })
            .collect::<Vec<_>>(),
    }))
}

fn build_agents_md(
    peers: &[(String, String, String, String)],
    cwd: &str,
    workspace_name: &str,
    workspace_id: &str,
    orchestrator_surface: &str,
) -> String {
    let mut out = String::new();
    out.push_str("# AGENTS.md — agent-to-agent message protocol\n\n");
    out.push_str(
        "This file is auto-generated by `limux agent-team`. It defines how the\n\
         agents running in this workspace team communicate with each other via\n\
         the limux control socket. Humans should feel free to edit the\n\
         'Policies' section below; everything else is mechanical.\n\n",
    );

    out.push_str(&format!(
        "## Team workspace\n\n\
         The orchestrator (the pane that ran `limux agent-team`) and all\n\
         spawned peers share one workspace:\n\n\
         - Workspace name: `{workspace_name}`\n\
         - Workspace ID: `{workspace_id}`\n\
         - Orchestrator surface: `{orchestrator_surface}`\n\
         - Shared cwd: `{cwd}`\n\n",
    ));

    out.push_str("## Peers in this team\n\n");
    out.push_str("| Agent | Pane | Surface | Launch command |\n");
    out.push_str("|-------|------|---------|----------------|\n");
    for (name, pane_id, surface_id, launch) in peers {
        out.push_str(&format!(
            "| `{name}` | `{pane_id}` | `{surface_id}` | `{launch}` |\n"
        ));
    }
    out.push('\n');
    out.push_str(
        "The orchestrator is not in the table — message it back using its\n\
         `Orchestrator surface` from the block above.\n\n",
    );

    out.push_str("## How to send a message\n\n");
    out.push_str(
        "Messages use the `<agent-msg>` XML envelope so they're easy to\n\
         extract from the terminal scrollback. To send a message to a peer,\n\
         look up their `Surface` in the peers table above and run (from any\n\
         shell, including the agent's own terminal — `limux` is on PATH):\n\n",
    );
    out.push_str("```bash\n");
    out.push_str("limux send --surface <peer-surface-id> $'<agent-msg from=\"<me>\" to=\"<peer>\" id=\"<uuid>\" ts=\"<iso8601>\">\\n<body/>\\n</agent-msg>\\n'\n");
    out.push_str("```\n\n");
    out.push_str(
        "The message appears at the peer's prompt as plain stdin, so the\n\
         peer's agent CLI picks it up like a normal user message. Trailing\n\
         newline is required so the agent's read-line actually fires.\n\n",
    );

    out.push_str("### Envelope format\n\n");
    out.push_str("```xml\n");
    out.push_str("<agent-msg from=\"codex\" to=\"claude\" id=\"<uuid>\" ts=\"2026-04-19T16:48:00Z\" reply-to=\"<parent-uuid>\">\n");
    out.push_str(
        "  <context>optional: one or two sentences about what the request is for</context>\n",
    );
    out.push_str("  <request>the actual ask, in prose or code</request>\n");
    out.push_str("  <expect>how you want the peer to reply (\"inline code diff\" / \"short summary\" / etc.)</expect>\n");
    out.push_str("</agent-msg>\n");
    out.push_str("```\n\n");

    out.push_str("Rules:\n");
    out.push_str("- `from` / `to` MUST be one of the agent names in the peers table.\n");
    out.push_str("- `id` is a fresh UUID (e.g. `uuidgen`); peers echo it in `reply-to`.\n");
    out.push_str("- `ts` is ISO-8601 UTC (`date -u +%Y-%m-%dT%H:%M:%SZ`).\n");
    out.push_str("- Inner tags are guidance, not required — `<request>` alone is fine.\n");
    out.push_str("- Keep bodies short; link to files in the shared cwd for anything long.\n\n");

    out.push_str("### Replying\n\n");
    out.push_str("Reply with the envelope reversed and `reply-to` set to the original `id`:\n\n");
    out.push_str("```bash\n");
    out.push_str("limux send --surface <orig-sender-surface-id> $'<agent-msg from=\"claude\" to=\"codex\" id=\"<new-uuid>\" reply-to=\"<orig-uuid>\" ts=\"<iso8601>\">\\n<response>...</response>\\n</agent-msg>\\n'\n");
    out.push_str("```\n\n");

    out.push_str("## Pinging the human\n\n");
    out.push_str(
        "When you need human input, use `limux notify` — it pops a toast\n\
         and lights up the workspace in the sidebar. Example:\n\n",
    );
    out.push_str("```bash\n");
    out.push_str("limux notify --subtitle 'needs review' --body 'Claude blocked on auth choice' 'Input needed'\n");
    out.push_str("```\n\n");

    out.push_str("## Environment contract\n\n");
    out.push_str(
        "Every pane spawned by limux inherits:\n\
         - `LIMUX_WORKSPACE_ID` — the team workspace's UUID\n\
         - `LIMUX_SURFACE_ID` — this pane's surface id (this is your `from`)\n\
         - `LIMUX_PANE_ID`, `LIMUX_TAB_ID`\n\
         - `LIMUX_SOCKET` — the control socket path\n\n\
         This means `limux identify`, `limux send` (with `--surface`), and\n\
         `limux notify` all auto-target the right thing with no flags needed\n\
         from inside the agent's own terminal.\n\n",
    );

    out.push_str("## Splitting your own pane\n\n");
    out.push_str("If you need a scratch terminal next to you, split your own pane:\n\n");
    out.push_str("```bash\n");
    out.push_str("limux new-pane --direction right --command bash\n");
    out.push_str("```\n\n");
    out.push_str(
        "`new-pane` reads `LIMUX_WORKSPACE_ID`, `LIMUX_SURFACE_ID`, and\n\
         `LIMUX_PANE_ID`, so it splits your current pane even if GTK focus has\n\
         moved elsewhere. Live GTK self-spawn currently supports terminal\n\
         panes only; browser pane creation is deferred.\n\n",
    );

    out.push_str("## Policies (edit these freely)\n\n");
    out.push_str(
        "- If a peer is silent for more than 60 seconds, re-send with `reply-to` = your last id.\n",
    );
    out.push_str(
        "- Never send more than 200 lines at once; write to a file and send the path instead.\n",
    );
    out.push_str("- If two agents disagree on an approach, both message the human via `limux notify` and stop.\n");
    out.push_str("- Before taking destructive actions (rm, git push, kubectl apply), ask the human via `limux notify`.\n\n");

    out.push_str("---\n");
    out.push_str(
        "_Generated by `limux agent-team`. Safe to edit the Policies\n\
         section; regenerating will overwrite everything above it._\n",
    );

    out
}

async fn run_close_workspace(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| env::var("LIMUX_WORKSPACE_ID").ok())
        .ok_or_else(|| anyhow!("close-workspace requires --workspace <id|ref>"))?;
    client
        .call("workspace.close", json!({ "workspace_id": workspace }))
        .await
}

async fn run_sidebar_state(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| env::var("LIMUX_WORKSPACE_ID").ok())
        .ok_or_else(|| anyhow!("sidebar-state requires --workspace <id|ref>"))?;

    match client
        .call(
            "sidebar.state",
            json!({ "workspace_id": workspace.clone() }),
        )
        .await
    {
        Ok(payload) => return Ok(payload),
        Err(error) if error.to_string().starts_with("-32601:") => {}
        Err(error) => return Err(error),
    }

    let listed = client.call("workspace.list", json!({})).await?;
    let rows = listed
        .get("workspaces")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let matched = rows.into_iter().find(|row| {
        let id = get_string(row, &["workspace_id", "id"]).unwrap_or_default();
        let rf = get_string(row, &["workspace_ref", "ref"]).unwrap_or_default();
        workspace == id || workspace == rf
    });

    let cwd = matched
        .as_ref()
        .and_then(|row| get_string(row, &["cwd"]))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".to_string());

    let git_branch = if cwd != "none" {
        let output = Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .arg("rev-parse")
            .arg("--abbrev-ref")
            .arg("HEAD")
            .output();
        match output {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            _ => "none".to_string(),
        }
    } else {
        "none".to_string()
    };

    Ok(json!({
        "workspace": workspace,
        "cwd": cwd,
        "git_branch": git_branch,
    }))
}

async fn run_new_surface(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace");
    call_in_workspace_scope(client, workspace, "surface.create", json!({})).await
}

fn env_opt(name: &str) -> Option<String> {
    env::var(name).ok()
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

fn build_new_pane_request(
    args: &[String],
    env_lookup: impl Fn(&str) -> Option<String>,
) -> (Option<String>, Value) {
    let workspace =
        nonempty(parse_opt(args, "--workspace").or_else(|| env_lookup("LIMUX_WORKSPACE_ID")));
    let surface = nonempty(parse_opt(args, "--surface").or_else(|| env_lookup("LIMUX_SURFACE_ID")));
    let pane = nonempty(parse_opt(args, "--pane").or_else(|| env_lookup("LIMUX_PANE_ID")));
    let direction = parse_opt(args, "--direction").unwrap_or_else(|| "right".to_string());
    let pane_type = parse_opt(args, "--type").unwrap_or_else(|| "terminal".to_string());
    let command = nonempty(parse_opt(args, "--command"));
    let url = nonempty(parse_opt(args, "--url"));

    let mut params = Map::new();
    params.insert("direction".to_string(), Value::String(direction));
    params.insert("type".to_string(), Value::String(pane_type));
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }
    if let Some(pane) = pane {
        params.insert("pane_id".to_string(), Value::String(pane));
    }
    if let Some(command) = command {
        params.insert("command".to_string(), Value::String(command));
    }
    if let Some(url) = url {
        params.insert("url".to_string(), Value::String(url));
    }

    (workspace, Value::Object(params))
}

async fn run_new_pane(client: &mut Client, args: &[String]) -> Result<Value> {
    // `pane.create` contract shared with the core dispatcher and live GTK host:
    // direction/type are validated by the server, and responses keep
    // pane_id/pane_ref/surface_id/surface_ref. Inside a Limux terminal,
    // LIMUX_* defaults make `limux new-pane --command claude` split the
    // caller's pane; outside Limux, omitting workspace preserves active-focus
    // server behavior.
    let (workspace, params) = build_new_pane_request(args, env_opt);
    call_in_workspace_scope(client, workspace, "pane.create", params).await
}

async fn run_read_screen(client: &mut Client, args: &[String]) -> Result<Value> {
    if let Some(lines) = parse_opt(args, "--lines") {
        if lines.parse::<u64>().unwrap_or(0) == 0 {
            bail!("--lines must be greater than 0");
        }
    }

    let workspace = parse_opt(args, "--workspace");
    let surface = parse_opt(args, "--surface");
    let mut params = Map::new();
    if let Some(workspace) = workspace {
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }

    client
        .call("surface.read_text", Value::Object(params))
        .await
}

async fn run_rename_workspace_like(
    client: &mut Client,
    command: &str,
    args: &[String],
) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace").or_else(|| env::var("LIMUX_WORKSPACE_ID").ok());
    let title = trailing_title(args).ok_or_else(|| {
        if command == "rename-window" {
            anyhow!("rename-window requires a title")
        } else {
            anyhow!("rename-workspace requires a title")
        }
    })?;

    let mut params = Map::new();
    params.insert("title".to_string(), Value::String(title));
    if let Some(workspace) = workspace {
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }

    client.call("workspace.rename", Value::Object(params)).await
}

async fn run_rename_tab(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| env::var("LIMUX_WORKSPACE_ID").ok())
        .unwrap_or_default();
    let tab = parse_opt(args, "--tab")
        .or_else(|| env::var("LIMUX_TAB_ID").ok())
        .unwrap_or_default();
    let title = trailing_title(args).ok_or_else(|| anyhow!("rename-tab requires a title"))?;

    let mut params = Map::new();
    params.insert("action".to_string(), Value::String("rename".to_string()));
    params.insert("title".to_string(), Value::String(title));
    if !workspace.is_empty() {
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }
    if !tab.is_empty() {
        params.insert("surface_id".to_string(), Value::String(tab));
    }

    client.call("tab.action", Value::Object(params)).await
}

async fn run_tab_action(client: &mut Client, args: &[String]) -> Result<Value> {
    if parse_flag(args, "--help") {
        return Ok(json!({
            "help": "Usage: limux tab-action --action <name> [--workspace <id|ref>] [--tab <id|ref>] [--title <text>] [--url <url>]\nTarget tab:\n  --tab tab:<n>       Stable tab reference alias\n  --tab surface:<n>   Surface alias (legacy-compatible)\nExamples:\n  limux tab-action --workspace workspace:2 --tab tab:1 --action pin\n  limux tab-action --tab tab:3 --action mark-unread"
        }));
    }

    let action = parse_opt(args, "--action")
        .ok_or_else(|| anyhow!("tab-action requires --action <name>"))?;
    let workspace = parse_opt(args, "--workspace").or_else(|| env::var("LIMUX_WORKSPACE_ID").ok());
    let tab = parse_opt(args, "--tab").or_else(|| env::var("LIMUX_TAB_ID").ok());
    let title = parse_opt(args, "--title").or_else(|| trailing_title(args));
    let url = parse_opt(args, "--url");

    if action == "new-terminal-right" || action == "new-browser-right" {
        let pane_type = if action == "new-browser-right" {
            "browser"
        } else {
            "terminal"
        };
        let mut params = vec![
            "--direction".to_string(),
            "right".to_string(),
            "--type".to_string(),
            pane_type.to_string(),
        ];
        if let Some(workspace) = workspace.clone() {
            params.push("--workspace".to_string());
            params.push(workspace);
        }
        if let Some(url) = url {
            params.push("--url".to_string());
            params.push(url);
        }
        let created = run_new_pane(client, &params).await?;
        let tab_ref = tab.unwrap_or_else(|| "tab:1".to_string());
        return Ok(json!({
            "tab_ref": tab_ref,
            "surface_id": created.get("surface_id").cloned().unwrap_or(Value::Null),
            "surface_ref": created.get("surface_ref").cloned().unwrap_or(Value::Null),
        }));
    }

    let mut params = Map::new();
    params.insert("action".to_string(), Value::String(action.clone()));
    if let Some(workspace) = workspace {
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }
    if let Some(tab) = tab.clone() {
        params.insert("surface_id".to_string(), Value::String(tab));
    }
    if let Some(title) = title {
        params.insert("title".to_string(), Value::String(title));
    }

    let mut payload = client.call("tab.action", Value::Object(params)).await?;
    if let Some(obj) = payload.as_object_mut() {
        if !obj.contains_key("tab_ref") {
            obj.insert(
                "tab_ref".to_string(),
                Value::String(tab.unwrap_or_else(|| "tab:1".to_string())),
            );
        }
        if action == "pin" {
            obj.insert("pinned".to_string(), Value::Bool(true));
        }
        if action == "unpin" {
            obj.insert("pinned".to_string(), Value::Bool(false));
        }
    }
    Ok(payload)
}

async fn run_browser(
    client: &mut Client,
    args: &[String],
    json_output: bool,
) -> Result<CommandOutput> {
    let mut browser_args = args.to_vec();
    let mut local_json = json_output;

    loop {
        if browser_args.last().map(|s| s.as_str()) == Some("--json") {
            local_json = true;
            browser_args.pop();
            continue;
        }
        break;
    }

    let workspace = parse_opt(&browser_args, "--workspace");
    let mut surface = parse_opt(&browser_args, "--surface");

    let mut positional: Vec<String> = Vec::new();
    let mut skip = false;
    for (idx, arg) in browser_args.iter().enumerate() {
        if skip {
            skip = false;
            continue;
        }
        match arg.as_str() {
            "--workspace" | "--surface" | "--id-format" | "--timeout-ms" | "--load-state"
            | "--out" | "--file" | "--format" | "--browser" => {
                if idx + 1 < browser_args.len() {
                    skip = true;
                }
            }
            value if value.starts_with('-') => {}
            _ => positional.push(arg.clone()),
        }
    }

    if positional.is_empty() {
        bail!("browser requires a subcommand");
    }

    let mut pos_idx = 0usize;
    let first = positional[0].clone();
    let verbs_without_surface = [
        "open",
        "open-split",
        "new",
        "identify",
        "import-cookies",
        "profile",
        "profiles",
        "list-profiles",
    ];

    if !verbs_without_surface.contains(&first.as_str()) {
        if !first.contains(':') && !first.contains('-') {
            // probably still subcommand
        } else {
            surface = Some(first);
            pos_idx = 1;
        }
    }

    if pos_idx >= positional.len() {
        bail!("browser requires a subcommand");
    }
    let sub = positional[pos_idx].clone();
    let rest = positional[(pos_idx + 1)..].to_vec();

    let output = match sub.as_str() {
        "profile" | "profiles" | "list-profiles" => {
            let filter = parse_opt(&browser_args, "--browser").or_else(|| rest.first().cloned());
            CommandOutput::Json(browser_profiles_payload(
                filter.as_deref(),
                parse_flag(&browser_args, "--include-missing"),
            ))
        }
        "open" | "open-split" | "new" => {
            let url = rest
                .first()
                .cloned()
                .unwrap_or_else(|| "about:blank".to_string());
            if let Some(surface) = surface.clone() {
                let payload = browser_call(client, Some(surface), "browser.navigate", {
                    let mut p = Map::new();
                    p.insert("url".to_string(), Value::String(url));
                    p
                })
                .await?;
                CommandOutput::Json(payload)
            } else {
                let payload = call_in_workspace_scope(
                    client,
                    workspace.clone(),
                    "browser.open_split",
                    json!({ "url": url }),
                )
                .await?;
                CommandOutput::Json(payload)
            }
        }
        "url" | "get-url" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser url requires a surface"))?;
            let payload = browser_call(client, Some(sid), "browser.url.get", Map::new()).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(get_string(&payload, &["url"]).unwrap_or_default())
            }
        }
        "goto" | "navigate" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser navigate requires a surface"))?;
            let url = rest
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("browser navigate requires a URL"))?;
            let payload = browser_call(client, Some(sid.clone()), "browser.navigate", {
                let mut p = Map::new();
                p.insert("url".to_string(), Value::String(url));
                p
            })
            .await?;
            if parse_flag(&browser_args, "--snapshot-after") {
                let snap = browser_call(client, Some(sid), "browser.snapshot", Map::new()).await?;
                if local_json {
                    let mut merged = payload;
                    if let Some(obj) = merged.as_object_mut() {
                        obj.insert("post_action_snapshot".to_string(), snap);
                    }
                    CommandOutput::Json(merged)
                } else {
                    CommandOutput::Text(
                        get_string(&snap, &["snapshot", "text"])
                            .unwrap_or_else(|| "OK".to_string()),
                    )
                }
            } else {
                CommandOutput::Json(payload)
            }
        }
        "wait" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser wait requires a surface"))?;
            let mut p = Map::new();
            if let Some(selector) = parse_opt(&browser_args, "--selector") {
                p.insert("selector".to_string(), Value::String(selector));
            }
            if let Some(timeout_ms) = parse_opt(&browser_args, "--timeout-ms") {
                if let Ok(ms) = timeout_ms.parse::<u64>() {
                    p.insert("timeout_ms".to_string(), Value::Number(ms.into()));
                }
            }
            let payload = browser_call(client, Some(sid), "browser.wait", p).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "snapshot" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser snapshot requires a surface"))?;
            let payload = browser_call(client, Some(sid), "browser.snapshot", Map::new()).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                let url = get_string(&payload, &["url"]).unwrap_or_default();
                if parse_flag(&browser_args, "--interactive") && url == "about:blank" {
                    CommandOutput::Text("about:blank\nNo interactive elements found; try `browser <surface> get url`.".to_string())
                } else if parse_flag(&browser_args, "--interactive") {
                    let mut text = get_string(&payload, &["snapshot", "text"])
                        .unwrap_or_else(|| "OK".to_string());
                    if let Some(refs) = payload.get("refs").and_then(Value::as_object) {
                        for key in refs.keys() {
                            text.push_str(&format!("\nref={}", key));
                        }
                    }
                    CommandOutput::Text(text)
                } else {
                    CommandOutput::Text(
                        get_string(&payload, &["snapshot", "text"])
                            .unwrap_or_else(|| "OK".to_string()),
                    )
                }
            }
        }
        "screenshot" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser screenshot requires a surface"))?;
            let out = parse_opt(&browser_args, "--out");
            let mut params = Map::new();
            if let Some(path) = out.clone() {
                params.insert("path".to_string(), Value::String(path));
            }
            let mut payload = browser_call(client, Some(sid), "browser.screenshot", params).await?;
            let mut path = get_string(&payload, &["path"])
                .unwrap_or_else(|| "/tmp/limux-browser-shot.png".to_string());
            if let Some(out_path) = &out {
                path = out_path.clone();
            }
            if !Path::new(&path).exists() {
                if let Some(parent) = Path::new(&path).parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create screenshot directory {}", parent.display())
                    })?;
                }
                fs::write(&path, [])
                    .with_context(|| format!("failed to create screenshot {}", path))?;
            }
            let url = format!("file://{}", path);
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("path".to_string(), Value::String(path.clone()));
                obj.insert("url".to_string(), Value::String(url.clone()));
                obj.remove("png_base64");
            }
            if out.is_some() {
                CommandOutput::Text(format!("OK {}", path))
            } else if local_json {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(path)
            }
        }
        "find" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser find requires a surface"))?;
            let locator = rest.first().cloned().unwrap_or_else(|| "text".to_string());
            let value = rest.get(1).cloned().unwrap_or_default();
            let method = format!("browser.find.{}", locator);
            let mut params = Map::new();
            match locator.as_str() {
                "role" => {
                    params.insert("role".to_string(), Value::String(value));
                }
                "nth" => {
                    params.insert(
                        "selector".to_string(),
                        Value::String(rest.get(1).cloned().unwrap_or_default()),
                    );
                    let index = rest.get(2).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                    params.insert("index".to_string(), Value::Number(index.into()));
                }
                "first" | "last" => {
                    params.insert("selector".to_string(), Value::String(value));
                }
                _ => {
                    params.insert(locator.clone(), Value::String(value));
                }
            }
            let payload = browser_call(client, Some(sid), &method, params).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(
                    get_string(&payload, &["element_ref"]).unwrap_or_else(|| "@e1".to_string()),
                )
            }
        }
        "frame" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser frame requires a surface"))?;
            let target = rest.first().cloned().unwrap_or_else(|| "main".to_string());
            let payload = if target == "main" {
                browser_call(client, Some(sid), "browser.frame.main", Map::new()).await?
            } else {
                browser_call(client, Some(sid), "browser.frame.select", {
                    let mut p = Map::new();
                    p.insert("selector".to_string(), Value::String(target));
                    p
                })
                .await?
            };
            CommandOutput::Json(payload)
        }
        "click" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser click requires a surface"))?;
            let selector = parse_opt(&browser_args, "--selector")
                .or_else(|| rest.first().cloned())
                .ok_or_else(|| anyhow!("browser click requires a selector"))?;
            let payload = browser_call(client, Some(sid), "browser.click", {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "fill" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser fill requires a surface"))?;
            let selector = parse_opt(&browser_args, "--selector")
                .or_else(|| rest.first().cloned())
                .unwrap_or_default();
            let text = parse_opt(&browser_args, "--text")
                .or_else(|| rest.get(1).cloned())
                .unwrap_or_default();
            let payload = browser_call(client, Some(sid), "browser.fill", {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p.insert("text".to_string(), Value::String(text));
                p
            })
            .await?;
            if parse_flag(&browser_args, "--snapshot-after") {
                let snap =
                    browser_call(client, surface.clone(), "browser.snapshot", Map::new()).await?;
                if local_json {
                    let mut merged = payload;
                    if let Some(obj) = merged.as_object_mut() {
                        obj.insert("post_action_snapshot".to_string(), snap);
                    }
                    CommandOutput::Json(merged)
                } else {
                    CommandOutput::Text(
                        get_string(&snap, &["snapshot", "text"])
                            .unwrap_or_else(|| "OK".to_string()),
                    )
                }
            } else {
                CommandOutput::Json(payload)
            }
        }
        "get" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser get requires a surface"))?;
            let get_verb = rest.first().cloned().unwrap_or_else(|| "url".to_string());
            let method = match get_verb.as_str() {
                "url" => "browser.url.get".to_string(),
                "title" => "browser.get.title".to_string(),
                "text" => "browser.get.text".to_string(),
                "html" => "browser.get.html".to_string(),
                "value" => "browser.get.value".to_string(),
                "attr" => "browser.get.attr".to_string(),
                "count" => "browser.get.count".to_string(),
                "box" => "browser.get.box".to_string(),
                "styles" => "browser.get.styles".to_string(),
                other => bail!("Unsupported browser get subcommand: {}", other),
            };
            let selector = rest
                .get(1)
                .cloned()
                .or_else(|| parse_opt(&browser_args, "--selector"));
            let mut p = Map::new();
            if let Some(selector) = selector {
                p.insert("selector".to_string(), Value::String(selector));
            }
            if let Some(attr) = parse_opt(&browser_args, "--attr") {
                p.insert("name".to_string(), Value::String(attr));
            }
            if let Some(property) = parse_opt(&browser_args, "--property") {
                p.insert("property".to_string(), Value::String(property));
            }
            let payload = browser_call(client, Some(sid), &method, p).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                let text = get_string(&payload, &["url", "title", "text", "value", "html"])
                    .unwrap_or_else(|| "OK".to_string());
                CommandOutput::Text(text)
            }
        }
        "cookies" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser cookies requires a surface"))?;
            let op = rest.first().cloned().unwrap_or_else(|| "get".to_string());
            let method = match op.as_str() {
                "get" => "browser.cookies.get",
                "set" => "browser.cookies.set",
                "clear" => "browser.cookies.clear",
                _ => bail!("Unsupported browser cookies subcommand: {}", op),
            };
            let mut p = Map::new();
            if let Some(name) = rest
                .get(1)
                .cloned()
                .or_else(|| parse_opt(&browser_args, "--name"))
            {
                p.insert("name".to_string(), Value::String(name));
            }
            if let Some(value) = rest
                .get(2)
                .cloned()
                .or_else(|| parse_opt(&browser_args, "--value"))
            {
                p.insert("value".to_string(), Value::String(value));
            }
            let payload = browser_call(client, Some(sid), method, p).await?;
            CommandOutput::Json(payload)
        }
        "storage" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser storage requires a surface"))?;
            if rest.len() < 2 {
                bail!("browser storage requires <local|session> <get|set|clear>");
            }
            let storage_type = rest[0].clone();
            let op = rest[1].clone();
            let method = match op.as_str() {
                "get" => "browser.storage.get",
                "set" => "browser.storage.set",
                "clear" => "browser.storage.clear",
                _ => bail!("Unsupported browser storage subcommand: {}", op),
            };
            let mut p = Map::new();
            p.insert("type".to_string(), Value::String(storage_type));
            if let Some(key) = rest.get(2) {
                p.insert("key".to_string(), Value::String(key.clone()));
            }
            if let Some(value) = rest.get(3) {
                p.insert("value".to_string(), Value::String(value.clone()));
            }
            let payload = browser_call(client, Some(sid), method, p).await?;
            CommandOutput::Json(payload)
        }
        "import-cookies" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser import-cookies requires a surface"))?;
            let file = parse_opt(&browser_args, "--file")
                .or_else(|| rest.first().cloned())
                .ok_or_else(|| anyhow!("browser import-cookies requires --file <path>"))?;
            let format =
                BrowserCookieImportFormat::parse(parse_opt(&browser_args, "--format").as_deref())?;
            let cookies = load_browser_cookie_import_file(Path::new(&file), format)?;
            let host_payload = browser_call(client, Some(sid.clone()), "browser.cookies.import", {
                let mut p = Map::new();
                p.insert(
                    "cookies".to_string(),
                    Value::Array(
                        cookies
                            .iter()
                            .map(BrowserCookieImportRow::to_import_json)
                            .collect(),
                    ),
                );
                p
            })
            .await;

            match host_payload {
                Ok(mut payload) => {
                    if let Some(map) = payload.as_object_mut() {
                        map.insert("file".to_string(), Value::String(file));
                    }
                    CommandOutput::Json(payload)
                }
                Err(error) if is_unknown_method_error(&error) => {
                    let mut imported = Vec::new();
                    let mut skipped = Vec::new();

                    for cookie in &cookies {
                        if cookie.http_only {
                            skipped.push(json!({
                                "name": cookie.name.clone(),
                                "domain": cookie.domain.clone(),
                                "path": cookie.path.clone(),
                                "reason": "http_only cookies cannot be set through the legacy page bridge",
                            }));
                            continue;
                        }

                        let mut p = Map::new();
                        p.insert("name".to_string(), Value::String(cookie.name.clone()));
                        p.insert("value".to_string(), Value::String(cookie.value.clone()));
                        browser_call(client, Some(sid.clone()), "browser.cookies.set", p).await?;
                        imported.push(json!({
                            "name": cookie.name.clone(),
                            "domain": cookie.domain.clone(),
                            "path": cookie.path.clone(),
                            "secure": cookie.secure,
                        }));
                    }

                    CommandOutput::Json(json!({
                        "ok": true,
                        "file": file,
                        "imported_count": imported.len(),
                        "skipped_count": skipped.len(),
                        "imported": imported,
                        "skipped": skipped,
                        "note": "Running host does not expose browser.cookies.import; imported page-visible cookies through document.cookie and skipped httpOnly rows.",
                    }))
                }
                Err(error) => return Err(error),
            }
        }
        "tab" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser tab requires a surface"))?;
            let tab_verb = rest.first().cloned().unwrap_or_else(|| "list".to_string());
            let (method, p) = match tab_verb.as_str() {
                "list" => ("browser.tab.list", Map::new()),
                "new" => {
                    let mut p = Map::new();
                    if let Some(url) = rest.get(1) {
                        p.insert("url".to_string(), Value::String(url.clone()));
                    }
                    ("browser.tab.new", p)
                }
                "switch" => {
                    let mut p = Map::new();
                    if let Some(target) = rest.get(1) {
                        p.insert(
                            "target_surface_id".to_string(),
                            Value::String(target.clone()),
                        );
                    }
                    ("browser.tab.switch", p)
                }
                "close" => {
                    let mut p = Map::new();
                    if let Some(target) = rest.get(1) {
                        p.insert(
                            "target_surface_id".to_string(),
                            Value::String(target.clone()),
                        );
                    }
                    ("browser.tab.close", p)
                }
                _ => bail!("Unsupported browser tab subcommand: {}", tab_verb),
            };
            let payload = browser_call(client, Some(sid), method, p).await?;
            CommandOutput::Json(payload)
        }
        "eval" | "js" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser eval requires a surface"))?;
            let script = rest.join(" ");
            if script.trim().is_empty() {
                bail!("browser eval requires a script");
            }
            let payload = browser_call(client, Some(sid), "browser.eval", {
                let mut p = Map::new();
                p.insert("script".to_string(), Value::String(script));
                p
            })
            .await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                let value = payload.get("value").cloned().unwrap_or(Value::Null);
                let text = match value {
                    Value::Null => "null".to_string(),
                    Value::Bool(value) => value.to_string(),
                    Value::Number(value) => value.to_string(),
                    Value::String(value) => value,
                    other => serde_json::to_string(&other).unwrap_or_else(|_| "OK".to_string()),
                };
                CommandOutput::Text(text)
            }
        }
        "type" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser type requires a surface"))?;
            let selector = parse_opt(&browser_args, "--selector")
                .or_else(|| rest.first().cloned())
                .ok_or_else(|| anyhow!("browser type requires a selector"))?;
            let text = parse_opt(&browser_args, "--text")
                .or_else(|| rest.get(1).cloned())
                .ok_or_else(|| anyhow!("browser type requires text"))?;
            let payload = browser_call(client, Some(sid), "browser.type", {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p.insert("text".to_string(), Value::String(text));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "check" | "uncheck" | "focus" | "hover" | "dblclick" | "doubleclick"
        | "scroll-into-view" | "scroll_into_view" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let selector = parse_opt(&browser_args, "--selector")
                .or_else(|| rest.first().cloned())
                .ok_or_else(|| anyhow!("browser {} requires a selector", sub))?;
            let method = match sub.as_str() {
                "doubleclick" => "browser.dblclick".to_string(),
                "scroll-into-view" => "browser.scroll_into_view".to_string(),
                other => format!("browser.{}", other.replace('-', "_")),
            };
            let payload = browser_call(client, Some(sid), &method, {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "select" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser select requires a surface"))?;
            let selector = parse_opt(&browser_args, "--selector")
                .or_else(|| rest.first().cloned())
                .ok_or_else(|| anyhow!("browser select requires a selector"))?;
            let value = parse_opt(&browser_args, "--value")
                .or_else(|| rest.get(1).cloned())
                .ok_or_else(|| anyhow!("browser select requires a value"))?;
            let payload = browser_call(client, Some(sid), "browser.select", {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p.insert("value".to_string(), Value::String(value));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "scroll" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser scroll requires a surface"))?;
            let explicit_selector = parse_opt(&browser_args, "--selector");
            let amount_flag = parse_opt(&browser_args, "--amount");
            let (selector, amount) = if let Some(selector) = explicit_selector {
                (
                    Some(selector),
                    amount_flag.or_else(|| rest.first().cloned()),
                )
            } else if rest
                .first()
                .and_then(|value| value.parse::<u64>().ok())
                .is_some()
            {
                (None, amount_flag.or_else(|| rest.first().cloned()))
            } else {
                (
                    rest.first().cloned(),
                    amount_flag.or_else(|| rest.get(1).cloned()),
                )
            };
            let mut p = Map::new();
            if let Some(selector) = selector {
                p.insert("selector".to_string(), Value::String(selector));
            }
            if let Some(amount) = amount {
                let dy = amount
                    .parse::<u64>()
                    .map_err(|_| anyhow!("browser scroll amount must be a non-negative integer"))?;
                p.insert("dy".to_string(), Value::Number(dy.into()));
            }
            let payload = browser_call(client, Some(sid), "browser.scroll", p).await?;
            CommandOutput::Json(payload)
        }
        "press" | "keydown" | "keyup" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let key = rest
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("browser {} requires a key", sub))?;
            let method = format!("browser.{sub}");
            let payload = browser_call(client, Some(sid), &method, {
                let mut p = Map::new();
                p.insert("key".to_string(), Value::String(key));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "addscript" | "addinitscript" | "addstyle" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let content = rest.join(" ");
            if content.trim().is_empty() {
                bail!("browser {} requires content", sub);
            }
            let field = if sub == "addstyle" { "css" } else { "script" };
            let method = format!("browser.{}", sub);
            let mut p = Map::new();
            p.insert(field.to_string(), Value::String(content));
            let payload = browser_call(client, Some(sid), &method, p).await?;
            CommandOutput::Json(payload)
        }
        "console" | "errors" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let op = rest.first().cloned().unwrap_or_else(|| "list".to_string());
            let method = format!("browser.{}.{}", sub, op);
            let payload = browser_call(client, Some(sid), &method, Map::new()).await?;
            CommandOutput::Json(payload)
        }
        "highlight" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser highlight requires a surface"))?;
            let selector = rest.first().cloned().unwrap_or_default();
            let payload = browser_call(client, Some(sid), "browser.highlight", {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "state" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser state requires a surface"))?;
            let op = rest.first().cloned().unwrap_or_else(|| "save".to_string());
            let path = rest
                .get(1)
                .cloned()
                .ok_or_else(|| anyhow!("browser state {} requires a file path", op))?;
            let method = match op.as_str() {
                "save" => "browser.state.save",
                "load" => "browser.state.load",
                _ => bail!("Unsupported browser state subcommand: {}", op),
            };
            let payload = browser_call(client, Some(sid), method, {
                let mut p = Map::new();
                p.insert("path".to_string(), Value::String(path));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "viewport" => {
            bail!("not_supported: browser viewport is not supported in linux mock");
        }
        _ => {
            // Generic passthrough to browser.<sub>
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let method = format!("browser.{}", sub);
            let payload = browser_call(client, Some(sid), &method, Map::new()).await?;
            CommandOutput::Json(payload)
        }
    };

    Ok(output)
}

fn is_unsupported_tmux_cmd(cmd: &str) -> bool {
    matches!(cmd, "popup" | "bind-key" | "unbind-key" | "copy-mode")
}

async fn run_tmux_compat(client: &mut Client, command: &str, args: &[String]) -> Result<Value> {
    if is_unsupported_tmux_cmd(command) {
        bail!("not supported");
    }

    match command {
        "capture-pane" => run_read_screen(client, args).await,
        "pipe-pane" => {
            let capture = run_read_screen(client, args).await?;
            let text = get_string(&capture, &["text"]).unwrap_or_default();
            let shell_cmd = parse_opt(args, "--command")
                .ok_or_else(|| anyhow!("pipe-pane requires --command"))?;
            let mut child = Command::new("bash")
                .arg("-lc")
                .arg(shell_cmd)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("failed to spawn pipe-pane command")?;
            if let Some(stdin) = child.stdin.as_mut() {
                use std::io::Write;
                stdin
                    .write_all(text.as_bytes())
                    .context("failed to write pipe-pane stdin")?;
            }
            let status = child
                .wait()
                .context("failed waiting for pipe-pane command")?;
            if !status.success() {
                bail!("pipe-pane command failed");
            }
            Ok(json!({"ok": true}))
        }
        "wait-for" => {
            let signal = parse_flag(args, "-S") || parse_flag(args, "--signal");
            let name = trailing_title(args).ok_or_else(|| anyhow!("wait-for requires a name"))?;
            let timeout = parse_opt(args, "--timeout")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5);
            let path = wait_signal_path(&name);
            if signal {
                fs::write(&path, b"1").context("failed to write wait-for signal")?;
                Ok(json!({"ok": true, "name": name}))
            } else {
                let deadline = Instant::now() + Duration::from_secs(timeout);
                loop {
                    if path.exists() {
                        let _ = fs::remove_file(&path);
                        return Ok(json!({"ok": true, "name": name}));
                    }
                    if Instant::now() >= deadline {
                        bail!("wait-for timed out waiting for '{}'", name);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        "find-window" => {
            let needle = trailing_title(args).unwrap_or_default();
            let listed = client.call("workspace.list", json!({})).await?;
            let rows = listed
                .get("workspaces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut out = String::new();
            for row in rows {
                let title = get_string(&row, &["title", "name"]).unwrap_or_default();
                if title.contains(&needle) {
                    let handle = handle_from_payload(&row, "workspace_id", "workspace_ref");
                    out = format!("{} {}", handle, title);
                    break;
                }
            }
            Ok(json!({"text": out}))
        }
        "last-window" => client.call("workspace.last", json!({})).await,
        "next-window" => client.call("workspace.next", json!({})).await,
        "previous-window" => client.call("workspace.previous", json!({})).await,
        "swap-pane" => {
            let workspace = parse_opt(args, "--workspace");
            let pane =
                parse_opt(args, "--pane").ok_or_else(|| anyhow!("swap-pane requires --pane"))?;
            let target = parse_opt(args, "--target-pane")
                .ok_or_else(|| anyhow!("swap-pane requires --target-pane"))?;

            let source_surface =
                selected_surface_for_pane(client, workspace.clone(), &pane).await?;
            let target_surface =
                selected_surface_for_pane(client, workspace.clone(), &target).await?;

            let _ = call_in_workspace_scope(
                client,
                workspace.clone(),
                "surface.move",
                json!({"surface_id": source_surface, "target_pane_id": target, "index": 0}),
            )
            .await?;
            let _ = call_in_workspace_scope(
                client,
                workspace.clone(),
                "surface.move",
                json!({"surface_id": target_surface, "target_pane_id": pane, "index": 0}),
            )
            .await?;

            Ok(json!({"ok": true}))
        }
        "break-pane" => {
            let workspace = parse_opt(args, "--workspace");
            let pane = parse_opt(args, "--pane");
            let surface = parse_opt(args, "--surface");
            let mut p = Map::new();
            if let Some(pane) = pane {
                p.insert("pane_id".to_string(), Value::String(pane));
            }
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            call_in_workspace_scope(client, workspace, "pane.break", Value::Object(p)).await
        }
        "join-pane" => {
            let workspace = parse_opt(args, "--workspace");
            let pane = parse_opt(args, "--pane");
            let surface = parse_opt(args, "--surface");
            let target = parse_opt(args, "--target-pane")
                .ok_or_else(|| anyhow!("join-pane requires --target-pane"))?;
            let mut p = Map::new();
            p.insert("target_pane_id".to_string(), Value::String(target));
            if let Some(pane) = pane {
                p.insert("pane_id".to_string(), Value::String(pane));
            }
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            call_in_workspace_scope(client, workspace, "pane.join", Value::Object(p)).await
        }
        "last-pane" => {
            let workspace = parse_opt(args, "--workspace");
            call_in_workspace_scope(client, workspace, "pane.last", json!({})).await
        }
        "clear-history" => {
            let workspace = parse_opt(args, "--workspace");
            let surface = parse_opt(args, "--surface");
            let mut p = Map::new();
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            call_in_workspace_scope(client, workspace, "surface.clear_history", Value::Object(p))
                .await
        }
        "set-hook" => {
            let list_mode = parse_flag(args, "--list");
            let unset = parse_opt(args, "--unset");
            with_locked_json_map(&client.socket, "hooks", |hooks, path| {
                if list_mode {
                    let text = hooks
                        .iter()
                        .map(|(k, v)| format!("{}={}", k, v))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Ok(json!({
                        "text": text,
                        "path": path.display().to_string(),
                    }));
                }
                if let Some(name) = unset {
                    hooks.remove(&name);
                    write_json_map(path, hooks)?;
                    return Ok(json!({"ok": true}));
                }
                let name = args
                    .iter()
                    .find(|a| !a.starts_with('-'))
                    .cloned()
                    .unwrap_or_default();
                let body = trailing_title(args).unwrap_or_default();
                if name.is_empty() || body.is_empty() {
                    bail!("set-hook requires <name> <command>");
                }
                hooks.insert(name, body);
                write_json_map(path, hooks)?;
                Ok(json!({"ok": true}))
            })
        }
        "resize-pane" => {
            let workspace = parse_opt(args, "--workspace");
            let pane =
                parse_opt(args, "--pane").ok_or_else(|| anyhow!("resize-pane requires --pane"))?;
            let direction = if parse_flag(args, "-R") {
                "right"
            } else if parse_flag(args, "-L") {
                "left"
            } else if parse_flag(args, "-D") {
                "down"
            } else if parse_flag(args, "-U") {
                "up"
            } else {
                "right"
            };
            let amount = parse_opt(args, "--amount")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1);
            call_in_workspace_scope(
                client,
                workspace,
                "pane.resize",
                json!({"pane_id": pane, "direction": direction, "amount": amount}),
            )
            .await
        }
        "set-buffer" => {
            let name =
                parse_opt(args, "--name").ok_or_else(|| anyhow!("set-buffer requires --name"))?;
            let body = trailing_title(args).unwrap_or_default();
            with_locked_json_map(&client.socket, "buffers", |buffers, path| {
                buffers.insert(name, body);
                write_json_map(path, buffers)?;
                Ok(json!({"ok": true}))
            })
        }
        "list-buffers" => with_locked_json_map(&client.socket, "buffers", |buffers, _path| {
            let text = buffers.keys().cloned().collect::<Vec<_>>().join("\n");
            Ok(json!({"text": text}))
        }),
        "paste-buffer" => {
            let name =
                parse_opt(args, "--name").ok_or_else(|| anyhow!("paste-buffer requires --name"))?;
            let workspace = parse_opt(args, "--workspace");
            let surface = parse_opt(args, "--surface");
            let text = with_locked_json_map(&client.socket, "buffers", |buffers, _path| {
                Ok(buffers.get(&name).cloned().unwrap_or_default())
            })?;
            let mut p = Map::new();
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            p.insert("text".to_string(), Value::String(text));
            call_in_workspace_scope(client, workspace, "surface.send_text", Value::Object(p)).await
        }
        "respawn-pane" => {
            let workspace = parse_opt(args, "--workspace");
            let surface = parse_opt(args, "--surface");
            let command = parse_opt(args, "--command").unwrap_or_default();
            let mut p = Map::new();
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            p.insert("text".to_string(), Value::String(format!("{}\n", command)));
            call_in_workspace_scope(client, workspace, "surface.send_text", Value::Object(p)).await
        }
        "display-message" => {
            let msg = trailing_title(args).unwrap_or_default();
            Ok(json!({"text": msg}))
        }
        _ => bail!("unknown tmux command"),
    }
}

async fn execute_command(client: &mut Client, opts: &GlobalOptions) -> Result<CommandOutput> {
    if let Some(raw_request) = &opts.request {
        let request: V2Request =
            serde_json::from_str(raw_request).context("request must be a valid v2 JSON object")?;
        let mut payload = client.send_request(request).await?;
        apply_id_format(&mut payload, opts.id_format);
        return Ok(CommandOutput::Json(payload));
    }

    if opts.command_args.is_empty() {
        print_help();
        bail!("missing command");
    }

    let command = opts.command_args[0].as_str();
    let args = &opts.command_args[1..];
    let mut effective_id_format = opts.id_format;
    if command == "browser" {
        if let Some(raw) = parse_opt(args, "--id-format") {
            effective_id_format = IdFormat::parse(&raw)?;
        }
    }

    let mut out = match command {
        "identify" => CommandOutput::Json(run_identify(client, args).await?),
        "list-panels" | "list-panes" | "list-workspaces" | "surface-health" => {
            let payload = run_list(client, command, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(render_list_text(command, &payload))
            }
        }
        "send" => {
            let payload = run_send(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "surface_id", "surface_ref");
                CommandOutput::Text(format!("OK {}", handle.trim()))
            }
        }
        "send-key" => {
            let payload = run_send_key(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "surface_id", "surface_ref");
                CommandOutput::Text(format!("OK {}", handle.trim()))
            }
        }
        "notify" => {
            let payload = run_notify(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "list-notifications" | "notifications" => {
            let payload = run_list_notifications(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(notification_rows_text(&payload))
            }
        }
        "clear-notifications" => {
            let payload = run_clear_notifications(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "jump-notification" | "open-notification" => {
            let payload = run_jump_notification(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(notification_jump_text(&payload))
            }
        }
        "claude-hook" | "opencode-hook" | "gemini-hook" => {
            let agent = match command {
                "claude-hook" => agent_hooks::AgentKind::Claude,
                "opencode-hook" => agent_hooks::AgentKind::OpenCode,
                "gemini-hook" => agent_hooks::AgentKind::Gemini,
                _ => unreachable!(),
            };
            let payload = run_agent_hook(client, agent, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "hooks" => return run_hooks_command(client, args, opts.json_output).await,
        "commands" | "custom-commands" => {
            return run_project_commands_command(client, args, opts.json_output).await;
        }
        "run-command" => {
            let payload = run_project_command(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "workspace_id", "workspace_ref");
                let command = get_string(&payload, &["project_command"]).unwrap_or_default();
                CommandOutput::Text(format!("OK {handle} command={command}"))
            }
        }
        "new-workspace" => {
            let payload = run_new_workspace(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "workspace_id", "workspace_ref");
                CommandOutput::Text(format!("OK {}", handle))
            }
        }
        "ssh" => {
            let payload = run_ssh_workspace(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "workspace_id", "workspace_ref");
                CommandOutput::Text(format!("OK {}", handle))
            }
        }
        "close-workspace" => {
            let payload = run_close_workspace(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "agent-team" => {
            let payload = run_agent_team(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let agents_md = payload
                    .get("agents_md")
                    .and_then(|v| v.as_str())
                    .unwrap_or("AGENTS.md");
                let workspace = payload
                    .get("workspace_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let peers = payload
                    .get("peers")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|p| p.get("agent").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                CommandOutput::Text(format!(
                    "OK agent-team workspace={workspace} peers=[{peers}] agents_md={agents_md}"
                ))
            }
        }
        "sidebar-state" => {
            let payload = run_sidebar_state(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let workspace =
                    get_string(&payload, &["workspace"]).unwrap_or_else(|| "none".to_string());
                let cwd = get_string(&payload, &["cwd"]).unwrap_or_else(|| "none".to_string());
                let git_branch =
                    get_string(&payload, &["git_branch"]).unwrap_or_else(|| "none".to_string());
                let unread = payload
                    .get("unread")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let latest_notification = get_string(&payload, &["latest_notification"])
                    .unwrap_or_else(|| "none".to_string());
                let pr_number = payload
                    .get("pr_number")
                    .and_then(Value::as_u64)
                    .map(|number| number.to_string())
                    .unwrap_or_else(|| "none".to_string());
                let pr_status =
                    get_string(&payload, &["pr_status"]).unwrap_or_else(|| "none".to_string());
                let ports = payload
                    .get("ports")
                    .and_then(Value::as_array)
                    .map(|ports| {
                        ports
                            .iter()
                            .filter_map(|port| port.get("port").and_then(Value::as_u64))
                            .map(|port| port.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .filter(|ports| !ports.is_empty())
                    .unwrap_or_else(|| "none".to_string());
                CommandOutput::Text(format!(
                    "workspace={}\ncwd={}\ngit_branch={}\nunread={}\nlatest_notification={}\npr_number={}\npr_status={}\nports={}",
                    workspace,
                    cwd,
                    git_branch,
                    unread,
                    latest_notification,
                    pr_number,
                    pr_status,
                    ports
                ))
            }
        }
        "new-surface" => {
            let payload = run_new_surface(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "surface_id", "surface_ref");
                CommandOutput::Text(format!("OK {}", handle))
            }
        }
        "new-pane" => {
            let payload = run_new_pane(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "surface_id", "surface_ref");
                CommandOutput::Text(format!("OK {}", handle))
            }
        }
        "tab-action" => {
            let payload = run_tab_action(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else if let Some(help) = get_string(&payload, &["help"]) {
                CommandOutput::Text(help)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "rename-workspace" | "rename-window" => {
            let payload = run_rename_workspace_like(client, command, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "rename-tab" => {
            let payload = run_rename_tab(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "read-screen" | "capture-pane" => {
            let payload = run_read_screen(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(get_string(&payload, &["text"]).unwrap_or_default())
            }
        }
        "browser" => return run_browser(client, args, opts.json_output).await,
        "open-browser" => {
            let mut bridged = vec!["open".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "navigate-browser" => {
            let mut bridged = vec!["navigate".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "browser-back" => {
            let mut bridged = vec!["back".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "browser-forward" => {
            let mut bridged = vec!["forward".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "browser-reload" => {
            let mut bridged = vec!["reload".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "pipe-pane" | "wait-for" | "find-window" | "last-window" | "next-window"
        | "previous-window" | "swap-pane" | "break-pane" | "join-pane" | "last-pane"
        | "clear-history" | "set-hook" | "resize-pane" | "set-buffer" | "list-buffers"
        | "paste-buffer" | "respawn-pane" | "display-message" | "popup" | "bind-key"
        | "unbind-key" | "copy-mode" => {
            let payload = run_tmux_compat(client, command, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else if let Some(text) = get_string(&payload, &["text"]) {
                CommandOutput::Text(text)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        _ => bail!("unknown command: {}", command),
    };

    if let CommandOutput::Json(ref mut payload) = out {
        apply_id_format(payload, effective_id_format);
    }

    Ok(out)
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = parse_global_args()?;
    if should_launch_host(&opts) {
        return launch_host();
    }

    let socket = resolve_socket_path(opts.socket.clone(), opts.socket_mode);

    let mut client = Client::new(socket);
    let output = execute_command(&mut client, &opts).await;

    match output {
        Ok(CommandOutput::Text(text)) => {
            println!("{}", text);
            Ok(())
        }
        Ok(CommandOutput::Json(value)) => {
            if opts.pretty {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value)
                        .context("failed to pretty print response")?
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string(&value).context("failed to encode json output")?
                );
            }
            Ok(())
        }
        Err(err) => {
            eprintln!("{}", err);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod cli_arg_tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn default_opts(command_args: Vec<String>) -> GlobalOptions {
        GlobalOptions {
            socket: None,
            socket_mode: SocketMode::Runtime,
            json_output: false,
            id_format: IdFormat::Refs,
            request: None,
            pretty: false,
            command_args,
        }
    }

    #[test]
    fn no_args_launches_host_but_cli_flags_do_not() {
        assert!(should_launch_host(&default_opts(Vec::new())));

        let mut json_only = default_opts(Vec::new());
        json_only.json_output = true;
        assert!(!should_launch_host(&json_only));

        assert!(!should_launch_host(&default_opts(args(&[
            "list-workspaces"
        ]))));
    }

    #[test]
    fn host_binary_candidates_cover_installed_and_dev_layouts() {
        let installed = Path::new("/usr/bin/limux");
        let candidates = host_binary_candidates(installed);
        assert!(candidates.contains(&PathBuf::from("/usr/libexec/limux/limux-host")));
        assert!(!candidates.contains(&PathBuf::from("/usr/bin/limux")));

        let dev = Path::new("/repo/target/debug/limux-cli");
        let candidates = host_binary_candidates(dev);
        assert!(candidates.contains(&PathBuf::from("/repo/target/debug/limux")));
    }

    #[test]
    fn project_command_parser_accepts_object_and_array_forms() {
        let source = Path::new("/repo/cmux.json");
        let parsed = parse_project_commands_from_value(
            &json!({
                "commands": {
                    "dev": "npm run dev",
                    "test": {
                        "label": "Tests",
                        "cmd": ["npm", "test"],
                        "cwd": "web"
                    }
                }
            }),
            source,
        )
        .expect("project commands");

        let dev = parsed.iter().find(|command| command.name == "dev").unwrap();
        assert_eq!(dev.label, "dev");
        assert_eq!(dev.command, "npm run dev");
        assert_eq!(resolved_project_command_cwd(dev, None), "/repo");

        let test = parsed
            .iter()
            .find(|command| command.name == "test")
            .unwrap();
        assert_eq!(test.label, "Tests");
        assert_eq!(test.command, "'npm' 'test'");
        assert_eq!(resolved_project_command_cwd(test, None), "/repo/web");

        let array = parse_project_commands_from_value(
            &json!({
                "commands": [{
                    "id": "lint",
                    "run": "cargo clippy",
                    "cwd": "."
                }]
            }),
            source,
        )
        .expect("array commands");
        assert_eq!(array[0].name, "lint");
        assert_eq!(array[0].command, "cargo clippy");
    }

    #[test]
    fn browser_cookie_import_parses_json_and_netscape_files() {
        let json_rows = parse_browser_cookie_import_json(
            r#"{
                "cookies": [
                    {
                        "name": "sid",
                        "value": "123",
                        "domain": ".example.com",
                        "path": "/",
                        "secure": true,
                        "expirationDate": 1893456000
                    },
                    {
                        "key": "prefs",
                        "value": "dark",
                        "httpOnly": true
                    }
                ]
            }"#,
            Path::new("cookies.json"),
        )
        .expect("json cookies");

        assert_eq!(json_rows.len(), 2);
        assert_eq!(json_rows[0].name, "sid");
        assert_eq!(json_rows[0].domain.as_deref(), Some(".example.com"));
        assert!(json_rows[0].secure);
        assert_eq!(json_rows[0].expires_unix, Some(1893456000));
        assert!(json_rows[1].http_only);

        let netscape_rows = parse_browser_cookie_import_netscape(
            "# Netscape HTTP Cookie File\n#HttpOnly_.example.com\tTRUE\t/\tFALSE\t0\tsession\tabc\n.example.com\tTRUE\t/\tTRUE\t0\ttheme\tdark\n",
            Path::new("cookies.txt"),
        )
        .expect("netscape cookies");

        assert_eq!(netscape_rows.len(), 2);
        assert_eq!(netscape_rows[0].name, "session");
        assert_eq!(netscape_rows[0].domain.as_deref(), Some(".example.com"));
        assert!(netscape_rows[0].http_only);
        assert!(!netscape_rows[0].secure);
        assert_eq!(netscape_rows[0].expires_unix, None);
        assert_eq!(netscape_rows[1].name, "theme");
        assert!(netscape_rows[1].secure);
    }

    #[test]
    fn browser_profile_discovery_finds_chromium_and_firefox_profiles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let config = dir.path().join("config");
        let chrome_default = config.join("google-chrome/Default");
        let firefox_profile = home.join(".mozilla/firefox/abc.default-release");

        fs::create_dir_all(chrome_default.join("Network")).expect("chrome network dir");
        fs::create_dir_all(chrome_default.join("Sessions")).expect("chrome sessions dir");
        fs::write(chrome_default.join("Preferences"), "{}").expect("chrome preferences");
        fs::write(chrome_default.join("Network/Cookies"), "").expect("chrome cookies");
        fs::write(chrome_default.join("History"), "").expect("chrome history");

        fs::create_dir_all(&firefox_profile).expect("firefox profile");
        fs::write(
            home.join(".mozilla/firefox/profiles.ini"),
            "[Profile0]\nName=default-release\nIsRelative=1\nPath=abc.default-release\n",
        )
        .expect("firefox profiles.ini");
        fs::write(firefox_profile.join("cookies.sqlite"), "").expect("firefox cookies");
        fs::write(firefox_profile.join("places.sqlite"), "").expect("firefox places");

        let payload = browser_profiles_payload_in(
            &BrowserProfileDiscoveryDirs {
                home_dir: Some(home),
                config_dir: Some(config),
            },
            None,
            false,
        );
        let profiles = payload["profiles"].as_array().expect("profiles array");
        assert_eq!(payload["profile_count"], 2);
        assert!(profiles.iter().any(|profile| {
            profile["browser"] == "google-chrome"
                && profile["profile"] == "Default"
                && profile["cookie_store"]["exists"] == true
                && profile["history_store"]["exists"] == true
        }));
        assert!(profiles.iter().any(|profile| {
            profile["browser"] == "firefox"
                && profile["profile"] == "default-release"
                && profile["cookie_store"]["exists"] == true
                && profile["history_store"]["exists"] == true
        }));

        let chrome_payload = browser_profiles_payload_in(
            &BrowserProfileDiscoveryDirs {
                home_dir: None,
                config_dir: Some(dir.path().join("config")),
            },
            Some("chrome"),
            false,
        );
        assert_eq!(chrome_payload["profile_count"], 1);
    }

    #[test]
    fn project_command_config_search_prefers_nearest_then_cmux() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let nested = root.join("a/b");
        fs::create_dir_all(&nested).expect("nested dirs");
        fs::write(
            root.join("cmux.json"),
            r#"{"commands":{"root":"echo root"}}"#,
        )
        .expect("root cmux");
        fs::write(
            nested.join("limux.json"),
            r#"{"commands":{"nested":"echo nested"}}"#,
        )
        .expect("nested limux");

        assert_eq!(
            find_project_command_config_in(&nested),
            Some(nested.join("limux.json"))
        );

        fs::write(
            nested.join("cmux.json"),
            r#"{"commands":{"cmux":"echo cmux"}}"#,
        )
        .expect("nested cmux");
        assert_eq!(
            find_project_command_config_in(&nested),
            Some(nested.join("cmux.json"))
        );
    }

    #[test]
    fn project_command_workspace_request_resolves_cwd_and_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("cmux.json");
        fs::write(
            &config,
            r#"{
                "commands": {
                    "dev": {
                        "label": "Dev server",
                        "command": "npm run dev",
                        "cwd": "web"
                    }
                }
            }"#,
        )
        .expect("config");

        let request = build_project_command_workspace_request(&args(&[
            "--config",
            config.to_str().unwrap(),
            "dev",
        ]))
        .expect("workspace request");
        assert_eq!(request.command_name, "dev");
        assert_eq!(request.workspace_name, "Dev server");
        assert_eq!(request.command, "npm run dev");
        assert_eq!(
            request.cwd,
            dir.path().join("web").to_string_lossy().to_string()
        );

        let named = build_project_command_workspace_request(&args(&[
            "--config",
            config.to_str().unwrap(),
            "--name",
            "Frontend",
            "--cwd",
            "/tmp/frontend",
            "dev",
        ]))
        .expect("named workspace request");
        assert_eq!(named.workspace_name, "Frontend");
        assert_eq!(named.cwd, "/tmp/frontend");
    }

    #[test]
    fn ssh_workspace_request_builds_quoted_create_command() {
        let request = build_ssh_workspace_request(&args(&[
            "--cwd",
            "/repo",
            "-p",
            "2222",
            "dev@example.com",
        ]))
        .expect("ssh request");
        assert_eq!(request.name, "ssh:dev@example.com");
        assert_eq!(request.cwd.as_deref(), Some("/repo"));
        assert_eq!(request.target, "dev@example.com");
        assert_eq!(request.command, "ssh '-p' '2222' 'dev@example.com'");

        let named = build_ssh_workspace_request(&args(&[
            "--name",
            "prod",
            "--",
            "admin@prod.internal",
            "tmux attach",
        ]))
        .expect("named ssh request");
        assert_eq!(named.name, "prod");
        assert_eq!(named.target, "admin@prod.internal");
        assert_eq!(named.command, "ssh 'admin@prod.internal' 'tmux attach'");
    }

    #[test]
    fn ssh_workspace_request_rejects_missing_remote() {
        assert!(build_ssh_workspace_request(&args(&["--cwd", "/repo"])).is_err());
        assert!(build_ssh_workspace_request(&args(&["-p", "2222"])).is_err());
    }

    #[test]
    fn notification_rows_text_formats_empty_and_populated_lists() {
        assert_eq!(
            notification_rows_text(&json!({ "notifications": [] })),
            "none"
        );
        assert_eq!(
            notification_rows_text(&json!({
                "notifications": [{
                    "id": 7,
                    "unread": true,
                    "workspace_ref": "workspace:codex",
                    "surface_ref": "surface:4:tab-a",
                    "message": "Ready"
                }]
            })),
            "id=7 unread=true workspace=workspace:codex surface=surface:4:tab-a message=Ready"
        );
        assert_eq!(
            notification_jump_text(&json!({
                "notification": {
                    "id": 8,
                    "unread": false,
                    "workspace_ref": "workspace:codex",
                    "message": "Opened"
                }
            })),
            "id=8 unread=false workspace=workspace:codex surface=none message=Opened"
        );
    }

    #[test]
    fn notify_positional_title_skips_option_values() {
        let args = args(&[
            "--subtitle",
            "needs review",
            "--body",
            "blocked",
            "Input needed",
        ]);

        assert_eq!(trailing_title(&args).as_deref(), Some("Input needed"));
    }

    #[test]
    fn hook_event_comes_from_json_after_option_values() {
        let args = args(&["--workspace", "codex"]);
        let payload = json!({ "hook_event_name": "Notification" });

        assert_eq!(parse_hook_event(&args, &payload), "Notification");
    }

    #[test]
    fn hook_event_prefers_explicit_event_flag() {
        let args = args(&["--workspace", "codex", "--event", "Stop"]);
        let payload = json!({ "hook_event_name": "Notification" });

        assert_eq!(parse_hook_event(&args, &payload), "Stop");
    }

    #[test]
    fn hook_event_accepts_positional_event_after_options() {
        let args = args(&["--workspace", "codex", "Stop"]);
        let payload = json!({ "hook_event_name": "Notification" });

        assert_eq!(parse_hook_event(&args, &payload), "Stop");
    }

    #[test]
    fn external_session_end_preserves_restorable_hook_session() {
        assert_eq!(
            agent_hook_persistence_action("SessionEnd"),
            AgentHookPersistenceAction::Preserve
        );
        assert_eq!(
            agent_hook_persistence_action("session-end"),
            AgentHookPersistenceAction::Preserve
        );
    }

    #[test]
    fn internal_cleanup_removes_restorable_hook_session() {
        assert_eq!(
            agent_hook_persistence_action("cleanup"),
            AgentHookPersistenceAction::Remove
        );
        assert_eq!(
            agent_hook_persistence_action("restore-exit"),
            AgentHookPersistenceAction::Remove
        );
    }

    #[test]
    fn default_hook_setup_omits_opencode_until_supported() {
        assert_eq!(
            default_hook_targets(),
            vec![
                agent_hooks::AgentKind::Codex,
                agent_hooks::AgentKind::Claude,
                agent_hooks::AgentKind::Gemini,
            ]
        );
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::OpenCode));
    }

    #[test]
    fn opencode_plugin_embeds_installer_cli_command() {
        let source = opencode_plugin_source_with_command("/tmp/limux-cli").expect("plugin source");

        assert!(source.contains("const LIMUX_COMMAND = \"/tmp/limux-cli\";"));
        assert!(source.contains("process.env.LIMUX_BIN || LIMUX_COMMAND"));
        assert!(!source.contains("process.env.LIMUX_BIN || \"limux\""));
    }

    #[test]
    fn opencode_plugin_removes_only_deleted_sessions() {
        let source = opencode_plugin_source_with_command("/tmp/limux-cli").expect("plugin source");

        assert!(
            source.contains("if (type === \"session.error\") send(\"session-end\", ctx, event);")
        );
        assert!(source.contains("if (type === \"session.deleted\") send(\"cleanup\", ctx, event);"));
        assert!(source.contains("type === \"session.status\""));
        assert!(source.contains("type === \"session.compacted\""));
    }

    #[test]
    fn stop_hook_output_matches_codex_schema_shape() {
        let output = agent_hook_output("stop", &json!({ "session_id": "session-a" }));

        assert_eq!(
            output,
            json!({
                "continue": true,
                "suppressOutput": false
            })
        );
    }

    #[test]
    fn session_start_hook_output_uses_camel_case_specific_output() {
        let output = agent_hook_output(
            "session-start",
            &json!({ "additionalContext": "Limux session restore tracking active." }),
        );

        assert_eq!(
            output,
            json!({
                "continue": true,
                "suppressOutput": false,
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": "Limux session restore tracking active."
                }
            })
        );
    }

    #[test]
    fn claude_hook_install_writes_required_matcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        install_json_hooks(
            &path,
            agent_hooks::AgentKind::Claude,
            &[("SessionStart", "session-start")],
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        let entry = &root["hooks"]["SessionStart"][0];
        assert_eq!(entry["matcher"], "*");
        assert_eq!(entry["hooks"][0]["timeout"], 5);
        assert!(entry["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks claude session-start"));
    }

    #[test]
    fn codex_hook_install_keeps_codex_schema_without_matcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hooks.json");

        install_json_hooks(
            &path,
            agent_hooks::AgentKind::Codex,
            &[("SessionStart", "session-start")],
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read hooks")).expect("json");
        let entry = &root["hooks"]["SessionStart"][0];
        assert!(entry.get("matcher").is_none());
        assert_eq!(entry["hooks"][0]["timeout"], 5000);
        assert!(entry["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks codex session-start"));
    }

    #[test]
    fn environ_parser_reads_requested_limux_value() {
        let environ = b"PATH=/bin\0LIMUX_WORKSPACE_ID=ws-1\0LIMUX_SURFACE_ID=7:tab-a\0";

        assert_eq!(
            env_value_from_environ(environ, "LIMUX_WORKSPACE_ID").as_deref(),
            Some("ws-1")
        );
        assert_eq!(
            env_value_from_environ(environ, "LIMUX_SURFACE_ID").as_deref(),
            Some("7:tab-a")
        );
        assert_eq!(env_value_from_environ(environ, "LIMUX_PANE_ID"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_parser_handles_process_names_with_spaces() {
        let stat = "1234 (claude hook sh) S 987 1 1 0 -1 4194560";

        assert_eq!(parse_proc_stat_parent_pid(stat), Some(987));
    }

    #[test]
    fn hook_session_id_falls_back_to_transcript_stem() {
        let payload = json!({
            "transcript_path": "/home/amwill/.claude/projects/-home-amwill-Applications-limux/268746f1-5a8f-471c-85db-dc50649c2f9c.jsonl"
        });

        assert_eq!(
            hook_session_id(&payload).as_deref(),
            Some("268746f1-5a8f-471c-85db-dc50649c2f9c")
        );
    }

    #[test]
    fn hook_session_id_prefers_explicit_session_id() {
        let payload = json!({
            "session_id": "explicit-session",
            "transcript_path": "/tmp/transcript-session.jsonl"
        });

        assert_eq!(
            hook_session_id(&payload).as_deref(),
            Some("explicit-session")
        );
    }
}

#[cfg(test)]
mod agent_team_tests {
    use super::*;

    #[test]
    fn agent_launch_known() {
        for agent in [
            "codex",
            "claude",
            "claude-code",
            "opencode",
            "gemini",
            "gemini-cli",
        ] {
            assert!(
                agent_launch_command(agent).is_some(),
                "expected '{agent}' to be a known agent"
            );
        }
    }

    #[test]
    fn agent_launch_unknown_returns_none() {
        assert!(agent_launch_command("nonsense-cli").is_none());
    }

    #[test]
    fn agents_md_contains_protocol_and_peers() {
        let peers = vec![
            (
                "codex".to_string(),
                "10".to_string(),
                "10:tab-a".to_string(),
                "codex".to_string(),
            ),
            (
                "claude".to_string(),
                "11".to_string(),
                "11:tab-a".to_string(),
                "claude".to_string(),
            ),
        ];
        let md = build_agents_md(
            &peers,
            "/tmp/team",
            "active-ws",
            "ws-uuid-123",
            "9:terminal-orch",
        );

        // Header & generation marker
        assert!(md.contains("AGENTS.md — agent-to-agent message protocol"));
        assert!(md.contains("Generated by `limux agent-team`"));

        // Team workspace block
        assert!(md.contains("Workspace name: `active-ws`"));
        assert!(md.contains("Workspace ID: `ws-uuid-123`"));
        assert!(md.contains("Orchestrator surface: `9:terminal-orch`"));
        assert!(md.contains("Shared cwd: `/tmp/team`"));

        // Peer table rows (Agent | Pane | Surface | Launch)
        assert!(md.contains("| `codex` | `10` | `10:tab-a` | `codex` |"));
        assert!(md.contains("| `claude` | `11` | `11:tab-a` | `claude` |"));

        // Protocol envelope spec uses --surface, not --workspace
        assert!(md.contains("<agent-msg from=\"codex\" to=\"claude\""));
        assert!(md.contains("limux send --surface"));
        assert!(!md.contains("limux send --workspace"));
        assert!(md.contains("reply-to"));

        // Notify + env contract
        assert!(md.contains("limux notify"));
        assert!(md.contains("LIMUX_WORKSPACE_ID"));
        assert!(md.contains("LIMUX_SURFACE_ID"));
        assert!(md.contains("limux new-pane --direction right --command bash"));
        assert!(md.contains("Live GTK self-spawn currently supports terminal"));
    }
}

#[cfg(test)]
mod new_pane_tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn test_env(name: &str) -> Option<String> {
        match name {
            "LIMUX_WORKSPACE_ID" => Some("workspace:agent".to_string()),
            "LIMUX_SURFACE_ID" => Some("surface:11:tab-a".to_string()),
            "LIMUX_PANE_ID" => Some("pane:11".to_string()),
            _ => None,
        }
    }

    #[test]
    fn new_pane_serializes_env_defaults_and_command() {
        let (workspace, params) = build_new_pane_request(&args(&["--command", "claude"]), test_env);

        assert_eq!(workspace.as_deref(), Some("workspace:agent"));
        assert_eq!(
            params,
            json!({
                "direction": "right",
                "type": "terminal",
                "surface_id": "surface:11:tab-a",
                "pane_id": "pane:11",
                "command": "claude"
            })
        );
    }

    #[test]
    fn new_pane_flags_override_env_and_preserve_raw_refs() {
        let (workspace, params) = build_new_pane_request(
            &args(&[
                "--workspace",
                "raw-workspace",
                "--surface",
                "7:tab-b",
                "--pane",
                "7",
                "--direction",
                "down",
                "--type",
                "terminal",
                "--command",
                "codex --ask-for-approval never",
            ]),
            test_env,
        );

        assert_eq!(workspace.as_deref(), Some("raw-workspace"));
        assert_eq!(
            params,
            json!({
                "direction": "down",
                "type": "terminal",
                "surface_id": "7:tab-b",
                "pane_id": "7",
                "command": "codex --ask-for-approval never"
            })
        );
    }

    #[test]
    fn new_pane_without_env_preserves_active_workspace_fallback() {
        let (workspace, params) = build_new_pane_request(&args(&[]), |_| None);

        assert_eq!(workspace, None);
        assert_eq!(
            params,
            json!({
                "direction": "right",
                "type": "terminal"
            })
        );
    }
}
