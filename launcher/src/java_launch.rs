//! The command that starts the real Minecraft: Java Edition, read from its version JSON as the
//! official launcher reads it: the main class, the classpath of this system's libraries and the
//! client jar, the JVM and game arguments with their rules and `${…}` placeholders (or the old
//! `minecraftArguments` line), an old version's natives unpacked into a folder of their own, and
//! the logging setup Mojang gives (for 1.7 to 1.18, the one that keeps Log4Shell out).
//!
//! The files are where `mojang::download` puts them: `versions/`, `libraries/` and `assets/` in
//! one game folder. Nothing here reaches the network.

use crate::mojang;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Who plays: the account's name, UUID, Minecraft access token and Xbox user id.
pub struct Player {
    pub name: String,
    pub uuid: String,
    pub token: String,
    pub xuid: String,
    /// Which service signed the token in. Mojang's game takes `--userType`; it only ever saw
    /// what Mojang's launcher gives (`msa`, or `legacy`/`mojang` for old sign-ins). An Ely.by
    /// token is neither: the Java game cannot check it with Mojang, so a Java instance must not
    /// be started with one (see `account::java_player`). Horizon Client reads its session file
    /// instead, where `account::write_session_file` says `elyby`.
    pub elyby: bool,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Player({}, <token hidden>)", self.name)
    }
}

/// Everything the command needs besides the version JSON.
pub struct Launch<'a> {
    /// Holds `versions/`, `libraries/`, `assets/`.
    pub minecraft: &'a Path,
    /// The instance's game folder (saves, options.txt, mods).
    pub game_dir: &'a Path,
    /// Where an old version's natives are unpacked.
    pub natives: &'a Path,
    /// The `java` program.
    pub java: &'a Path,
    pub memory_mb: u32,
    /// The player's own JVM arguments.
    pub jvm_args: &'a [String],
    pub player: &'a Player,
    /// `osx`, `linux` or `windows`, as version files name systems.
    pub os: &'a str,
    /// The window's size (`--width`, `--height`); none: the game's own 854×480.
    pub window: Option<(u32, u32)>,
    /// A server to join once the game has started: `host` or `host:port`.
    pub server: Option<&'a str>,
}

impl Launch<'_> {
    /// The optional features of a version's argument rules this launch turns on
    /// (<https://minecraft.wiki/w/Client.json>).
    fn features(&self) -> Vec<&'static str> {
        let mut on = Vec::new();
        if self.window.is_some() {
            on.push("has_custom_resolution");
        }
        if self.server.is_some() {
            on.push("is_quick_play_multiplayer");
        }
        on
    }
}

/// `host:port` as the game's arguments before 1.20 take it: `--server host --port port`
/// (25565 when none is given; an IPv6 address keeps its own colons).
fn host_and_port(address: &str) -> (&str, u16) {
    match address.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') || host.ends_with(']') => match port.parse() {
            Ok(port) => (host.trim_matches(['[', ']']), port),
            Err(_) => (address, 25565),
        },
        _ => (address, 25565),
    }
}

/// Whether one of `args`' rules asks for `feature`.
fn mentions(args: &Value, feature: &str) -> bool {
    args.as_array().into_iter().flatten().any(|a| {
        a["rules"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r["features"].get(feature).is_some())
    })
}

/// This system's name in version files.
pub fn os_name() -> &'static str {
    match crate::sources::Os::current() {
        crate::sources::Os::Linux => "linux",
        crate::sources::Os::MacOs => "osx",
        crate::sources::Os::Windows => "windows",
    }
}

/// Whether a rule list lets this system in: the last matching rule decides; no rules allow.
/// The optional features (demo, custom resolution, quick play) are on only when named in `on`.
fn allowed(rules: &Value, os: &str, on: &[&str]) -> bool {
    let Some(rules) = rules.as_array() else {
        return true;
    };
    let arch_matches = |arch: &str| match arch {
        "x86" => cfg!(target_arch = "x86"),
        "arm64" | "aarch64" => cfg!(target_arch = "aarch64"),
        "x86_64" | "amd64" => cfg!(target_arch = "x86_64"),
        _ => false,
    };
    let mut allow = false;
    for rule in rules {
        let system = &rule["os"];
        let matches = system["name"].as_str().is_none_or(|n| n == os)
            && system["arch"].as_str().is_none_or(arch_matches)
            && rule["features"].as_object().is_none_or(|f| {
                f.iter()
                    .all(|(name, v)| v.as_bool() == Some(on.contains(&name.as_str())))
            });
        if matches {
            allow = rule["action"] == "allow";
        }
    }
    allow
}

/// A version file.
pub fn read(path: &Path) -> Result<Value, String> {
    fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A library's name without its version: `group:artifact[:classifier]`.
fn library_key(library: &Value) -> String {
    let mut parts = library["name"].as_str().unwrap_or_default().split(':');
    let (group, artifact) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    match parts.nth(1) {
        Some(classifier) => format!("{group}:{artifact}:{classifier}"),
        None => format!("{group}:{artifact}"),
    }
}

/// A version with what it `inheritsFrom` (a loader's profile on the game's version) merged in, as
/// the official launcher reads it: the child's fields over the parent's, its libraries first (a
/// parent's library it names again is left out), its arguments after the parent's, and the
/// parent's client jar (`jar`).
pub fn resolve(version: Value, minecraft: &Path) -> Result<Value, String> {
    resolve_from(version, minecraft, &mut Vec::new())
}

/// `resolve`, with the ids already on the way down: a version met twice is a loop.
fn resolve_from(version: Value, minecraft: &Path, seen: &mut Vec<String>) -> Result<Value, String> {
    let Some(parent) = version["inheritsFrom"].as_str() else {
        return Ok(version);
    };
    let id = version["id"].as_str().unwrap_or_default();
    seen.push(id.to_owned());
    if seen.iter().any(|s| s == parent) {
        return Err(format!("{id}: inheritsFrom loops"));
    }
    let file = minecraft
        .join("versions")
        .join(mojang::relative(parent)?)
        .join(format!("{parent}.json"));
    let mut merged = resolve_from(read(&file)?, minecraft, seen)?;
    let jar = merged["jar"]
        .as_str()
        .or(merged["id"].as_str())
        .unwrap_or(parent)
        .to_string();
    let own: Vec<Value> = version["libraries"].as_array().cloned().unwrap_or_default();
    let named: std::collections::HashSet<String> = own.iter().map(library_key).collect();
    let mut libraries = own;
    libraries.extend(
        merged["libraries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|l| !named.contains(&library_key(l)))
            .cloned(),
    );
    for side in ["game", "jvm"] {
        let mut list = merged["arguments"][side]
            .as_array()
            .cloned()
            .unwrap_or_default();
        list.extend(
            version["arguments"][side]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
        if !list.is_empty() {
            merged["arguments"][side] = Value::Array(list);
        }
    }
    for (key, value) in version.as_object().into_iter().flatten() {
        if !matches!(key.as_str(), "libraries" | "arguments" | "inheritsFrom") {
            merged[key] = value.clone();
        }
    }
    merged["libraries"] = Value::Array(libraries);
    merged["jar"] = jar.into();
    Ok(merged)
}

/// The libraries this system uses, then the client jar (a loader's: the game's, `jar`).
pub fn classpath(version: &Value, minecraft: &Path, os: &str) -> Result<Vec<PathBuf>, String> {
    let id = version["id"].as_str().ok_or("The version file has no id")?;
    let jar = version["jar"].as_str().unwrap_or(id);
    let mut paths: Vec<PathBuf> = Vec::new();
    for library in version["libraries"].as_array().into_iter().flatten() {
        if !allowed(&library["rules"], os, &[]) {
            continue;
        }
        if let Some(path) = library["downloads"]["artifact"]["path"].as_str() {
            let path = minecraft.join("libraries").join(mojang::relative(path)?);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths.push(
        minecraft
            .join("versions")
            .join(mojang::relative(jar)?)
            .join(format!("{jar}.jar")),
    );
    Ok(paths)
}

fn native_path_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{}: refusing a linked or non-directory extraction parent",
            path.display()
        ),
    )
}

fn metadata_is_reparse_point(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(any(unix, windows)))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, information.as_mut_ptr()) };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    let information = unsafe { information.assume_init() };
    Ok((
        information.dwVolumeSerialNumber,
        information.nFileIndexHigh,
        information.nFileIndexLow,
    ))
}

fn same_native_file(path: &Path, file: &File) -> io::Result<bool> {
    let path_metadata = fs::symlink_metadata(path)?;
    let opened = file.metadata()?;
    if !path_metadata.is_file()
        || metadata_is_reparse_point(&path_metadata)
        || !opened.is_file()
        || metadata_is_reparse_point(&opened)
    {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(path_metadata.dev() == opened.dev() && path_metadata.ino() == opened.ino())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options
            .write(true)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        let current = options.open(path)?;
        let current_metadata = current.metadata()?;
        if !current_metadata.is_file() || metadata_is_reparse_point(&current_metadata) {
            return Ok(false);
        }
        Ok(windows_file_identity(&current)? == windows_file_identity(file)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(path_metadata.len() == opened.len()
            && path_metadata.modified().ok() == opened.modified().ok())
    }
}

fn ensure_native_root(root: &Path) -> io::Result<()> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata_is_reparse_point(&metadata) => Ok(()),
        Ok(_) => Err(native_path_error(root)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root)?;
            let metadata = fs::symlink_metadata(root)?;
            if metadata.is_dir() && !metadata_is_reparse_point(&metadata) {
                Ok(())
            } else {
                Err(native_path_error(root))
            }
        }
        Err(error) => Err(error),
    }
}

fn ensure_native_parents(root: &Path, relative: &Path) -> io::Result<()> {
    let mut current = root.to_path_buf();
    for component in relative
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .components()
    {
        let Component::Normal(name) = component else {
            return Err(native_path_error(&current));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata_is_reparse_point(&metadata) => {
                continue;
            }
            Ok(_) => return Err(native_path_error(&current)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(create_error) if create_error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(create_error) => return Err(create_error),
                }
                let metadata = fs::symlink_metadata(&current)?;
                if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
                    return Err(native_path_error(&current));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn open_native_output(path: &Path) -> io::Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
                return Err(native_path_error(path));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || metadata_is_reparse_point(&opened) || !same_native_file(path, &file)? {
        return Err(native_path_error(path));
    }
    file.set_len(0)?;
    Ok(file)
}

fn write_native_entry_with(
    root: &Path,
    relative: &Path,
    entry: &mut impl Read,
    after_open: impl FnOnce(),
) -> io::Result<()> {
    ensure_native_parents(root, relative)?;
    let out = root.join(relative);
    let mut file = open_native_output(&out)?;
    // Recheck parents and the leaf after opening. Once this succeeds, the retained
    // handle is the only destination used by the write, even if the path is swapped.
    ensure_native_parents(root, relative)?;
    if !same_native_file(&out, &file)? {
        return Err(native_path_error(&out));
    }
    after_open();
    io::copy(entry, &mut file)?;
    Ok(())
}

fn write_native_entry(root: &Path, relative: &Path, entry: &mut impl Read) -> io::Result<()> {
    write_native_entry_with(root, relative, entry, || {})
}

/// Unpacks this system's natives of an old version (a library's classifier) into `to`, without
/// what the library's `extract.exclude` names (META-INF/). Versions since 1.19 have none.
pub fn unpack_natives(
    version: &Value,
    minecraft: &Path,
    os: &str,
    to: &Path,
) -> Result<(), String> {
    ensure_native_root(to).map_err(|error| format!("{}: {error}", to.display()))?;
    for library in version["libraries"].as_array().into_iter().flatten() {
        if !allowed(&library["rules"], os, &[]) {
            continue;
        }
        let Some(key) = mojang::native_classifier(library, os) else {
            continue;
        };
        let Some(path) = library["downloads"]["classifiers"][key.as_str()]["path"].as_str() else {
            continue;
        };
        let jar = minecraft.join("libraries").join(mojang::relative(path)?);
        let exclude: Vec<&str> = library["extract"]["exclude"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let file = fs::File::open(&jar).map_err(|e| format!("{}: {e}", jar.display()))?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("{}: {e}", jar.display()))?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            let Some(name) = entry.enclosed_name() else {
                continue;
            };
            let text = name.to_string_lossy().replace('\\', "/");
            if entry.is_dir() || exclude.iter().any(|x| text.starts_with(x)) {
                continue;
            }
            let out = to.join(&name);
            write_native_entry(to, &name, &mut entry)
                .map_err(|error| format!("{}: {error}", out.display()))?;
        }
    }
    Ok(())
}

/// Where an old version reads its assets by name (`virtual` indexes, `map_to_resources`), with
/// the objects copied there once; the asset store itself for every other version.
pub fn game_assets(version: &Value, minecraft: &Path, game_dir: &Path) -> Result<PathBuf, String> {
    let id = version["assetIndex"]["id"].as_str().unwrap_or("legacy");
    let assets = minecraft.join("assets");
    let index = assets
        .join("indexes")
        .join(mojang::relative(&format!("{id}.json"))?);
    let Ok(text) = fs::read_to_string(&index) else {
        return Ok(assets);
    };
    let index: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", index.display()))?;
    let to = if index["map_to_resources"] == true {
        game_dir.join("resources")
    } else if index["virtual"] == true {
        assets.join("virtual").join(mojang::relative(id)?)
    } else {
        return Ok(assets);
    };
    for (name, object) in index["objects"].as_object().into_iter().flatten() {
        let Some(hash) = object["hash"].as_str().filter(|h| h.len() == 40) else {
            continue;
        };
        let out = to.join(mojang::relative(name)?);
        if !out.is_file() {
            let from = assets.join("objects").join(&hash[..2]).join(hash);
            fs::create_dir_all(out.parent().unwrap())
                .and_then(|_| fs::copy(&from, &out))
                .map_err(|e| format!("{}: {e}", from.display()))?;
        }
    }
    Ok(to)
}

/// The arguments of one list (`arguments.jvm` or `arguments.game`), their rules applied.
fn list(args: &Value, os: &str, on: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for arg in args.as_array().into_iter().flatten() {
        match arg {
            Value::String(s) => out.push(s.clone()),
            Value::Object(o) if allowed(&o["rules"], os, on) => match &o["value"] {
                Value::String(s) => out.push(s.clone()),
                Value::Array(values) => {
                    out.extend(values.iter().filter_map(Value::as_str).map(String::from))
                }
                _ => {}
            },
            _ => {}
        }
    }
    out
}

/// `${name}` replaced by its value; an unknown one stays as written.
fn fill(arg: &str, vars: &HashMap<&str, String>) -> String {
    let mut out = String::new();
    let mut rest = arg;
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        out.push_str(&rest[..start]);
        let key = &rest[start + 2..start + len];
        match vars.get(key) {
            Some(value) => out.push_str(value),
            None => out.push_str(&rest[start..start + len + 1]),
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

/// The command that starts `version` (its JSON) as `launch` says, run in the game folder.
/// Unpacks natives and lays out old assets first.
pub fn command(version: &Value, launch: &Launch) -> Result<Command, String> {
    let id = version["id"].as_str().ok_or("The version file has no id")?;
    let main = version["mainClass"]
        .as_str()
        .ok_or("The version file has no mainClass")?;
    let separator = if cfg!(windows) { ";" } else { ":" };
    let classpath = classpath(version, launch.minecraft, launch.os)?;
    unpack_natives(version, launch.minecraft, launch.os, launch.natives)?;
    let assets = launch.minecraft.join("assets");
    let game_assets = game_assets(version, launch.minecraft, launch.game_dir)?;
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let p = launch.player;
    // `${user_type}`: never pretend an Ely.by sign-in is a Microsoft one (E1.0.1). Mojang's game
    // only ever saw what Mojang's launcher gives (`msa`). `account::java_player` refuses to hand
    // an Ely.by token to a Java instance at all, so this branch is defense in depth: should one
    // ever reach here, it is not labelled as a Microsoft purchase.
    let user_type = if p.elyby { "elyby" } else { "msa" };
    let vars: HashMap<&str, String> = HashMap::from([
        ("auth_player_name", p.name.clone()),
        ("auth_uuid", p.uuid.clone()),
        ("auth_access_token", p.token.clone()),
        ("auth_session", format!("token:{}:{}", p.token, p.uuid)),
        ("auth_xuid", p.xuid.clone()),
        ("clientid", String::new()),
        ("user_type", user_type.into()),
        ("user_properties", "{}".into()),
        ("version_name", id.to_string()),
        (
            "version_type",
            version["type"].as_str().unwrap_or("release").into(),
        ),
        ("game_directory", path(launch.game_dir)),
        ("assets_root", path(&assets)),
        ("game_assets", path(&game_assets)),
        (
            "assets_index_name",
            version["assetIndex"]["id"]
                .as_str()
                .unwrap_or("legacy")
                .into(),
        ),
        ("natives_directory", path(launch.natives)),
        (
            "library_directory",
            path(&launch.minecraft.join("libraries")),
        ),
        ("classpath_separator", separator.into()),
        (
            "classpath",
            classpath
                .iter()
                .map(|c| path(c))
                .collect::<Vec<_>>()
                .join(separator),
        ),
        ("launcher_name", "horizon-launcher".into()),
        (
            "resolution_width",
            launch.window.map(|w| w.0.to_string()).unwrap_or_default(),
        ),
        (
            "resolution_height",
            launch.window.map(|w| w.1.to_string()).unwrap_or_default(),
        ),
        (
            "quickPlayMultiplayer",
            launch.server.unwrap_or_default().to_string(),
        ),
        ("launcher_version", env!("CARGO_PKG_VERSION").into()),
    ]);
    let mut jvm = vec![
        format!("-Xmx{}M", launch.memory_mb),
        // Whatever the logging setup, never look names up in log messages.
        "-Dlog4j2.formatMsgNoLookups=true".to_string(),
    ];
    let logging = &version["logging"]["client"];
    if let (Some(arg), Some(file)) = (logging["argument"].as_str(), logging["file"]["id"].as_str())
    {
        let config = launch
            .minecraft
            .join("assets")
            .join("log_configs")
            .join(mojang::relative(file)?);
        jvm.push(arg.replace("${path}", &path(&config)));
    }
    let arguments = &version["arguments"];
    let (mut jvm_list, mut game) = (Vec::new(), Vec::new());
    // Before 1.13: one line of game arguments, and the JVM's natives and classpath. A loader's
    // profile on such a version adds its own lists after them.
    if let Some(line) = version["minecraftArguments"].as_str() {
        jvm_list.extend(
            [
                "-Djava.library.path=${natives_directory}",
                "-cp",
                "${classpath}",
            ]
            .map(String::from),
        );
        game.extend(line.split_whitespace().map(String::from));
    }
    let on = launch.features();
    jvm_list.extend(list(&arguments["jvm"], launch.os, &on));
    game.extend(list(&arguments["game"], launch.os, &on));
    // Versions whose rules don't ask for them still take the arguments (since 1.6.1): the
    // window's size, and before 1.20 (23w14a) a server as `--server` and `--port`.
    // <https://minecraft.wiki/w/Java_Edition_client_command_line_arguments>
    if let Some((width, height)) = launch
        .window
        .filter(|_| !mentions(&arguments["game"], "has_custom_resolution"))
    {
        game.extend(["--width".into(), width.to_string()]);
        game.extend(["--height".into(), height.to_string()]);
    }
    if let Some(server) = launch
        .server
        .filter(|_| !mentions(&arguments["game"], "is_quick_play_multiplayer"))
    {
        let (host, port) = host_and_port(server);
        game.extend(["--server".into(), host.to_string()]);
        game.extend(["--port".into(), port.to_string()]);
    }
    jvm.extend(jvm_list.iter().map(|a| fill(a, &vars)));
    jvm.extend(launch.jvm_args.iter().cloned());
    let mut cmd = Command::new(launch.java);
    crate::process::scrub_secrets(&mut cmd);
    cmd.current_dir(launch.game_dir)
        .args(&jvm)
        .arg(main)
        .args(game.iter().map(|a| fill(a, &vars)));
    Ok(cmd)
}

#[cfg(test)]
#[path = "java_launch/tests.rs"]
mod tests;
