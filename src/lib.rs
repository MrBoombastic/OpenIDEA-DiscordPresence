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
        json!({ "root": self.root, "file": self.file, "language": self.language, "branch": self.branch }).to_string()
    }

    pub fn from_json(text: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(text).ok()?;
        let get = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        Some(Self { root: get("root"), file: get("file"), language: get("language"), project: None, branch: get("branch") }
            .with_project_name())
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

/// The Discord `activity` object (without timestamps).
pub fn activity(settings: &Settings, s: &Snapshot) -> Value {
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
    match (settings.language_images, lang.and_then(language_image)) {
        (true, Some(key)) if editing => {
            let l = lang.unwrap_or_default();
            assets.insert("large_image".into(), key.into());
            assets.insert("large_text".into(), language_name(l).into());
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
    Value::Object(a)
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

    fn snapshot(ctx: &Invocation, focused: Option<&str>) -> Snapshot {
        let editor = ctx.editor();
        let path = editor.as_ref().map(|e| e.path()).filter(|p| !p.is_empty()).or_else(|| focused.map(str::to_owned));
        let language = editor.as_ref().map(|e| e.language()).filter(|l| !l.is_empty());
        let project = ctx.project();
        let root = project.as_ref().and_then(|p| p.root().ok());
        let branch = project.as_ref().and_then(|p| p.vcs_status().ok().flatten()).and_then(|s| s.branch);
        Snapshot {
            root,
            file: path.as_deref().map(last_segment),
            language: language.or_else(|| path.as_deref().and_then(language_from_path)),
            project: None,
            branch,
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
                match (start, last) {
                    (Some(start), Some(last)) if now.saturating_sub(last) < SESSION_GAP_SECS => Some(start),
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
            Trigger::Switch => Some(Self::snapshot(ctx, focused)),
            Trigger::Touch => {
                let owner = self.shown().and_then(|s| s.root);
                (owner.is_none() || owner == ctx_root()).then(|| Self::snapshot(ctx, focused))
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
            if idle || last == 0 || closed {
                if closed {
                    self.set_shown(None);
                }
                self.clear();
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
        _ => return None,
    })
}

/// A language id from the file name, for when the editor doesn't say.
pub fn language_from_path(path: &str) -> Option<String> {
    let name = last_segment(path).to_lowercase();
    if name == "dockerfile" {
        return Some("dockerfile".into());
    }
    let ext = name.rsplit_once('.')?.1;
    let id = match ext {
        "rs" => "rust",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "groovy" | "gradle" => "groovy",
        "scala" => "scala",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "py" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "rb" => "ruby",
        "swift" => "swift",
        "dart" => "dart",
        "lua" => "lua",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "sql" => "sql",
        "md" => "markdown",
        "sh" | "bash" | "zsh" => "shellscript",
        "ps1" => "powershell",
        "vue" => "vue",
        "svelte" => "svelte",
        "sol" => "solidity",
        "zig" => "zig",
        _ => return None,
    };
    Some(id.into())
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
            root: None,
            file: Some("main.rs".into()),
            language: Some("rust".into()),
            project: Some("openidea".into()),
            branch: None,
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
        let s = Snapshot { root: None, file: Some("App.java".into()), language: Some("java".into()), project: Some("shop".into()), branch: Some("main".into()) };
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
}
