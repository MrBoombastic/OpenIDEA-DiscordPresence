//! Discord Presence for OpenIDEA: shows the file and project you are working
//! on as your Discord activity ("Rich Presence").
//!
//! It talks to the Discord desktop app over its local IPC socket
//! (`\\.\pipe\discord-ipc-N` on Windows, `discord-ipc-N` in the runtime or
//! temporary directory elsewhere) with the `ipc:` capability of plugin API 1.4.
//! The connection lives as long as the plugin instance: when the IDE reclaims
//! the idle plugin (or after `idleMinutes` without editor activity) the
//! presence is cleared, and the next editor event shows it again.

use openidea_plugin::host::{self, Connection, storage};
use openidea_plugin::{
    Context, Error, Event, Invocation, LogLevel, Plugin, StatusItem, register_plugin,
};
use serde_json::{Value, json};

/// The Discord application ("OpenIDEA"): its name is what Discord shows after
/// "Playing", and its Rich Presence art assets are the image keys below.
pub const CLIENT_ID: &str = "1570755627544739840";
/// The application's art asset shown as the image.
pub const IMAGE: &str = "openidea";
/// Where the "Get OpenIDEA" button leads.
pub const OPENIDEA_URL: &str = "https://openidea.sh";

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

/// Where the Discord app listens, in the order tried (each with `-0` … `-9`).
/// Every entry has a matching `ipc:` grant in plugin.toml.
pub const SOCKET_DIRS: &[&str] = &[
    r"\\.\pipe\",
    "$XDG_RUNTIME_DIR/",
    "$XDG_RUNTIME_DIR/app/com.discordapp.Discord/",
    "$XDG_RUNTIME_DIR/snap.discord/",
    "$TMPDIR/",
    "/tmp/",
];

const STATUS_ITEM: &str = "discord";
/// When the current session started (seconds); kept across idle reclaim.
const SESSION_START: &str = "session-start";
/// The snapshot shown (JSON), so a woken instance shows the same window.
const SHOWN: &str = "shown";
/// Last editor activity (seconds).
const LAST_SEEN: &str = "last-seen";
/// The socket that worked last time, tried first.
const LAST_SOCKET: &str = "socket";
/// Turned off with the status bar item / Toggle command (until turned on again).
const PAUSED: &str = "paused";

/// A new session starts after this long without activity.
const SESSION_GAP_SECS: u64 = 30 * 60;
/// How long a new connection waits for READY.
const READY_WAIT: std::time::Duration = std::time::Duration::from_secs(25);
/// Don't retry a failed connection more often than this.
const RETRY_SECS: u64 = 30;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Wire format: little-endian op and length, then UTF-8 JSON.

pub fn encode(op: u32, payload: &Value) -> Vec<u8> {
    let body = payload.to_string().into_bytes();
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Splits complete frames off the front of `buf`; an incomplete tail stays.
pub fn decode(buf: &mut Vec<u8>) -> Vec<(u32, Value)> {
    let mut frames = Vec::new();
    loop {
        if buf.len() < 8 {
            break;
        }
        let op = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        if buf.len() < 8 + len {
            break;
        }
        let value = serde_json::from_slice(&buf[8..8 + len]).unwrap_or(Value::Null);
        buf.drain(..8 + len);
        frames.push((op, value));
    }
    frames
}

// ---------------------------------------------------------------------------
// What to show.

/// What the presence is built from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The project's folder: identifies the window the presence follows.
    pub root: Option<String>,
    pub file: Option<String>,
    pub language: Option<String>,
    pub project: Option<String>,
    pub branch: Option<String>,
    /// A tool or framework recognised by the file name (`package.json` →
    /// npm): its art asset and name, shown instead of the language logo.
    pub tool: Option<(String, String)>,
    /// A file that may hold secrets (`.env`, keys, certificates…).
    pub sensitive: bool,
    /// The repository's web page (from the git remote, without credentials).
    pub repo: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub details: String,
    pub state: String,
    pub idle_details: String,
    pub idle_state: String,
    pub language_images: bool,
    pub timestamp: String,
    pub idle_minutes: u64,
    /// `clear`, `idle` (show `idle_text`) or `keep`.
    pub idle_action: String,
    pub idle_text: String,
    pub reset_timer_on_return: bool,
    pub privacy_mode: bool,
    pub hide_sensitive_files: bool,
    pub repository_button: bool,
    pub openidea_button: bool,
    pub custom_button_label: String,
    pub custom_button_url: String,
    pub status_bar: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            details: "Editing {file}".into(),
            state: "in {project}".into(),
            idle_details: "Browsing {project}".into(),
            idle_state: "".into(),
            language_images: true,
            timestamp: "session".into(),
            idle_minutes: 5,
            idle_action: "clear".into(),
            idle_text: "Idle in {project}".into(),
            reset_timer_on_return: false,
            privacy_mode: false,
            hide_sensitive_files: true,
            repository_button: false,
            openidea_button: true,
            custom_button_label: "".into(),
            custom_button_url: "".into(),
            status_bar: true,
        }
    }
}

impl Snapshot {
    fn with_project_name(mut self) -> Self {
        self.project = self.root.as_deref().map(last_segment);
        self
    }

    pub fn to_json(&self) -> String {
        json!({
            "root": self.root, "file": self.file, "language": self.language, "branch": self.branch,
            "tool": self.tool.as_ref().map(|(k, n)| [k, n]), "sensitive": self.sensitive, "repo": self.repo,
        })
        .to_string()
    }

    pub fn from_json(text: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(text).ok()?;
        let get = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let tool = v.get("tool").and_then(Value::as_array).and_then(|t| {
            Some((t.first()?.as_str()?.to_owned(), t.get(1)?.as_str()?.to_owned()))
        });
        Some(
            Self {
                root: get("root"),
                file: get("file"),
                language: get("language"),
                project: None,
                branch: get("branch"),
                tool,
                sensitive: v.get("sensitive").and_then(Value::as_bool).unwrap_or(false),
                repo: get("repo"),
            }
            .with_project_name(),
        )
    }
}

impl Settings {
    pub fn read() -> Self {
        let d = Self::default();
        let s = |key: &str, default: &String| host::setting_string(key).unwrap_or_else(|| default.clone());
        Self {
            enabled: host::setting_bool("enabled").unwrap_or(d.enabled)
                && storage::get(PAUSED).ok().flatten().is_none(),
            details: s("details", &d.details),
            state: s("state", &d.state),
            idle_details: s("idleDetails", &d.idle_details),
            idle_state: s("idleState", &d.idle_state),
            language_images: host::setting_bool("languageImages").unwrap_or(d.language_images),
            timestamp: s("timestamp", &d.timestamp),
            idle_minutes: host::setting_number("idleMinutes").map(|n| n.max(0.0) as u64).unwrap_or(d.idle_minutes),
            idle_action: s("idleAction", &d.idle_action),
            idle_text: s("idleText", &d.idle_text),
            reset_timer_on_return: host::setting_bool("resetTimerOnReturn").unwrap_or(d.reset_timer_on_return),
            privacy_mode: host::setting_bool("privacyMode").unwrap_or(d.privacy_mode),
            hide_sensitive_files: host::setting_bool("hideSensitiveFiles").unwrap_or(d.hide_sensitive_files),
            repository_button: host::setting_bool("repositoryButton").unwrap_or(d.repository_button),
            openidea_button: host::setting_bool("openideaButton").unwrap_or(d.openidea_button),
            custom_button_label: s("customButtonLabel", &d.custom_button_label),
            custom_button_url: s("customButtonUrl", &d.custom_button_url),
            status_bar: host::setting_bool("statusBar").unwrap_or(d.status_bar),
        }
    }
}

/// `rust` → `Rust`, `typescriptreact` → `TypeScript React`.
pub fn language_name(id: &str) -> String {
    let known = match id {
        "rust" => "Rust",
        "java" => "Java",
        "kotlin" => "Kotlin",
        "groovy" => "Groovy",
        "scala" => "Scala",
        "javascript" => "JavaScript",
        "javascriptreact" => "JavaScript React",
        "typescript" => "TypeScript",
        "typescriptreact" => "TypeScript React",
        "python" => "Python",
        "go" => "Go",
        "c" => "C",
        "cpp" => "C++",
        "csharp" => "C#",
        "fsharp" => "F#",
        "php" => "PHP",
        "ruby" => "Ruby",
        "swift" => "Swift",
        "dart" => "Dart",
        "lua" => "Lua",
        "html" => "HTML",
        "css" => "CSS",
        "scss" => "SCSS",
        "less" => "Less",
        "json" | "jsonc" => "JSON",
        "yaml" => "YAML",
        "toml" => "TOML",
        "xml" => "XML",
        "sql" => "SQL",
        "markdown" => "Markdown",
        "shellscript" | "shell" | "bash" => "Shell",
        "powershell" => "PowerShell",
        "dockerfile" => "Dockerfile",
        "vue" => "Vue",
        "svelte" => "Svelte",
        "solidity" => "Solidity",
        "zig" => "Zig",
        "haskell" => "Haskell",
        "elixir" => "Elixir",
        "erlang" => "Erlang",
        "clojure" => "Clojure",
        "r" => "R",
        "bat" => "Batch",
        "dotenv" => ".env",
        "go.mod" => "Go Module",
        "go.work" => "Go Workspace",
        "objective-c" => "Objective-C",
        "objective-cpp" => "Objective-C++",
        "ocaml" | "ocaml.interface" => "OCaml",
        "gdscript" => "GDScript",
        "graphql" => "GraphQL",
        "latex" => "LaTeX",
        "cmake" => "CMake",
        "django-html" => "Django Template",
        "erb" => "ERB",
        "jsp" => "JSP",
        "hcl" => "HCL",
        "terraform" | "terraform-vars" => "Terraform",
        "asciidoc" => "AsciiDoc",
        "nix" => "Nix",
        "sass" => "Sass",
        "jupyter" => "Jupyter Notebook",
        "cuda" => "CUDA",
        "razor" => "Razor",
        "dotnet" => ".NET project",
        "vb" => "Visual Basic",
        "xaml" => "XAML",
        "svg" => "SVG",
        "mdx" => "MDX",
        "postcss" => "PostCSS",
        "nushell" => "Nushell",
        "clojurescript" => "ClojureScript",
        "coffeescript" => "CoffeeScript",
        "purescript" => "PureScript",
        "rescript" => "ReScript",
        "reason" => "Reason",
        "matlab" => "MATLAB",
        "apl" => "APL",
        "awk" => "AWK",
        "glsl" => "GLSL",
        "hlsl" => "HLSL",
        "wgsl" => "WGSL",
        "wasm" => "WebAssembly",
        "proto" => "Protocol Buffers",
        "qml" => "QML",
        "ejs" => "EJS",
        "renpy" => "Ren'Py",
        "unity" => "Unity",
        "unreal" => "Unreal Engine",
        "cucumber" => "Gherkin",
        "gdshader" => "Godot Shader",
        "godot-resource" => "Godot Resource",
        "ignore" => "Git",
        "makefile" => "Makefile",
        "plaintext" | "text" | "" => "Text",
        _ => "",
    };
    if !known.is_empty() {
        return known.into();
    }
    let mut chars = id.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn last_segment(path: &str) -> String {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(path).to_owned()
}

/// Fills `{file}`, `{language}`, `{project}` and `{branch}`; Discord wants
/// 2–128 characters, so short text is padded and long text cut.
pub fn fill(template: &str, s: &Snapshot) -> Option<String> {
    let lang = s.language.as_deref().map(language_name).unwrap_or_default();
    let text = template
        .replace("{file}", s.file.as_deref().unwrap_or(""))
        .replace("{language}", &lang)
        .replace("{project}", s.project.as_deref().unwrap_or(""))
        .replace("{branch}", s.branch.as_deref().unwrap_or(""));
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_matches(|c: char| c == '·' || c == '-' || c == '|' || c.is_whitespace()).to_owned();
    if text.is_empty() {
        return None;
    }
    let mut text: String = text.chars().take(128).collect();
    while text.chars().count() < 2 {
        text.push(' ');
    }
    Some(text)
}

/// What may be shown of `s`: privacy mode keeps only the language, and a
/// file that may hold secrets is shown as "a private file".
pub fn redact(settings: &Settings, s: &Snapshot) -> Snapshot {
    let mut d = s.clone();
    let secret = settings.hide_sensitive_files && s.sensitive;
    if settings.privacy_mode {
        let lang = s.language.as_deref().map(language_name).filter(|l| l != "Text");
        d.file = s.file.as_ref().map(|_| match lang {
            Some(l) => format!("a {l} file"),
            None => "a file".into(),
        });
        d.project = s.project.as_ref().map(|_| "a project".into());
        d.branch = None;
        d.repo = None;
    }
    if secret {
        d.file = s.file.as_ref().map(|_| "a private file".into());
        d.language = None;
        d.tool = None;
    }
    d
}

/// The activity's buttons (Discord shows them to others, not to you).
pub fn buttons(settings: &Settings, s: &Snapshot) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(url) = s.repo.as_ref().filter(|_| settings.repository_button && !settings.privacy_mode) {
        out.push(json!({ "label": "View Repository", "url": url }));
    }
    let label = settings.custom_button_label.trim();
    let url = settings.custom_button_url.trim();
    if !label.is_empty()
        && (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() <= 512
        && !url.contains(char::is_whitespace)
    {
        out.push(json!({ "label": label.chars().take(32).collect::<String>(), "url": url }));
    }
    if settings.openidea_button {
        out.push(json!({ "label": "Get OpenIDEA", "url": OPENIDEA_URL }));
    }
    out.truncate(2);
    out
}

/// The Discord `activity` object (without timestamps).
pub fn activity(settings: &Settings, s: &Snapshot) -> Value {
    let s = &redact(settings, s);
    let editing = s.file.is_some();
    let (details, state) =
        if editing { (&settings.details, &settings.state) } else { (&settings.idle_details, &settings.idle_state) };
    let mut a = serde_json::Map::new();
    if let Some(d) = fill(details, s) {
        a.insert("details".into(), d.into());
    }
    if let Some(st) = fill(state, s) {
        a.insert("state".into(), st.into());
    }
    let mut assets = serde_json::Map::new();
    let lang = s.language.as_deref().filter(|l| !l.is_empty());
    let logo = s
        .tool
        .clone()
        .or_else(|| lang.and_then(|l| Some((language_image(l)?.to_owned(), language_name(l)))));
    match logo {
        Some((key, text)) if settings.language_images && editing => {
            assets.insert("large_image".into(), key.into());
            assets.insert("large_text".into(), text.into());
            assets.insert("small_image".into(), IMAGE.into());
            assets.insert("small_text".into(), "OpenIDEA".into());
        }
        _ => {
            assets.insert("large_image".into(), IMAGE.into());
            let text = lang.filter(|_| editing).map(language_name).unwrap_or_else(|| "OpenIDEA".into());
            assets.insert("large_text".into(), text.into());
        }
    }
    a.insert("assets".into(), assets.into());
    let buttons = buttons(settings, s);
    if !buttons.is_empty() {
        a.insert("buttons".into(), buttons.into());
    }
    Value::Object(a)
}

/// Shown after `idleMinutes` without activity when `idleAction` is `idle`.
pub fn idle_activity(settings: &Settings, s: &Snapshot) -> Value {
    let s = &redact(settings, s);
    let mut a = serde_json::Map::new();
    a.insert("details".into(), fill(&settings.idle_text, s).unwrap_or_else(|| "Idle".into()).into());
    a.insert("assets".into(), json!({ "large_image": IMAGE, "large_text": "OpenIDEA" }));
    let buttons = buttons(settings, s);
    if !buttons.is_empty() {
        a.insert("buttons".into(), buttons.into());
    }
    Value::Object(a)
}

/// The web page of a git remote, without credentials: `git@host:o/r.git`,
/// `ssh://git@host:22/o/r`, `https://user:token@host/o/r.git?x#y` →
/// `https://host/o/r`. Anything else (local paths, file://) gives `None`.
pub fn remote_web_url(remote: &str) -> Option<String> {
    let r = remote.trim();
    let (host, path) = if let Some(rest) =
        r.strip_prefix("https://").or_else(|| r.strip_prefix("http://")).or_else(|| r.strip_prefix("ssh://")).or_else(|| r.strip_prefix("git://"))
    {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        let host = host.split(':').next()?;
        (host.to_owned(), path.to_owned())
    } else {
        // scp-like: [user@]host:path
        let (left, path) = r.split_once(':')?;
        if left.contains('/') || left.contains('\\') || left.len() < 2 {
            return None;
        }
        (left.rsplit('@').next()?.to_owned(), path.to_owned())
    };
    let path = path.split(['?', '#']).next()?.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    let host_ok = host.contains('.')
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    let path_ok = !path.is_empty()
        && path.contains('/')
        && path.chars().all(|c| c.is_ascii_alphanumeric() || "-._~/".contains(c))
        && !path.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..");
    (host_ok && path_ok).then(|| format!("https://{}/{path}", host.to_ascii_lowercase()))
}

// ---------------------------------------------------------------------------
// The plugin.

#[derive(Default)]
pub struct DiscordPresence {
    conn: Option<Connection>,
    inbox: Vec<u8>,
    /// The activity last sent (`Null`: cleared), to skip repeats.
    pub sent: Option<Value>,
    retry_after: u64,
    nonce: u64,
    file_since: (Option<String>, u64),
    /// Last connection problem, for the status bar and Reconnect.
    pub last_error: Option<String>,
    shown: Option<Snapshot>,
    /// Discord sent READY: it accepted the application id.
    ready: bool,
    /// The activity to show (`Null`: none), sent once READY.
    want: Option<Value>,
    /// Nonce of the SET_ACTIVITY not answered yet.
    awaiting: Option<String>,
    closed_reason: Option<String>,
}

/// What caused a refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// A file was focused or opened, or a command ran: show this window.
    Switch,
    /// A save or VCS change: refresh only if it is the window shown.
    Touch,
    /// Settings changed: send again.
    Settings,
    /// The schedule: idle check, reconnect.
    Timer,
}

impl DiscordPresence {
    fn connect(&mut self, client_id: &str) -> Result<(), Error> {
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(Some(last)) = storage::get(LAST_SOCKET) {
            candidates.push(last);
        }
        for dir in SOCKET_DIRS {
            for i in 0..10 {
                let name = format!("{dir}discord-ipc-{i}");
                if !candidates.contains(&name) {
                    candidates.push(name);
                }
            }
        }
        let mut last_err = Error::NotFound("Discord is not running".into());
        for name in candidates {
            let conn = match host::ipc_connect(&name) {
                Ok(c) => c,
                Err(e @ Error::Denied(_)) => {
                    last_err = e;
                    continue;
                }
                Err(_) => continue,
            };
            conn.write(&encode(OP_HANDSHAKE, &json!({ "v": 1, "client_id": client_id })))?;
            let _ = storage::set(LAST_SOCKET, &name);
            self.conn = Some(conn);
            self.inbox.clear();
            self.ready = false;
            self.awaiting = None;
            self.sent = None;
            return self.wait_ready();
        }
        Err(last_err)
    }

    /// Waits for READY. Discord checks the application id with its API
    /// first, which can take 10 s or more (Vesktop: seconds to never when
    /// connections are frequent), so this runs in a task (60 s budget).
    /// Without READY the connection stays open and a later pump may get it.
    fn wait_ready(&mut self) -> Result<(), Error> {
        let started = std::time::Instant::now();
        let mut task = None;
        while !self.ready && started.elapsed() < READY_WAIT {
            if task.is_none() && started.elapsed() > std::time::Duration::from_secs(2) {
                task = Some(host::begin_task("Connecting to Discord", false));
            }
            if !self.pump(500) {
                self.disconnect();
                return Err(Error::Failed(
                    self.closed_reason.take().unwrap_or_else(|| "Discord closed the connection".into()),
                ));
            }
        }
        if !self.ready {
            host::log(LogLevel::Warn, "Discord has not sent READY yet");
        }
        Ok(())
    }

    fn disconnect(&mut self) {
        if let Some(c) = self.conn.take() {
            let _ = c.close();
        }
        self.inbox.clear();
        self.ready = false;
        self.awaiting = None;
        self.sent = None;
    }

    /// Reads what Discord sent: READY, answers, pings, errors. `false`: closed.
    fn pump(&mut self, timeout_ms: u32) -> bool {
        let Some(conn) = &self.conn else { return false };
        match conn.read(64 * 1024, timeout_ms) {
            Ok(bytes) => self.inbox.extend(bytes),
            Err(_) => return false,
        }
        let mut open = true;
        for (op, v) in decode(&mut self.inbox) {
            match op {
                OP_PING => {
                    let _ = conn.write(&encode(OP_PONG, &v));
                }
                OP_CLOSE => {
                    let msg = v.get("message").and_then(Value::as_str).unwrap_or("closed by Discord");
                    host::log(LogLevel::Warn, &format!("Discord closed the connection: {msg}"));
                    self.closed_reason = Some(format!("Discord closed the connection: {msg}"));
                    open = false;
                }
                // The SDK's mock host echoes what is written: its handshake stands for READY.
                #[cfg(test)]
                OP_HANDSHAKE => self.ready = true,
                _ => {
                    let evt = v.get("evt").and_then(Value::as_str);
                    if evt == Some("READY") {
                        self.ready = true;
                    }
                    let error = (evt == Some("ERROR")).then(|| {
                        let msg = v.pointer("/data/message").and_then(Value::as_str).unwrap_or("unknown error");
                        host::log(LogLevel::Warn, &format!("Discord: {msg}"));
                        msg.to_owned()
                    });
                    let nonce = v.get("nonce").and_then(Value::as_str);
                    if nonce.is_some() && nonce == self.awaiting.as_deref() {
                        self.awaiting = None;
                        self.last_error = error;
                    }
                }
            }
        }
        open
    }

    fn send_activity(&mut self, activity: &Value) -> Result<(), Error> {
        let conn = self.conn.as_ref().ok_or(Error::NotFound("connection".into()))?;
        self.nonce += 1;
        let mut args = json!({ "pid": 1 });
        if !activity.is_null() {
            args["activity"] = activity.clone();
        }
        let nonce = format!("oi-{}", self.nonce);
        let frame = json!({ "cmd": "SET_ACTIVITY", "args": args, "nonce": nonce });
        conn.write(&encode(OP_FRAME, &frame))?;
        self.awaiting = Some(nonce);
        for _ in 0..6 {
            if !self.pump(250) {
                return Err(Error::Failed("Discord closed the connection".into()));
            }
            if self.awaiting.is_none() {
                return Ok(());
            }
        }
        // Answered later (or never); a later pump clears this.
        host::log(LogLevel::Warn, "Discord hasn't answered SET_ACTIVITY yet");
        self.last_error = Some("Discord hasn't confirmed the activity yet".into());
        Ok(())
    }

    /// Sends the wanted activity once Discord is READY and it changed.
    fn flush(&mut self) -> Result<(), Error> {
        if self.conn.is_none() {
            return Ok(());
        }
        if !self.ready && !self.pump(0) {
            return Err(Error::Failed(
                self.closed_reason.take().unwrap_or_else(|| "Discord closed the connection".into()),
            ));
        }
        if !self.ready {
            self.last_error = Some("Waiting for Discord to accept the application".into());
            return Ok(());
        }
        let Some(want) = self.want.clone() else { return Ok(()) };
        if self.sent.as_ref() == Some(&want) {
            return Ok(());
        }
        self.send_activity(&want)?;
        self.sent = Some(want);
        Ok(())
    }

    /// Shows `activity` (`Null` clears it), connecting when needed.
    fn publish(&mut self, activity: Value) {
        self.want = Some(activity.clone());
        if self.conn.is_none() {
            if activity.is_null() || now_secs() < self.retry_after {
                return;
            }
            self.last_error = None;
            if let Err(e) = self.connect(CLIENT_ID) {
                host::log(LogLevel::Warn, &format!("can't connect to Discord: {e}"));
                self.retry_after = now_secs() + RETRY_SECS;
                self.last_error = Some(e.to_string());
                return;
            }
        }
        if let Err(e) = self.flush() {
            host::log(LogLevel::Warn, &format!("Discord connection lost: {e}"));
            // Discord went away (or restarted): try again on the next event or timer.
            self.disconnect();
            self.retry_after = now_secs() + RETRY_SECS;
            self.last_error = Some(e.to_string());
        }
    }

    fn clear(&mut self) {
        if self.conn.is_some() && self.ready {
            self.publish(Value::Null);
        }
        self.want = None;
        self.disconnect();
    }

    fn snapshot(ctx: &Invocation, focused: Option<&str>, settings: &Settings) -> Snapshot {
        let editor = ctx.editor();
        let path = editor.as_ref().map(|e| e.path()).filter(|p| !p.is_empty()).or_else(|| focused.map(str::to_owned));
        let language = editor.as_ref().map(|e| e.language());
        let project = ctx.project();
        let root = project.as_ref().and_then(|p| p.root().ok());
        let branch = project.as_ref().and_then(|p| p.vcs_status().ok().flatten()).and_then(|s| s.branch);
        // Only asked for when the button is on: nothing else needs it.
        let repo = (settings.repository_button && !settings.privacy_mode)
            .then(|| project.as_ref().and_then(|p| p.vcs_remotes().ok()))
            .flatten()
            .and_then(|remotes| {
                let origin = remotes.iter().find(|(name, _)| name == "origin").or(remotes.first());
                origin.and_then(|(_, url)| remote_web_url(url))
            });
        Snapshot {
            root,
            file: path.as_deref().map(last_segment),
            language: pick_language(language.as_deref(), path.as_deref()),
            project: None,
            branch,
            tool: path.as_deref().and_then(tool_of).map(|(k, n)| (k.to_owned(), n.to_owned())),
            sensitive: path.as_deref().is_some_and(is_sensitive),
            repo,
        }
        .with_project_name()
    }

    fn start_time(&mut self, settings: &Settings, s: &Snapshot) -> Option<u64> {
        let now = now_secs();
        match settings.timestamp.as_str() {
            "none" => None,
            "file" => {
                if self.file_since.0 != s.file {
                    self.file_since = (s.file.clone(), now);
                }
                Some(self.file_since.1)
            }
            _ => {
                let last = storage::get(LAST_SEEN).ok().flatten().and_then(|v| v.parse::<u64>().ok());
                let start = storage::get(SESSION_START).ok().flatten().and_then(|v| v.parse::<u64>().ok());
                // Back from being idle starts a new session when asked to.
                let gap = if settings.reset_timer_on_return && settings.idle_minutes > 0 {
                    settings.idle_minutes * 60
                } else {
                    SESSION_GAP_SECS
                };
                match (start, last) {
                    (Some(start), Some(last)) if now.saturating_sub(last) < gap => Some(start),
                    _ => {
                        let _ = storage::set(SESSION_START, &now.to_string());
                        Some(now)
                    }
                }
            }
        }
    }

    fn show_status(&self, settings: &Settings) {
        if !settings.status_bar {
            host::remove_status_item(STATUS_ITEM);
            return;
        }
        let (text, tooltip) = if !settings.enabled {
            ("Discord: off", "Discord Presence is turned off. Click to turn it on.".to_owned())
        } else if self.conn.is_some() && self.last_error.is_none() && self.sent.as_ref().is_some_and(|v| !v.is_null()) {
            ("Discord", "Showing your activity on Discord. Click to turn it off.".to_owned())
        } else {
            let why = self.last_error.clone().unwrap_or_else(|| "Not connected".into());
            ("Discord: –", format!("{why}. Click to turn Discord Presence off."))
        };
        host::set_status_item(
            STATUS_ITEM,
            &StatusItem { text: text.into(), tooltip: Some(tooltip), command: Some("toggle".into()) },
        );
    }

    /// The snapshot shown last (kept across idle reclaim).
    fn shown(&mut self) -> Option<Snapshot> {
        if self.shown.is_none() {
            self.shown = storage::get(SHOWN).ok().flatten().and_then(|v| Snapshot::from_json(&v));
        }
        self.shown.clone()
    }

    fn set_shown(&mut self, s: Option<Snapshot>) {
        match &s {
            Some(s) => {
                let _ = storage::set(SHOWN, &s.to_json());
            }
            None => {
                let _ = storage::delete(SHOWN);
            }
        }
        self.shown = s;
    }

    /// Recomputes and sends the presence.
    ///
    /// The plugin has one instance for all windows and the presence follows
    /// the window the user worked in last: focusing or opening a file moves
    /// it there, while saves and VCS changes in other windows (a background
    /// fetch, Save All) are ignored, and the timer re-sends what is shown.
    pub fn refresh(&mut self, ctx: &Invocation, focused: Option<&str>, trigger: Trigger) {
        let settings = Settings::read();
        let now = now_secs();
        if !settings.enabled {
            self.clear();
            self.show_status(&settings);
            return;
        }
        let ctx_root = || ctx.project().and_then(|p| p.root().ok());
        let next = match trigger {
            Trigger::Switch => Some(Self::snapshot(ctx, focused, &settings)),
            Trigger::Touch => {
                let owner = self.shown().and_then(|s| s.root);
                (owner.is_none() || owner == ctx_root()).then(|| Self::snapshot(ctx, focused, &settings))
            }
            Trigger::Settings => {
                self.sent = None;
                None
            }
            Trigger::Timer => None,
        };
        if let Some(s) = next {
            // `start_time` reads the previous activity time: update it after.
            let start = self.start_time(&settings, &s);
            let _ = storage::set(LAST_SEEN, &now.to_string());
            self.set_shown(Some(s.clone()));
            self.send_snapshot(&settings, &s, start);
        } else {
            let last = storage::get(LAST_SEEN).ok().flatten().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            let idle = settings.idle_minutes > 0 && now.saturating_sub(last) >= settings.idle_minutes * 60;
            let shown = self.shown();
            // The window shown was closed: wait for the next event.
            let closed = shown.as_ref().and_then(|s| s.root.as_ref()).is_some_and(|root| {
                host::projects().is_ok_and(|ps| !ps.iter().any(|p| p.root().ok().as_ref() == Some(root)))
            });
            let keep = settings.idle_action == "keep";
            if last == 0 || closed || (idle && settings.idle_action != "idle" && !keep) {
                if closed {
                    self.set_shown(None);
                }
                self.clear();
            } else if let Some(s) = shown.as_ref().filter(|_| idle && !keep) {
                let mut a = idle_activity(&settings, s);
                if settings.timestamp != "none" {
                    a["timestamps"] = json!({ "start": last });
                }
                self.publish(a);
            } else if let Some(s) = shown.filter(|_| self.conn.is_none() || self.sent.is_none()) {
                // Discord started after the IDE, the plugin was woken, or settings changed.
                let start = self.start_time(&settings, &s);
                self.send_snapshot(&settings, &s, start);
            } else if self.conn.is_some() && (!self.pump(0) || self.flush().is_err()) {
                self.disconnect();
            }
        }
        self.show_status(&settings);
    }

    fn send_snapshot(&mut self, settings: &Settings, s: &Snapshot, start: Option<u64>) {
        let mut a = activity(settings, s);
        if let Some(start) = start {
            a["timestamps"] = json!({ "start": start });
        }
        self.publish(a);
    }
}

/// The art asset of a language (OpenIDEA language id), if the application has one.
/// Several ids share a logo; the PNGs are in the repository's `assets/`.
pub fn language_image(id: &str) -> Option<&'static str> {
    Some(match id {
        "arduino" => "arduino",
        "asciidoc" => "asciidoc",
        "astro" => "astro",
        "bat" => "batch",
        "c" => "c",
        "clojure" => "clojure",
        "cmake" => "cmake",
        "cpp" => "cpp",
        "csharp" => "csharp",
        "css" => "css",
        "d" => "d",
        "dart" => "dart",
        "django-html" => "django",
        "dockerfile" => "docker",
        "dotenv" => "dotenv",
        "erb" => "rails",
        "elixir" => "elixir",
        "erlang" => "erlang",
        "fsharp" => "fsharp",
        "gdscript" | "gdshader" | "godot-resource" => "godot",
        "gitattributes" | "ignore" => "git",
        "go" | "go.mod" | "go.work" => "go",
        "graphql" => "graphql",
        "groovy" => "groovy",
        "haskell" => "haskell",
        "hcl" | "terraform" | "terraform-vars" => "terraform",
        "html" => "html",
        "java" | "jsp" => "java",
        "javascript" => "javascript",
        "javascriptreact" | "typescriptreact" => "react",
        "json" | "jsonc" => "json",
        "julia" => "julia",
        "kotlin" => "kotlin",
        "latex" => "latex",
        "less" => "less",
        "lua" => "lua",
        "makefile" => "makefile",
        "markdown" => "markdown",
        "nix" => "nix",
        "objective-c" | "objective-cpp" => "objective_c",
        "ocaml" | "ocaml.interface" => "ocaml",
        "perl" => "perl",
        "php" => "php",
        "powershell" => "powershell",
        "prisma" => "prisma",
        "python" => "python",
        "r" => "r",
        "ruby" => "ruby",
        "rust" => "rust",
        "sass" | "scss" => "sass",
        "scala" => "scala",
        "shellscript" => "bash",
        "solidity" => "solidity",
        "sql" => "sql",
        "svelte" => "svelte",
        "swift" => "swift",
        "toml" => "toml",
        "typescript" => "typescript",
        "vue" => "vue",
        "xml" => "xml",
        "yaml" => "yaml",
        "zig" => "zig",
        "apl" => "apl",
        "ballerina" => "ballerina",
        "cairo" => "cairo",
        "carbon" => "carbon",
        "clojurescript" => "clojurescript",
        "coffeescript" => "coffeescript",
        "crystal" => "crystal",
        "cucumber" | "feature" | "gherkin" => "cucumber",
        "dotnet" | "xaml" | "msbuild" => "dotnet",
        "ejs" => "ejs",
        "elm" => "elm",
        "fish" | "zsh" | "awk" => "bash",
        "fortran" | "fortran-modern" | "fortranfreeform" => "fortran",
        "glsl" | "hlsl" | "wgsl" => "opengl",
        "handlebars" | "mustache" => "handlebars",
        "haxe" => "haxe",
        "jupyter" => "jupyter",
        "matlab" => "matlab",
        "mdx" => "mdx",
        "nim" => "nim",
        "nushell" => "nushell",
        "odin" => "odin",
        "postcss" => "postcss",
        "processing" => "processing",
        "proto" | "proto3" | "protobuf" => "grpc",
        "pug" | "jade" => "pug",
        "purescript" => "purescript",
        "qml" => "qt",
        "racket" => "racket",
        "razor" | "aspnetcorerazor" => "blazor",
        "reason" => "reason",
        "renpy" => "renpy",
        "rescript" => "rescript",
        "stylus" => "stylus",
        "svg" => "svg",
        "unity" => "unity",
        "unreal" => "unreal",
        "vala" => "vala",
        "vb" => "visualbasic",
        "vyper" => "vyper",
        "wasm" | "wat" => "wasm",
        _ => return None,
    })
}

/// The editor's language, or one from the file name when the editor has
/// none (or one without a logo, such as plain text for `.nim`).
pub fn pick_language(editor: Option<&str>, path: Option<&str>) -> Option<String> {
    let editor = editor.filter(|l| !l.is_empty());
    let from_path = || path.and_then(language_from_path);
    match editor {
        Some(l) if language_image(l).is_some() => Some(l.to_owned()),
        Some(l) => from_path().filter(|p| language_image(p).is_some()).or_else(|| Some(l.to_owned())),
        None => from_path(),
    }
}

/// A language id from the file name, for when the editor doesn't say.
pub fn language_from_path(path: &str) -> Option<String> {
    let name = last_segment(path).to_lowercase();
    let by_name = match name.as_str() {
        "dockerfile" | "containerfile" => Some("dockerfile"),
        "makefile" | "gnumakefile" => Some("makefile"),
        "cmakelists.txt" => Some("cmake"),
        "jenkinsfile" => Some("groovy"),
        "gemfile" | "rakefile" | "podfile" | "vagrantfile" => Some("ruby"),
        ".gitignore" | ".gitattributes" | ".gitmodules" => Some("ignore"),
        _ if name.starts_with("dockerfile.") || name.ends_with(".dockerfile") => Some("dockerfile"),
        _ if name == ".env" || name.starts_with(".env.") => Some("dotenv"),
        _ => None,
    };
    if let Some(id) = by_name {
        return Some(id.into());
    }
    let ext = name.rsplit_once('.')?.1;
    let id = match ext {
        "rs" => "rust",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "groovy" | "gradle" => "groovy",
        "scala" | "sc" => "scala",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "py" | "pyi" | "pyw" => "python",
        "ipynb" => "jupyter",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "ino" => "cpp",
        "cu" | "cuh" => "cuda",
        "cs" | "csx" => "csharp",
        "razor" | "cshtml" => "razor",
        "csproj" | "fsproj" | "vbproj" | "sln" | "slnx" | "props" => "dotnet",
        "fs" | "fsi" | "fsx" => "fsharp",
        "vb" | "vbs" => "vb",
        "php" => "php",
        "rb" | "rake" | "gemspec" => "ruby",
        "erb" => "erb",
        "swift" => "swift",
        "m" | "mm" => "objective-c",
        "dart" => "dart",
        "lua" => "lua",
        "html" | "htm" | "xhtml" => "html",
        "css" => "css",
        "scss" | "sass" => "scss",
        "less" => "less",
        "styl" => "stylus",
        "pcss" | "postcss" => "postcss",
        "json" | "json5" => "json",
        "jsonc" => "jsonc",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" | "xsd" | "xsl" | "xslt" | "plist" => "xml",
        "xaml" | "axaml" => "xaml",
        "svg" => "svg",
        "sql" | "psql" | "mysql" => "sql",
        "md" | "markdown" => "markdown",
        "mdx" => "mdx",
        "adoc" | "asciidoc" => "asciidoc",
        "tex" | "sty" | "cls" | "bib" => "latex",
        "sh" | "bash" | "zsh" | "ksh" => "shellscript",
        "fish" => "fish",
        "nu" => "nushell",
        "ps1" | "psm1" | "psd1" => "powershell",
        "bat" | "cmd" => "bat",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "sol" => "solidity",
        "vy" => "vyper",
        "cairo" => "cairo",
        "zig" | "zon" => "zig",
        "hs" | "lhs" => "haskell",
        "ex" | "exs" | "heex" => "elixir",
        "erl" | "hrl" => "erlang",
        "clj" | "cljc" | "edn" => "clojure",
        "cljs" => "clojurescript",
        "r" | "rmd" => "r",
        "jl" => "julia",
        "pl" | "pm" => "perl",
        "ml" => "ocaml",
        "mli" => "ocaml.interface",
        "nim" | "nims" | "nimble" => "nim",
        "cr" => "crystal",
        "elm" => "elm",
        "purs" => "purescript",
        "hx" => "haxe",
        "coffee" => "coffeescript",
        "rkt" => "racket",
        "f" | "f90" | "f95" | "f03" | "f08" | "for" => "fortran",
        "mat" | "mlx" => "matlab",
        "vala" | "vapi" => "vala",
        "bal" => "ballerina",
        "carbon" => "carbon",
        "apl" => "apl",
        "awk" => "awk",
        "pde" => "processing",
        "odin" => "odin",
        "res" | "resi" => "rescript",
        "re" | "rei" => "reason",
        "wat" | "wast" | "wasm" => "wasm",
        "glsl" | "vert" | "frag" | "geom" | "comp" | "tesc" | "tese" => "glsl",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
        "prisma" => "prisma",
        "tf" | "tfvars" | "hcl" => "terraform",
        "nix" => "nix",
        "d" => "d",
        "gd" => "gdscript",
        "gdshader" => "gdshader",
        "tscn" | "tres" | "godot" => "godot-resource",
        "qml" => "qml",
        "hbs" | "handlebars" | "mustache" => "handlebars",
        "pug" | "jade" => "pug",
        "ejs" => "ejs",
        "feature" => "cucumber",
        "rpy" => "renpy",
        "unity" | "prefab" => "unity",
        "uproject" | "uplugin" => "unreal",
        "cmake" => "cmake",
        "mk" => "makefile",
        "dockerfile" => "dockerfile",
        "env" => "dotenv",
        _ => return None,
    };
    Some(id.into())
}

/// A tool or framework recognised by its file (`package.json`, `vite.config.ts`,
/// `.github/workflows/ci.yml` …): its art asset and the name shown.
pub fn tool_of(path: &str) -> Option<(&'static str, &'static str)> {
    let norm = path.replace('\\', "/").to_lowercase();
    let name = last_segment(&norm);
    let name = name.as_str();
    // `vite.config.ts`, `vite.config.mjs` …: the part before `.config`.
    let config = name.split_once(".config.").map(|(a, _)| a);
    if norm.contains("/.github/workflows/") && (name.ends_with(".yml") || name.ends_with(".yaml")) {
        return Some(("githubactions", "GitHub Actions"));
    }
    if norm.contains("/.github/") && name.starts_with("dependabot.") {
        return Some(("dependabot", "Dependabot"));
    }
    Some(match (name, config) {
        ("package.json" | "package-lock.json" | ".npmrc" | ".npmignore", _) => ("npm", "npm"),
        ("yarn.lock" | ".yarnrc" | ".yarnrc.yml", _) => ("yarn", "Yarn"),
        ("pnpm-lock.yaml" | "pnpm-workspace.yaml", _) => ("pnpm", "pnpm"),
        ("bun.lock" | "bun.lockb" | "bunfig.toml", _) => ("bun", "Bun"),
        ("deno.json" | "deno.jsonc" | "deno.lock", _) => ("deno", "Deno"),
        (".nvmrc" | ".node-version", _) => ("nodejs", "Node.js"),
        ("build.gradle" | "build.gradle.kts" | "settings.gradle" | "settings.gradle.kts" | "gradle.properties"
        | "gradlew" | "gradlew.bat" | "libs.versions.toml", _) => ("gradle", "Gradle"),
        ("pom.xml" | "mvnw" | "mvnw.cmd", _) => ("maven", "Maven"),
        ("cargo.toml" | "cargo.lock", _) => ("rust", "Cargo"),
        ("go.mod" | "go.sum" | "go.work", _) => ("go", "Go Modules"),
        ("poetry.lock" | "poetry.toml", _) => ("poetry", "Poetry"),
        ("requirements.txt" | "pipfile" | "pipfile.lock" | "setup.py" | "setup.cfg" | "pyproject.toml", _) => {
            ("python", "Python")
        }
        ("environment.yml" | "environment.yaml", _) => ("anaconda", "Conda"),
        ("composer.json" | "composer.lock", _) => ("composer", "Composer"),
        ("gemfile" | "gemfile.lock", _) => ("ruby", "Bundler"),
        ("pubspec.yaml" | "pubspec.lock", _) => ("flutter", "Flutter"),
        ("androidmanifest.xml", _) => ("android", "Android"),
        ("package.swift", _) => ("swift", "Swift Package"),
        ("podfile" | "podfile.lock" | "project.pbxproj", _) => ("xcode", "Xcode"),
        ("project.godot", _) => ("godot", "Godot"),
        ("docker-compose.yml" | "docker-compose.yaml" | "compose.yml" | "compose.yaml" | ".dockerignore", _) => {
            ("docker", "Docker Compose")
        }
        ("chart.yaml" | "values.yaml" | "helmfile.yaml", _) => ("helm", "Helm"),
        ("kustomization.yaml" | "kustomization.yml", _) => ("kubernetes", "Kubernetes"),
        ("ansible.cfg" | "playbook.yml" | "playbook.yaml", _) => ("ansible", "Ansible"),
        ("vagrantfile", _) => ("vagrant", "Vagrant"),
        ("jenkinsfile", _) => ("jenkins", "Jenkins"),
        (".gitlab-ci.yml", _) => ("gitlab", "GitLab CI"),
        (".travis.yml", _) => ("travis", "Travis CI"),
        ("nginx.conf", _) => ("nginx", "NGINX"),
        ("vercel.json", _) => ("vercel", "Vercel"),
        ("netlify.toml", _) => ("netlify", "Netlify"),
        ("wrangler.toml" | "wrangler.json" | "wrangler.jsonc", _) => ("cloudflare", "Cloudflare Workers"),
        ("firebase.json" | ".firebaserc", _) => ("firebase", "Firebase"),
        ("tauri.conf.json", _) => ("tauri", "Tauri"),
        ("angular.json", _) => ("angular", "Angular"),
        ("nx.json", _) => ("nx", "Nx"),
        ("turbo.json", _) => ("turborepo", "Turborepo"),
        ("biome.json" | "biome.jsonc", _) => ("biome", "Biome"),
        ("renovate.json" | "renovate.json5" | ".renovaterc", _) => ("renovate", "Renovate"),
        (".editorconfig", _) => ("editorconfig", "EditorConfig"),
        (".babelrc", _) => ("babel", "Babel"),
        _ if name.starts_with(".eslintrc") || name == ".eslintignore" => ("eslint", "ESLint"),
        _ if name.starts_with(".prettierrc") || name == ".prettierignore" => ("prettier", "Prettier"),
        _ if name.starts_with("tsconfig") && name.ends_with(".json") => ("typescript", "TypeScript"),
        _ if name.starts_with("jsconfig") && name.ends_with(".json") => ("javascript", "JavaScript"),
        _ if name.ends_with(".csproj") || name.ends_with(".sln") || name.ends_with(".slnx") => ("dotnet", ".NET"),
        (_, Some(tool)) => match tool {
            "vite" => ("vite", "Vite"),
            "vitest" => ("vitest", "Vitest"),
            "tailwind" => ("tailwind", "Tailwind CSS"),
            "postcss" => ("postcss", "PostCSS"),
            "next" => ("nextjs", "Next.js"),
            "nuxt" => ("nuxt", "Nuxt"),
            "svelte" => ("svelte", "Svelte"),
            "astro" => ("astro", "Astro"),
            "webpack" => ("webpack", "webpack"),
            "rollup" => ("rollup", "Rollup"),
            "eslint" => ("eslint", "ESLint"),
            "prettier" => ("prettier", "Prettier"),
            "babel" => ("babel", "Babel"),
            "jest" => ("jest", "Jest"),
            "playwright" => ("playwright", "Playwright"),
            "cypress" => ("cypress", "Cypress"),
            "electron-builder" | "forge" => ("electron", "Electron"),
            "tsup" | "esbuild" => ("esbuild", "esbuild"),
            _ => return None,
        },
        _ if norm.contains("/.storybook/") => ("storybook", "Storybook"),
        _ => return None,
    })
}

/// Files that may hold secrets: shown as "a private file", without its name,
/// when `hideSensitiveFiles` is on.
pub fn is_sensitive(path: &str) -> bool {
    let norm = path.replace('\\', "/").to_lowercase();
    if ["/.ssh/", "/.gnupg/", "/.aws/", "/.kube/", "/.docker/"].iter().any(|d| norm.contains(d)) {
        return true;
    }
    let name = last_segment(&norm);
    let exact = [
        ".env", ".envrc", ".netrc", "_netrc", ".npmrc", ".pypirc", ".pgpass", ".git-credentials", ".htpasswd",
        "credentials", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "known_hosts", "authorized_keys",
        "kubeconfig", "terraform.tfstate", "terraform.tfstate.backup", "service-account.json",
        "google-services.json", "googleservice-info.plist", "local.properties",
    ];
    if exact.contains(&name.as_str()) || name.starts_with(".env.") || name.ends_with(".env") {
        // `.env.example` and friends are meant to be shared.
        return !["example", "sample", "template", "dist", "defaults"].iter().any(|s| name.ends_with(s));
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let secret_ext = [
        "pem", "key", "p12", "pfx", "jks", "keystore", "crt", "cer", "der", "csr", "p8", "ppk", "asc", "gpg",
        "pgp", "kdbx", "ovpn", "tfstate", "tfvars", "age", "secret",
    ];
    let words = ["secret", "secrets", "credential", "credentials", "passwd", "passwords"];
    secret_ext.contains(&ext)
        || name.starts_with("secret")
        || name.starts_with("credential")
        || name.split(['.', '-', '_']).any(|w| words.contains(&w))
}

impl Plugin for DiscordPresence {
    fn activate(&mut self, _ctx: &Context) -> Result<(), Error> {
        Ok(())
    }

    fn deactivate(&mut self) {
        self.clear();
    }

    fn run_command(&mut self, id: &str, ctx: &Invocation, _arg: Option<&str>) -> Result<Option<String>, Error> {
        match id {
            "toggle" => {
                if storage::get(PAUSED)?.is_some() {
                    storage::delete(PAUSED)?;
                } else {
                    storage::set(PAUSED, "1")?;
                }
                self.refresh(ctx, None, Trigger::Switch);
                Ok(None)
            }
            "reconnect" => {
                self.disconnect();
                self.retry_after = 0;
                self.refresh(ctx, None, Trigger::Switch);
                match &self.last_error {
                    Some(e) => Err(Error::Failed(e.clone())),
                    None => Ok(Some("Connected to Discord.".into())),
                }
            }
            _ => Err(Error::NotFound(format!("command {id}"))),
        }
    }

    fn handle_events(&mut self, events: &[Event], ctx: &Invocation) {
        let mut focused: Option<String> = None;
        let mut trigger = Trigger::Timer;
        let mut raise = |t: Trigger| {
            let rank = |t: Trigger| match t {
                Trigger::Switch => 3,
                Trigger::Touch => 2,
                Trigger::Settings => 1,
                Trigger::Timer => 0,
            };
            if rank(t) > rank(trigger) {
                trigger = t;
            }
        };
        for e in events {
            match e {
                Event::EditorFocused(p) => {
                    focused = p.clone();
                    raise(Trigger::Switch);
                }
                Event::FileOpened(p) => {
                    focused.get_or_insert_with(|| p.clone());
                    raise(Trigger::Switch);
                }
                Event::FileSaved(p) => {
                    focused.get_or_insert_with(|| p.clone());
                    raise(Trigger::Touch);
                }
                Event::VcsChanged(_) => raise(Trigger::Touch),
                Event::Other { kind, .. } if kind == "settings-changed" => raise(Trigger::Settings),
                _ => {}
            }
        }
        self.refresh(ctx, focused.as_deref(), trigger);
    }
}

register_plugin!(DiscordPresence);

#[cfg(test)]
mod tests {
    use super::*;
    use openidea_plugin::mock::MockHost;

    const CAPS: &[&str] =
        &["events", "storage", "project.read", "vcs.read", "ipc:$XDG_RUNTIME_DIR/discord-ipc-*"];

    fn host() -> MockHost {
        MockHost::install().with_capabilities(CAPS)
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = encode(OP_FRAME, &json!({"cmd": "SET_ACTIVITY"}));
        buf.extend(encode(OP_PING, &json!({})));
        let tail = encode(OP_FRAME, &json!({"x": 1}));
        buf.extend(&tail[..5]);
        let frames = decode(&mut buf);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], (OP_FRAME, json!({"cmd": "SET_ACTIVITY"})));
        assert_eq!(frames[1].0, OP_PING);
        assert_eq!(buf, tail[..5].to_vec(), "an incomplete frame waits for more bytes");
        assert_eq!(&encode(OP_HANDSHAKE, &json!({}))[..8], &[0, 0, 0, 0, 2, 0, 0, 0]);
    }

    #[test]
    fn templates() {
        let s = Snapshot {
            file: Some("main.rs".into()),
            language: Some("rust".into()),
            project: Some("openidea".into()),
            ..Snapshot::default()
        };
        assert_eq!(fill("Editing {file}", &s).unwrap(), "Editing main.rs");
        assert_eq!(fill("{project} · {branch}", &s).unwrap(), "openidea", "an empty placeholder drops its separator");
        assert_eq!(fill("{branch}", &s), None);
        assert_eq!(fill("A", &s).unwrap(), "A ", "Discord wants at least two characters");
        assert_eq!(fill(&"x".repeat(300), &s).unwrap().len(), 128);
        assert_eq!(language_name("typescriptreact"), "TypeScript React");
        assert_eq!(language_name("nim"), "Nim");
        assert_eq!(language_from_path("C:/x/Foo.KT").as_deref(), Some("kotlin"));
    }

    #[test]
    fn activity_shape() {
        let s = Snapshot {
            file: Some("App.java".into()),
            language: Some("java".into()),
            project: Some("shop".into()),
            branch: Some("main".into()),
            ..Snapshot::default()
        };
        let mut settings = Settings::default();
        let a = activity(&settings, &s);
        assert_eq!(a["details"], "Editing App.java");
        assert_eq!(a["state"], "in shop");
        assert_eq!(a["assets"]["large_image"], "java");
        assert_eq!(a["assets"]["large_text"], "Java");
        assert_eq!(a["assets"]["small_image"], "openidea");
        // A language without a logo, or language images off: the OpenIDEA logo.
        let txt = Snapshot { language: Some("plaintext".into()), ..s.clone() };
        assert_eq!(activity(&settings, &txt)["assets"]["large_image"], "openidea");
        let tsx = Snapshot { language: Some("typescriptreact".into()), ..s.clone() };
        assert_eq!(activity(&settings, &tsx)["assets"]["large_image"], "react");
        assert_eq!(activity(&settings, &tsx)["assets"]["large_text"], "TypeScript React");
        settings.language_images = false;
        let a = activity(&settings, &s);
        assert_eq!(a["assets"]["large_image"], "openidea");
        assert_eq!(a["assets"]["large_text"], "Java");
        assert!(a["assets"].get("small_image").is_none());
        settings.language_images = true;
        let idle = activity(&settings, &Snapshot { project: Some("shop".into()), ..Snapshot::default() });
        assert_eq!(idle["details"], "Browsing shop");
        assert!(idle.get("state").is_none());
        assert_eq!(idle["assets"]["large_image"], "openidea");
    }

    #[test]
    fn focusing_an_editor_publishes() {
        let h = host();
        h.open_editor("/project/src/main.rs", "rust", "fn main() {}", &[]);
        let mut p = DiscordPresence::default();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/main.rs".into()))], &h.invocation());
        assert!(p.conn.is_some(), "{:?}", p.last_error);
        let sent = p.sent.clone().unwrap();
        assert_eq!(sent["details"], "Editing main.rs");
        assert_eq!(sent["state"], "in project");
        assert!(sent["timestamps"]["start"].as_u64().unwrap() > 0);
        assert_eq!(h.storage().get(LAST_SOCKET).map(String::as_str), Some("$XDG_RUNTIME_DIR/discord-ipc-0"));
        assert_eq!(h.status_items()["discord"].text, "Discord");
    }

    #[test]
    fn without_the_ipc_grant_it_reports_denied() {
        let h = MockHost::install().with_capabilities(&["events", "storage", "project.read"]);
        let mut p = DiscordPresence::default();
        p.handle_events(&[Event::EditorFocused(None)], &h.invocation());
        assert!(p.conn.is_none());
        assert!(p.last_error.as_deref().unwrap().contains("ipc:"), "{:?}", p.last_error);
        assert_eq!(h.status_items()["discord"].text, "Discord: –");
    }

    #[test]
    fn follows_the_window_worked_in_last() {
        let h = host();
        h.add_project("/other");
        let mut p = DiscordPresence::default();
        h.open_editor("/project/src/main.rs", "rust", "", &[]);
        p.handle_events(&[Event::EditorFocused(Some("/project/src/main.rs".into()))], &h.invocation());
        assert_eq!(p.sent.as_ref().unwrap()["state"], "in project");
        // A background fetch in another window doesn't take it over.
        h.set_project_root(Some("/other"));
        h.add_project("/project");
        h.open_editor("/other/app.py", "python", "", &[]);
        p.handle_events(&[Event::VcsChanged("/other".into())], &h.invocation());
        p.handle_events(&[Event::FileSaved("/other/app.py".into())], &h.invocation());
        assert_eq!(p.sent.as_ref().unwrap()["state"], "in project");
        // The timer, whatever window it comes from, keeps what is shown.
        p.sent = None;
        p.handle_events(&[Event::Scheduled], &h.invocation());
        assert_eq!(p.sent.as_ref().unwrap()["details"], "Editing main.rs");
        // Focusing a file there does.
        p.handle_events(&[Event::EditorFocused(Some("/other/app.py".into()))], &h.invocation());
        assert_eq!(p.sent.as_ref().unwrap()["state"], "in other");
        assert_eq!(p.sent.as_ref().unwrap()["details"], "Editing app.py");
        // A new instance (after idle reclaim) shows the same window.
        let mut woken = DiscordPresence::default();
        woken.handle_events(&[Event::Scheduled], &h.invocation());
        assert_eq!(woken.sent.as_ref().unwrap()["state"], "in other");
        // Its window closed: cleared until the next event.
        h.set_project_root(Some("/project"));
        let mut after_close = DiscordPresence::default();
        storage::set(SHOWN, &Snapshot { root: Some("/gone".into()), ..Snapshot::default() }.to_json()).unwrap();
        after_close.handle_events(&[Event::Scheduled], &h.invocation());
        assert!(after_close.sent.is_none() && after_close.conn.is_none());
        assert!(!h.storage().contains_key(SHOWN));
    }

    #[test]
    fn answers_are_matched_by_nonce() {
        let h = host();
        let mut p = DiscordPresence::default();
        p.handle_events(&[Event::EditorFocused(None)], &h.invocation());
        assert!(p.ready);
        assert_eq!(p.awaiting, None, "the mock echoes the frame, nonce included");
        assert_eq!(p.last_error, None);
        // Discord refuses one: its message is shown.
        p.awaiting = Some("oi-99".into());
        p.inbox = encode(OP_FRAME, &json!({"evt": "ERROR", "nonce": "oi-99", "data": {"message": "bad asset"}}));
        assert!(p.pump(0));
        assert_eq!(p.awaiting, None);
        assert_eq!(p.last_error.as_deref(), Some("bad asset"));
        // An answer to something else changes nothing.
        p.awaiting = Some("oi-100".into());
        p.inbox = encode(OP_FRAME, &json!({"evt": null, "nonce": "oi-1"}));
        assert!(p.pump(0));
        assert_eq!(p.awaiting.as_deref(), Some("oi-100"));
    }

    #[test]
    fn waits_for_ready_before_sending() {
        let h = host();
        let mut p = DiscordPresence::default();
        p.handle_events(&[Event::EditorFocused(None)], &h.invocation());
        // Pretend READY hasn't come: the activity waits.
        p.ready = false;
        p.sent = None;
        p.want = Some(json!({"details": "Editing a.rs"}));
        p.flush().unwrap();
        assert!(p.sent.is_none());
        assert!(p.last_error.as_deref().unwrap().contains("Waiting"));
        // READY arrives (seen on the next pump): it is sent.
        p.inbox = encode(OP_FRAME, &json!({"cmd": "DISPATCH", "evt": "READY", "data": {}}));
        p.flush().unwrap();
        assert_eq!(p.sent.as_ref().unwrap()["details"], "Editing a.rs");
    }

    #[test]
    fn idle_and_disabled_clear_it() {
        let h = host();
        let mut p = DiscordPresence::default();
        p.handle_events(&[Event::EditorFocused(None)], &h.invocation());
        assert!(p.conn.is_some());
        // Ten minutes without activity, then the scheduled check.
        let _ = storage::set(LAST_SEEN, &(now_secs() - 600).to_string());
        p.handle_events(&[Event::Scheduled], &h.invocation());
        assert!(p.conn.is_none(), "idle: cleared and disconnected");
        p.handle_events(&[Event::FileSaved("/project/a.py".into())], &h.invocation());
        assert!(p.conn.is_some(), "back on activity");
        p.run_command("toggle", &h.invocation(), None).unwrap();
        assert!(p.conn.is_none());
        assert_eq!(h.status_items()["discord"].text, "Discord: off");
        p.run_command("toggle", &h.invocation(), None).unwrap();
        assert!(p.conn.is_some(), "on again");
        h.set_setting("enabled", "false");
        p.handle_events(&[Event::Scheduled], &h.invocation());
        assert!(p.conn.is_none(), "the setting turns it off too");
    }

    #[test]
    fn privacy_mode_and_secret_files() {
        let s = Snapshot {
            file: Some("main.rs".into()),
            language: Some("rust".into()),
            project: Some("acme-secret".into()),
            branch: Some("feature/x".into()),
            repo: Some("https://github.com/acme/secret".into()),
            ..Snapshot::default()
        };
        let mut settings = Settings { state: "in {project} · {branch}".into(), repository_button: true, ..Settings::default() };
        let a = activity(&settings, &s);
        assert_eq!(a["state"], "in acme-secret · feature/x");
        assert_eq!(a["buttons"][0]["url"], "https://github.com/acme/secret");
        settings.privacy_mode = true;
        let a = activity(&settings, &s);
        assert_eq!(a["details"], "Editing a Rust file");
        assert_eq!(a["state"], "in a project");
        assert_eq!(a["assets"]["large_image"], "rust", "the language stays");
        assert_eq!(a["buttons"].as_array().unwrap().len(), 1, "no repository button");
        assert_eq!(a["buttons"][0]["label"], "Get OpenIDEA");
        assert!(!a.to_string().contains("acme"));
        // A file that may hold secrets: no name, no logo.
        settings.privacy_mode = false;
        let env = Snapshot { file: Some(".env.production".into()), language: Some("dotenv".into()), sensitive: true, ..s.clone() };
        let a = activity(&settings, &env);
        assert_eq!(a["details"], "Editing a private file");
        assert_eq!(a["assets"]["large_image"], "openidea");
        assert!(!a.to_string().contains("production"));
        settings.hide_sensitive_files = false;
        assert_eq!(activity(&settings, &env)["details"], "Editing .env.production");
        for p in ["/p/.env", "/p/.env.local", "C:\\\\p\\\\certs\\\\server.KEY", "/home/a/.ssh/config", "/p/secrets.yaml",
                  "/p/prod.tfvars", "/p/config/credentials.json", "/p/keystore.jks", "/p/app_secrets.toml"] {
            assert!(is_sensitive(p), "{p}");
        }
        for p in ["/p/.env.example", "/p/src/tokenizer.rs", "/p/README.md", "/p/src/keyboard.ts", "/p/.envoy.yaml"] {
            assert!(!is_sensitive(p), "{p}");
        }
    }

    #[test]
    fn repository_urls_lose_credentials() {
        let cases = [
            ("git@github.com:MrBoombastic/OpenIDEA-DiscordPresence.git", Some("https://github.com/MrBoombastic/OpenIDEA-DiscordPresence")),
            ("https://alice:ghp_TOKEN@github.com/acme/app.git", Some("https://github.com/acme/app")),
            ("https://oauth2:x@gitlab.example.com/group/sub/app.git/?ref=1#y", Some("https://gitlab.example.com/group/sub/app")),
            ("ssh://git@codeberg.org:22/me/tool.git", Some("https://codeberg.org/me/tool")),
            ("/srv/git/app.git", None),
            ("C:\\repos\\app", None),
            ("file:///srv/app.git", None),
            ("https://github.com/acme", None),
            ("https://github.com/a/../b", None),
            ("https://evil.com\"/x/y", None),
        ];
        for (remote, web) in cases {
            assert_eq!(remote_web_url(remote).as_deref(), web, "{remote}");
        }
    }

    #[test]
    fn buttons_are_checked_and_capped() {
        let s = Snapshot { file: Some("a.rs".into()), repo: Some("https://github.com/a/b".into()), ..Snapshot::default() };
        let mut settings = Settings::default();
        assert_eq!(buttons(&settings, &s), vec![json!({"label": "Get OpenIDEA", "url": OPENIDEA_URL})]);
        settings.repository_button = true;
        settings.custom_button_label = "My site with a really long label over 32".into();
        settings.custom_button_url = "https://example.com".into();
        let b = buttons(&settings, &s);
        assert_eq!(b.len(), 2, "Discord allows two");
        assert_eq!(b[0]["label"], "View Repository");
        assert_eq!(b[1]["label"].as_str().unwrap().chars().count(), 32);
        settings.repository_button = false;
        settings.custom_button_url = "javascript:alert(1)".into();
        assert_eq!(buttons(&settings, &s).len(), 1, "only http(s) links");
        settings.openidea_button = false;
        assert!(buttons(&settings, &s).is_empty());
        assert!(activity(&settings, &s).get("buttons").is_none());
    }

    #[test]
    fn tools_and_languages_from_file_names() {
        let tool = |p: &str| tool_of(p).map(|t| t.0);
        assert_eq!(tool("/p/package.json"), Some("npm"));
        assert_eq!(tool("C:\\\\p\\\\build.gradle.kts"), Some("gradle"));
        assert_eq!(tool("/p/pom.xml"), Some("maven"));
        assert_eq!(tool("/p/vite.config.ts"), Some("vite"));
        assert_eq!(tool("/p/tailwind.config.js"), Some("tailwind"));
        assert_eq!(tool("/p/next.config.mjs"), Some("nextjs"));
        assert_eq!(tool("/p/.github/workflows/ci.yml"), Some("githubactions"));
        assert_eq!(tool("/p/docker-compose.yml"), Some("docker"));
        assert_eq!(tool("/p/tsconfig.app.json"), Some("typescript"));
        assert_eq!(tool("/p/.eslintrc.json"), Some("eslint"));
        assert_eq!(tool("/p/Cargo.toml"), Some("rust"));
        assert_eq!(tool("/p/src/main.rs"), None);
        assert_eq!(tool("/p/app.config.ts"), None);
        assert_eq!(language_from_path("/p/x.nim").as_deref(), Some("nim"));
        assert_eq!(language_from_path("/p/Dockerfile.dev").as_deref(), Some("dockerfile"));
        assert_eq!(language_from_path("/p/CMakeLists.txt").as_deref(), Some("cmake"));
        // The editor's language wins, unless it has no logo and the name does.
        assert_eq!(pick_language(Some("rust"), Some("/p/a.rs")).as_deref(), Some("rust"));
        assert_eq!(pick_language(Some("plaintext"), Some("/p/a.nim")).as_deref(), Some("nim"));
        assert_eq!(pick_language(Some("plaintext"), Some("/p/notes.txt")).as_deref(), Some("plaintext"));
        assert_eq!(pick_language(None, Some("/p/a.cr")).as_deref(), Some("crystal"));
        // A tool's logo replaces the language's.
        let s = Snapshot { file: Some("package.json".into()), language: Some("json".into()), tool: Some(("npm".into(), "npm".into())), ..Snapshot::default() };
        let a = activity(&Settings::default(), &s);
        assert_eq!(a["assets"]["large_image"], "npm");
        assert_eq!(a["assets"]["large_text"], "npm");
        assert_eq!(Snapshot::from_json(&s.to_json()).unwrap().tool, s.tool, "kept across idle reclaim");
        // Every logo the code can send exists as an art asset.
        let assets: Vec<String> = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/assets"))
            .map(|d| d.filter_map(|e| e.ok()?.file_name().to_str()?.strip_suffix(".png").map(str::to_owned)).collect())
            .unwrap_or_default();
        if !assets.is_empty() {
            for key in language_image_keys() {
                assert!(assets.iter().any(|a| a == key), "missing asset {key}.png");
            }
            for name in ["npm", "yarn", "pnpm", "bun", "deno", "nodejs", "gradle", "maven", "poetry", "anaconda", "composer",
                         "flutter", "android", "xcode", "helm", "kubernetes", "ansible", "vagrant", "jenkins", "gitlab",
                         "travis", "nginx", "vercel", "netlify", "cloudflare", "firebase", "tauri", "angular", "nx",
                         "turborepo", "biome", "renovate", "editorconfig", "babel", "eslint", "prettier", "githubactions",
                         "dependabot", "vite", "vitest", "tailwind", "postcss", "nextjs", "nuxt", "webpack", "rollup",
                         "jest", "playwright", "cypress", "electron", "esbuild", "storybook", "dotnet"] {
                assert!(assets.iter().any(|a| a == name), "missing asset {name}.png");
            }
        }
    }

    fn language_image_keys() -> Vec<&'static str> {
        let src = include_str!("lib.rs");
        let start = src.find("pub fn language_image").unwrap();
        let body = &src[start..start + src[start..].find("\n}\n").unwrap()];
        body.split("=> \"").skip(1).filter_map(|t| t.split('"').next()).collect()
    }

    #[test]
    fn idle_modes() {
        let h = host();
        h.open_editor("/project/src/main.rs", "rust", "", &[]);
        let mut p = DiscordPresence::default();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/main.rs".into()))], &h.invocation());
        let away = now_secs() - 600;
        // "idle": an Idle activity, timed from the last activity.
        h.set_setting("idleAction", "\"idle\"");
        let _ = storage::set(LAST_SEEN, &away.to_string());
        p.handle_events(&[Event::Scheduled], &h.invocation());
        let sent = p.sent.clone().unwrap();
        assert_eq!(sent["details"], "Idle in project");
        assert_eq!(sent["timestamps"]["start"], away);
        assert!(sent.get("state").is_none());
        // "keep": what was shown stays.
        h.set_setting("idleAction", "\"keep\"");
        p.handle_events(&[Event::Scheduled], &h.invocation());
        assert_eq!(p.sent.as_ref().unwrap()["details"], "Idle in project", "nothing new to send");
        p.sent = None;
        p.handle_events(&[Event::Scheduled], &h.invocation());
        assert_eq!(p.sent.as_ref().unwrap()["details"], "Editing main.rs");
        // Back after being idle: the timer restarts when asked to.
        let _ = storage::set(SESSION_START, &(now_secs() - 3600).to_string());
        h.set_setting("resetTimerOnReturn", "true");
        p.handle_events(&[Event::EditorFocused(Some("/project/src/main.rs".into()))], &h.invocation());
        assert!(p.sent.as_ref().unwrap()["timestamps"]["start"].as_u64().unwrap() >= now_secs() - 5);
    }
}
