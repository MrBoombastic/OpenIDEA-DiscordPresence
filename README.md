# Discord Presence for OpenIDEA

Shows what you are working on in OpenIDEA as your Discord activity: the file, its language, the project and (optionally) the branch, with the time elapsed.

```
Playing OpenIDEA
Editing main.rs
in openidea
00:42 elapsed
```

It talks to the Discord desktop app on the same computer through Discord's local socket (Rich Presence over IPC). Nothing goes over the network from the plugin, and the browser version of Discord can't be reached.

## Features

- **Nothing to install** besides the plugin: no companion program or daemon, and no permission to run programs. It only opens Discord's own `discord-ipc-N` socket, on Windows, macOS and Linux (including Flatpak and Snap Discord). Works with Discord, Canary, PTB and Vesktop.
- **Language and tool logos**: 153 logos. Besides over 100 languages it recognises project files: `package.json` shows npm, `build.gradle.kts` Gradle, `pom.xml` Maven, `vite.config.ts` Vite, `.github/workflows/*.yml` GitHub Actions, `docker-compose.yml`, `Cargo.toml`, `tsconfig.json`, Tailwind, Next.js, ESLint, Prettier, Jest, Playwright and about 50 more.
- **Privacy mode**: only the language is shown ("Editing a Rust file in a project").
- **Files that may hold secrets** (`.env`, keys, certificates, keystores, Terraform state, `secrets.*`, anything in `.ssh` or `.aws` …) are shown as "a private file", without their name or logo. On by default.
- **Buttons**: "View Repository" (from the git remote, as an https link with user names and tokens removed; off by default), one custom button, and "Get OpenIDEA".
- **When idle**: hide the activity, show "Idle in …" with how long you have been away, or keep the last file. Optionally restart the timer when you come back.
- **Several windows**: one presence that follows the window you're working in.
- **Reliable with slow clients**: waits until Discord has accepted the application, confirms every update and shows problems in the status bar.
- **No tracking**: the plugin logs nothing about your files or projects, only content-free warnings when Discord can't be reached.

## Settings

Settings › Plugins › Discord Presence:

| Setting | Default | |
| --- | --- | --- |
| Show my activity on Discord | on | Also toggled by clicking the **Discord** status bar item or **Discord Presence: Turn On/Off**. |
| First / second line while editing | `Editing {file}` / `in {project}` | Placeholders `{file}`, `{language}`, `{project}`, `{branch}`. Remove a placeholder to keep it private. |
| First / second line with no file open | `Browsing {project}` / empty | |
| Privacy mode | off | No file, project or branch names, and no repository button. |
| Hide files that may hold secrets | on | `.env*` (not `.env.example`), `*.pem`, `*.key`, `*.p12`, `*.jks`, `*.tfstate`, `*.tfvars`, `id_rsa`, `.npmrc`, `.netrc`, `secrets.*`, `credentials.*`, files in `.ssh`, `.aws`, `.gnupg`, `.kube` … |
| Elapsed time | `session` | `session` (reset after 30 minutes away), `file`, or `none`. |
| Idle after minutes without editor activity | 5 | Focusing an editor, opening or saving a file, or a VCS change counts as activity. `0`: never. |
| When idle | `clear` | `clear` hides the activity, `idle` shows the idle line (default `Idle in {project}`) with the time since your last activity, `keep` keeps the last file. |
| Restart the timer after being idle | off | Off: the session timer continues after breaks shorter than 30 minutes. |
| Language logo | on | The language's or tool's logo, with the OpenIDEA logo in the corner. Off, or with no logo: the OpenIDEA logo. |
| "View Repository" button | off | The project's `origin` remote as `https://host/owner/repo`. Discord shows buttons to others, not to you. |
| Custom button label / link | empty | A button of your own (`https://` links only). Discord shows at most two buttons. |
| "Get OpenIDEA" button | on | Links to openidea.sh. |

**Discord Presence: Reconnect** connects again right away (otherwise a failed connection is retried every 30 seconds, for example when Discord starts after the IDE).

## Permissions

| Permission | Why |
| --- | --- |
| `ipc:\\.\pipe\discord-ipc-*` (Windows), `ipc:$XDG_RUNTIME_DIR/…`, `ipc:$TMPDIR/…`, `ipc:/tmp/…` (Linux incl. Flatpak and Snap, macOS) | The Discord app's local socket. Only paths named `discord-ipc-N`. |
| `events` | Knowing which file is focused. |
| `project.read` | The project's folder name. |
| `storage` | Session start time, the socket that worked last, on/off. |
| `vcs.read` (optional) | `{branch}` and, only when the "View Repository" button is on, the `origin` remote. |

## Limits

- With several OpenIDEA windows open, the presence follows the window you worked in last (focused or opened a file there). Saves and VCS changes in the other windows, such as a background fetch, don't take it over; closing that window clears the presence until the next editor event.
- The IDE stops a plugin that has had nothing to do for 5 minutes; the connection closes and Discord removes the activity. It comes back within a minute (the plugin checks every minute) or with the next editor event.
- A remote project's plugin runs on the project's server, so the presence works only when Discord runs on that machine.

## Requirements

OpenIDEA with plugin API 1.4 (0.1.40 or later) and the Discord desktop app.

## Building

```sh
cargo test
openidea-plugin build          # dist/plugin.wasm + check
openidea-plugin link           # try it in the IDE
openidea-plugin pack . --key <your.key> --publisher mrboombastic
```

## Discord art assets

`assets/` holds the images uploaded to the OpenIDEA Discord application (Developer Portal › Rich Presence › Art Assets), named by their keys. Language and tool logos are from [Devicon](https://github.com/devicons/devicon) (MIT) and [Simple Icons](https://github.com/simple-icons/simple-icons) (CC0); `sql.png` is drawn for this plugin. The logos are trademarks of their owners.

## License

MIT
