//! Backups and restore (docs/server.md, section 7). A backup is one zip in `backups/`: the world folder and
//! the server's settings and lists. On a running server the game first saves everything and stops writing
//! (`save-off`, `save-all`), and writes again once the zip is done (`save-on`), so players stay online.
//! A restore puts the backup's world in place of the current one, which is moved aside, never deleted.

use crate::config::Config;
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

pub const DIR: &str = "backups";
/// The settings and lists a backup holds beside the world.
const FILES: [&str; 6] = [
    "server.toml",
    "server.properties",
    "ops.json",
    "whitelist.json",
    "banned-players.json",
    "banned-ips.json",
];
/// Automatic backups are named `<time>-auto.zip`; only those are pruned.
const AUTO: &str = "auto";

/// `2026-10-16_18-30-05`, in UTC: sorts by time, and is valid in a file name everywhere.
fn stamp(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (y, m, d) = horizon_server_files::java_properties::civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}_{:02}-{:02}-{:02}",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// A backup's file name: the time, and a name the owner gave (letters, digits, `-` and `_` only).
pub fn file_name(time: SystemTime, name: Option<&str>) -> Result<String, String> {
    let name = name.unwrap_or("manual");
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "backup names use letters, digits, - and _ only (got {name:?})"
        ));
    }
    Ok(format!("{}-{name}.zip", stamp(time)))
}

/// Adds a folder to the zip, under `prefix`, skipping the game's session lock.
fn add_dir(zip: &mut ZipWriter<BufWriter<File>>, dir: &Path, prefix: &str) -> Result<(), String> {
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .large_file(true);
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let inner = format!("{prefix}/{name}");
        if path.is_dir() {
            add_dir(zip, &path, &inner)?;
        } else if name != "session.lock" {
            zip.start_file(inner.as_str(), options)
                .map_err(|e| e.to_string())?;
            let mut file = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            std::io::copy(&mut file, zip).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    Ok(())
}

/// Zips the world and the settings into `backups/<file_name>`. The zip is written under a temporary name
/// and renamed when complete, so a backup that looks finished is one.
pub fn create(dir: &Path, config: &Config, file_name: &str) -> Result<PathBuf, String> {
    let backups = dir.join(DIR);
    std::fs::create_dir_all(&backups).map_err(|e| format!("{}: {e}", backups.display()))?;
    let path = backups.join(file_name);
    let part = backups.join(format!("{file_name}.part"));
    let result = (|| {
        let file = File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        let mut zip = ZipWriter::new(BufWriter::new(file));
        let world = dir.join(&config.world.name);
        if world.is_dir() {
            add_dir(&mut zip, &world, &config.world.name)?;
        }
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for name in FILES {
            if let Ok(bytes) = std::fs::read(dir.join(name)) {
                zip.start_file(name, options).map_err(|e| e.to_string())?;
                zip.write_all(&bytes).map_err(|e| e.to_string())?;
            }
        }
        zip.finish()
            .map_err(|e| e.to_string())?
            .into_inner()
            .map_err(|e| e.to_string())?
            .sync_all()
            .map_err(|e| e.to_string())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
            Ok(path)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

pub struct Backup {
    pub name: String,
    pub bytes: u64,
}

/// The finished backups, newest first.
pub fn list(dir: &Path) -> Vec<Backup> {
    let mut out: Vec<Backup> = std::fs::read_dir(dir.join(DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".zip").then(|| Backup {
                bytes: e.metadata().map_or(0, |m| m.len()),
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

/// Deletes the oldest automatic backups beyond `keep`. Backups made by hand are never deleted.
pub fn prune(dir: &Path, keep: usize) -> Vec<String> {
    let auto_suffix = format!("-{AUTO}.zip");
    list(dir)
        .into_iter()
        .filter(|b| b.name.ends_with(&auto_suffix))
        .skip(keep)
        .filter(|b| std::fs::remove_file(dir.join(DIR).join(&b.name)).is_ok())
        .map(|b| b.name)
        .collect()
}

pub fn auto_name(time: SystemTime) -> String {
    file_name(time, Some(AUTO)).expect("the automatic name is valid")
}

/// The backup a name points at: a file in `backups/`, by its full name or the start of it.
pub fn find(dir: &Path, name: &str) -> Result<String, String> {
    let all = list(dir);
    if let Some(b) = all
        .iter()
        .find(|b| b.name == name || b.name == format!("{name}.zip"))
    {
        return Ok(b.name.clone());
    }
    let matches: Vec<&Backup> = all.iter().filter(|b| b.name.starts_with(name)).collect();
    match matches.as_slice() {
        [one] => return Ok(one.name.clone()),
        [] => {}
        _ => {
            return Err(format!(
                "{name:?} matches {} backups: be more precise",
                matches.len()
            ));
        }
    }
    // The name given to `backup NAME` (or `auto`, `manual`): the newest backup with it.
    let label = format!("-{name}.zip");
    all.iter()
        .find(|b| b.name.ends_with(&label))
        .map(|b| b.name.clone())
        .ok_or_else(|| {
            format!("no backup {name:?} in {DIR}/ (`horizon-server backups` lists them)")
        })
}

fn put_restored_path_in_place(
    restored: &Path,
    current: &Path,
    moved: Option<&Path>,
    description: &str,
) -> Result<(), String> {
    if let Err(error) = std::fs::rename(restored, current) {
        if let Some(previous) = moved {
            std::fs::rename(previous, current).map_err(|rollback| {
                format!(
                    "{description}: {error}; rollback failed: {rollback}; previous world remains at {}",
                    previous.display()
                )
            })?;
        }
        return Err(format!("{description}: {error}"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ArchiveLimits {
    max_archive_bytes: u64,
    max_entries: usize,
    max_entry_bytes: u64,
    max_total_bytes: u64,
}

// A normal Minecraft world is far below these ceilings. The caps leave room for large servers while
// preventing an untrusted backup from expanding without bound.
const BACKUP_LIMITS: ArchiveLimits = ArchiveLimits {
    max_archive_bytes: 8 * 1024 * 1024 * 1024,
    max_entries: 100_000,
    max_entry_bytes: 8 * 1024 * 1024 * 1024,
    max_total_bytes: 32 * 1024 * 1024 * 1024,
};

struct ArchiveEntry {
    path: PathBuf,
    is_dir: bool,
    size: u64,
}

/// Returns a path and a portable, case-insensitive collision key. ZIP names must use forward slashes.
fn validate_archive_path(name: &str, is_dir: bool) -> Result<(PathBuf, String), String> {
    if name.is_empty()
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains(':')
        || name.chars().any(|c| c.is_control())
    {
        return Err(format!("unsafe ZIP path {name:?}"));
    }
    let raw = if is_dir {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    if raw.is_empty() || raw.ends_with('/') {
        return Err(format!("unsafe ZIP path {name:?}"));
    }

    let mut path = PathBuf::new();
    let mut key_parts = Vec::new();
    for component in raw.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.ends_with('.')
            || component.ends_with(' ')
            || component.encode_utf16().count() > 255
            || component.chars().any(|c| {
                c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*')
            })
        {
            return Err(format!(
                "unsafe ZIP path component {component:?} in {name:?}"
            ));
        }

        let base = component
            .split('.')
            .next()
            .unwrap_or(component)
            .trim_end_matches(|c| c == ' ' || c == '.')
            .to_ascii_uppercase();
        let reserved = matches!(
            base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            base.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
        if reserved {
            return Err(format!("reserved Windows device name in ZIP path {name:?}"));
        }

        path.push(component);
        key_parts.push(component.to_lowercase());
    }

    let key = key_parts.join("/");
    if key.encode_utf16().count() > 1024 {
        return Err(format!("ZIP path is too long: {name:?}"));
    }
    Ok((path, key))
}

/// Validates the complete central directory before any archive member is written.
fn inspect_archive(
    zip: &mut ZipArchive<File>,
    archive_bytes: u64,
    limits: ArchiveLimits,
) -> Result<Vec<ArchiveEntry>, String> {
    if archive_bytes > limits.max_archive_bytes {
        return Err(format!(
            "backup is too large: {archive_bytes} bytes (limit {})",
            limits.max_archive_bytes
        ));
    }
    if zip.len() > limits.max_entries {
        return Err(format!(
            "backup has too many entries: {} (limit {})",
            zip.len(),
            limits.max_entries
        ));
    }

    let mut entries = Vec::with_capacity(zip.len());
    let mut kinds = HashMap::<String, bool>::new();
    let mut explicit = HashSet::<String>::new();
    let mut total_bytes = 0u64;

    for index in 0..zip.len() {
        let entry = zip
            .by_index(index)
            .map_err(|e| format!("reading backup entry {index}: {e}"))?;
        if entry.encrypted() {
            return Err(format!("backup entry {:?} is encrypted", entry.name()));
        }
        let is_dir = entry.is_dir();
        if !is_dir && !entry.is_file() {
            return Err(format!(
                "backup entry {:?} is a symlink or special file",
                entry.name()
            ));
        }
        if entry.enclosed_name().is_none() {
            return Err(format!("ZIP entry escapes the backup: {:?}", entry.name()));
        }
        let (path, key) = validate_archive_path(entry.name(), is_dir)?;
        let size = entry.size();
        let compressed = entry.compressed_size();
        if is_dir && size != 0 {
            return Err(format!("directory entry {:?} contains data", entry.name()));
        }
        if size > limits.max_entry_bytes {
            return Err(format!(
                "backup entry {:?} is too large: {size} bytes (limit {})",
                entry.name(),
                limits.max_entry_bytes
            ));
        }
        if size > 0 && compressed == 0 {
            return Err(format!(
                "backup entry {:?} has an invalid compressed size",
                entry.name()
            ));
        }
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or_else(|| "backup expanded size overflows".to_owned())?;
        if total_bytes > limits.max_total_bytes {
            return Err(format!(
                "backup expands beyond the limit: {total_bytes} bytes (limit {})",
                limits.max_total_bytes
            ));
        }

        let components: Vec<&str> = key.split('/').collect();
        for end in 1..components.len() {
            let parent = components[..end].join("/");
            if kinds.get(&parent) == Some(&false) {
                return Err(format!(
                    "ZIP path has a file as a parent: {:?}",
                    entry.name()
                ));
            }
            kinds.entry(parent).or_insert(true);
        }
        if !explicit.insert(key.clone()) {
            return Err(format!(
                "duplicate or case-colliding ZIP path {:?}",
                entry.name()
            ));
        }
        match kinds.get(&key) {
            Some(existing_dir) if *existing_dir && is_dir => {}
            Some(_) => {
                return Err(format!(
                    "file/directory ZIP path collision at {:?}",
                    entry.name()
                ));
            }
            None => {
                kinds.insert(key, is_dir);
            }
        }
        entries.push(ArchiveEntry { path, is_dir, size });
    }
    Ok(entries)
}

/// Extracts only after the whole archive has passed the same path/type/size policy.
fn extract_backup_archive(
    zip_path: &Path,
    scratch: &Path,
    only_prefix: Option<&Path>,
    limits: ArchiveLimits,
) -> Result<usize, String> {
    let file = File::open(zip_path).map_err(|e| format!("{}: {e}", zip_path.display()))?;
    let archive_bytes = file
        .metadata()
        .map_err(|e| format!("{}: {e}", zip_path.display()))?
        .len();
    let mut zip = ZipArchive::new(file).map_err(|e| format!("{}: {e}", zip_path.display()))?;
    let entries = inspect_archive(&mut zip, archive_bytes, limits)?;
    let mut files = 0usize;

    for (index, info) in entries.iter().enumerate() {
        if only_prefix.is_some_and(|prefix| !info.path.starts_with(prefix)) {
            continue;
        }
        let out = scratch.join(&info.path);
        if info.is_dir {
            std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let entry = zip
            .by_index(index)
            .map_err(|e| format!("reading backup entry {index}: {e}"))?;
        let mut dest = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out)
            .map_err(|e| format!("{}: {e}", out.display()))?;
        let mut bounded = entry.take(info.size.saturating_add(1));
        let copied = std::io::copy(&mut bounded, &mut dest)
            .map_err(|e| format!("extracting {}: {e}", out.display()))?;
        if copied != info.size {
            return Err(format!(
                "backup entry {} expanded to {copied} bytes; expected {}",
                out.display(),
                info.size
            ));
        }
        dest.sync_all()
            .map_err(|e| format!("writing {}: {e}", out.display()))?;
        files += 1;
    }
    Ok(files)
}

#[cfg(unix)]
fn create_restore_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)
}

#[cfg(not(unix))]
fn create_restore_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

/// Uses a unique staging directory directly under the trusted server root, never a fixed path under .horizon.
fn create_restore_scratch(dir: &Path) -> Result<PathBuf, String> {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for attempt in 0..32 {
        let scratch = dir.join(format!(
            ".horizon-restore-{}-{nonce:x}-{attempt}",
            std::process::id()
        ));
        match create_restore_dir(&scratch) {
            Ok(()) => return Ok(scratch),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("{}: {error}", scratch.display())),
        }
    }
    Err("could not allocate a unique backup restore directory".into())
}

fn with_restore_scratch<T>(
    dir: &Path,
    operation: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    let scratch = create_restore_scratch(dir)?;
    let result = operation(&scratch);
    let cleanup = std::fs::remove_dir_all(&scratch);
    match result {
        Ok(value) => {
            if let Some(error) = cleanup
                .err()
                .filter(|error| error.kind() != std::io::ErrorKind::NotFound)
            {
                return Err(format!(
                    "restore completed, but temporary extraction cleanup failed: {error}"
                ));
            }
            Ok(value)
        }
        Err(error) => {
            if let Some(cleanup) = cleanup
                .err()
                .filter(|cleanup| cleanup.kind() != std::io::ErrorKind::NotFound)
            {
                return Err(format!(
                    "{error}; temporary extraction cleanup failed: {cleanup}"
                ));
            }
            Err(error)
        }
    }
}

/// Puts the backup's world in place of the current one, which goes to `backups/replaced-<time>/`. The
/// server must be stopped. Returns where the old world went.
pub fn restore(dir: &Path, config: &Config, name: &str) -> Result<Option<PathBuf>, String> {
    let zip_path = dir.join(DIR).join(name);
    with_restore_scratch(dir, |scratch| {
        extract_backup_archive(&zip_path, scratch, None, BACKUP_LIMITS)?;
        let world = &config.world.name;
        let restored = scratch.join(world);
        if !restored.join("level.dat").is_file() {
            return Err(format!(
                "{name} holds no world named {world:?} (no {world}/level.dat)"
            ));
        }
        let current = dir.join(world);
        let moved = if current.exists() {
            let aside = dir
                .join(DIR)
                .join(format!("replaced-{}", stamp(SystemTime::now())));
            std::fs::create_dir_all(&aside).map_err(|e| e.to_string())?;
            let target = aside.join(world);
            std::fs::rename(&current, &target)
                .map_err(|e| format!("moving the current world aside: {e}"))?;
            Some(target)
        } else {
            None
        };
        put_restored_path_in_place(
            &restored,
            &current,
            moved.as_deref(),
            "putting the backup in place",
        )?;
        Ok(moved)
    })
}

/// A dimension id (`minecraft:the_nether`, or `the_nether` for `minecraft:`) as the id and its folder in the
/// world (`dimensions/<namespace>/<path>`, as the game saves it). Only the characters of a resource id.
pub fn dimension_folder(dimension: &str) -> Result<(String, String), String> {
    let (namespace, path) = dimension
        .split_once(':')
        .unwrap_or(("minecraft", dimension));
    let namespace_ok = !namespace.is_empty()
        && namespace
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-' | '.'));
    let path_ok = !path.is_empty()
        && path
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
        && path
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-' | '.' | '/'));
    if !namespace_ok || !path_ok {
        return Err(format!(
            "{dimension:?} is not a dimension id (like minecraft:the_nether)"
        ));
    }
    Ok((
        format!("{namespace}:{path}"),
        format!("dimensions/{namespace}/{path}"),
    ))
}

/// Puts one dimension of the backup in place of the current one, leaving the rest of the world (the other
/// dimensions, `level.dat`, the players) as it is. The current dimension goes to
/// `backups/replaced-<time>/<world>/dimensions/...`. The server must be stopped. Returns where the old
/// folder went (None when the world had none).
pub fn restore_dimension(
    dir: &Path,
    config: &Config,
    name: &str,
    dimension: &str,
) -> Result<Option<PathBuf>, String> {
    let (id, folder) = dimension_folder(dimension)?;
    let world = &config.world.name;
    let prefix = PathBuf::from(world).join(&folder);
    let zip_path = dir.join(DIR).join(name);

    with_restore_scratch(dir, |scratch| {
        let files = extract_backup_archive(&zip_path, scratch, Some(&prefix), BACKUP_LIMITS)?;
        if files == 0 {
            return Err(format!(
                "{name} holds nothing of the dimension {id} (no {world}/{folder}/)"
            ));
        }

        let restored = scratch.join(&prefix);
        let current = dir.join(&prefix);
        let moved = if current.exists() {
            let target = dir
                .join(DIR)
                .join(format!("replaced-{}", stamp(SystemTime::now())))
                .join(&prefix);
            if target.exists() {
                return Err(format!(
                    "{} exists already; try again in a second",
                    target.display()
                ));
            }
            std::fs::create_dir_all(target.parent().expect("the folder has a parent"))
                .map_err(|e| e.to_string())?;
            std::fs::rename(&current, &target)
                .map_err(|e| format!("moving the current {id} aside: {e}"))?;
            Some(target)
        } else {
            None
        };
        put_restored_path_in_place(
            &restored,
            &current,
            moved.as_deref(),
            &format!("putting the backup's {id} in place"),
        )?;
        Ok(moved)
    })
}

/// `every` from server.toml: `"6h"`, `"30m"`, `"1d"`, or `"0"` for no automatic backups.
pub fn interval(every: &str) -> Result<Option<std::time::Duration>, String> {
    let every = every.trim();
    if every == "0" || every.is_empty() {
        return Ok(None);
    }
    let (number, unit) = every.split_at(every.len() - 1);
    let n: u64 = number.trim().parse().map_err(|_| {
        format!("backups.every is {every:?}: use a number and m, h or d, like \"6h\"")
    })?;
    let secs = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => {
            return Err(format!(
                "backups.every is {every:?}: use a number and m, h or d, like \"6h\""
            ));
        }
    };
    Ok((n > 0).then(|| std::time::Duration::from_secs(n * secs)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_sort_by_time() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_792_000_000);
        assert_eq!(
            file_name(t, Some("before-update")).unwrap(),
            "2026-10-14_17-46-40-before-update.zip"
        );
        assert!(file_name(t, Some("../x")).is_err());
        assert_eq!(auto_name(t), "2026-10-14_17-46-40-auto.zip");
    }

    #[test]
    fn intervals() {
        assert_eq!(
            interval("6h").unwrap(),
            Some(std::time::Duration::from_secs(21_600))
        );
        assert_eq!(
            interval("30m").unwrap(),
            Some(std::time::Duration::from_secs(1800))
        );
        assert_eq!(interval("0").unwrap(), None);
        assert!(interval("6 hours").is_err());
    }

    #[test]
    fn back_up_and_restore() {
        let dir =
            std::env::temp_dir().join(format!("horizon-server-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config::default();
        let world = dir.join("world");
        std::fs::create_dir_all(world.join("region")).unwrap();
        std::fs::write(world.join("level.dat"), b"before").unwrap();
        std::fs::write(world.join("region/r.0.0.mca"), vec![7u8; 10_000]).unwrap();
        std::fs::write(world.join("session.lock"), b"x").unwrap();
        std::fs::write(dir.join("server.toml"), crate::config::TEMPLATE).unwrap();
        let t = SystemTime::now();
        let name = file_name(t, None).unwrap();
        create(&dir, &config, &name).unwrap();
        assert_eq!(list(&dir).len(), 1);
        // The world changes, then the backup comes back.
        std::fs::write(world.join("level.dat"), b"after").unwrap();
        let moved = restore(&dir, &config, &find(&dir, &name[..10]).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(world.join("level.dat")).unwrap(), b"before");
        assert_eq!(
            std::fs::read(world.join("region/r.0.0.mca")).unwrap().len(),
            10_000
        );
        assert!(!world.join("session.lock").exists());
        assert_eq!(std::fs::read(moved.join("level.dat")).unwrap(), b"after");
        // Only automatic backups are pruned.
        for i in 0..3 {
            let later = t + std::time::Duration::from_secs(10 + i);
            create(&dir, &config, &auto_name(later)).unwrap();
        }
        assert_eq!(prune(&dir, 1).len(), 2);
        let left: Vec<String> = list(&dir).into_iter().map(|b| b.name).collect();
        assert_eq!(left.len(), 2);
        assert!(left.iter().any(|n| n.ends_with("-manual.zip")));
        // A backup is found by its name, or the newest with a label.
        assert_eq!(find(&dir, "manual").unwrap(), name);
        assert_eq!(find(&dir, &left[0]).unwrap(), left[0]);
        assert_eq!(
            find(&dir, left[0].trim_end_matches(".zip")).unwrap(),
            left[0]
        );
        assert_eq!(find(&dir, "auto").unwrap(), left[0]);
        assert!(find(&dir, "nothing").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn archive_paths_reject_windows_aliases_and_case_collisions() {
        assert!(validate_archive_path("world/CON.txt", false).is_err());
        assert!(validate_archive_path("world/CON .txt", false).is_err());
        assert!(validate_archive_path("world/Lpt9.dat", false).is_err());
        assert!(validate_archive_path("world/file:stream", false).is_err());
        assert!(validate_archive_path("world/trailing. ", false).is_err());
        assert!(validate_archive_path("world/../escape", false).is_err());
        assert!(validate_archive_path("world\\escape", false).is_err());

        let dir = std::env::temp_dir().join(format!(
            "horizon-server-zip-collision-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("collision.zip");
        let file = File::create(&path).unwrap();
        let mut zip = ZipWriter::new(std::io::BufWriter::new(file));
        for name in ["world/region/A.mca", "world/region/a.mca"] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(b"data").unwrap();
        }
        zip.finish().unwrap();
        let file = File::open(&path).unwrap();
        let bytes = file.metadata().unwrap().len();
        let mut archive = ZipArchive::new(file).unwrap();
        assert!(inspect_archive(&mut archive, bytes, BACKUP_LIMITS).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn archive_expansion_limit_is_checked_before_writing() {
        let dir =
            std::env::temp_dir().join(format!("horizon-server-zip-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("limit.zip");
        let file = File::create(&path).unwrap();
        let mut zip = ZipWriter::new(std::io::BufWriter::new(file));
        for name in ["world/one.dat", "world/two.dat"] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(b"four").unwrap();
        }
        zip.finish().unwrap();
        let file = File::open(&path).unwrap();
        let bytes = file.metadata().unwrap().len();
        let mut archive = ZipArchive::new(file).unwrap();
        let limits = ArchiveLimits {
            max_archive_bytes: u64::MAX,
            max_entries: 10,
            max_entry_bytes: 16,
            max_total_bytes: 7,
        };
        assert!(inspect_archive(&mut archive, bytes, limits).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dimension_restore_fails_closed_on_unsafe_unrelated_entry() {
        let dir =
            std::env::temp_dir().join(format!("horizon-server-dim-zipslip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config::default();
        let current = dir.join("world/dimensions/minecraft/the_nether/region/r.0.0.mca");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, b"before").unwrap();
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        let file = File::create(dir.join("backups/manual.zip")).unwrap();
        let mut zip = ZipWriter::new(std::io::BufWriter::new(file));
        zip.start_file(
            "world/dimensions/minecraft/the_nether/region/r.0.0.mca",
            SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"backup").unwrap();
        zip.start_file("../../escape.txt", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"pwned").unwrap();
        zip.finish().unwrap();

        assert!(restore_dimension(&dir, &config, "manual.zip", "the_nether").is_err());
        assert_eq!(std::fs::read(&current).unwrap(), b"before");
        assert!(!dir.join("escape.txt").exists());
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".horizon-restore-")
        }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restore_scratch_does_not_follow_a_horizon_symlink() {
        use std::os::unix::fs::symlink;

        let dir =
            std::env::temp_dir().join(format!("horizon-server-scratch-{}", std::process::id()));
        let outside = dir.with_extension("outside");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(outside.join("restore")).unwrap();
        std::fs::write(outside.join("restore/keep.txt"), b"keep").unwrap();
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        symlink(&outside, dir.join(".horizon")).unwrap();

        let config = Config::default();
        let file = File::create(dir.join("backups/manual.zip")).unwrap();
        let mut zip = ZipWriter::new(std::io::BufWriter::new(file));
        zip.start_file("world/level.dat", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"restored").unwrap();
        zip.finish().unwrap();

        restore(&dir, &config, "manual.zip").unwrap();
        assert_eq!(
            std::fs::read(outside.join("restore/keep.txt")).unwrap(),
            b"keep"
        );
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".horizon-restore-")
        }));
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn restore_refuses_zip_slip_entry() {
        let dir =
            std::env::temp_dir().join(format!("horizon-server-zipslip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config::default();
        let world = dir.join("world");
        std::fs::create_dir_all(&world).unwrap();
        std::fs::write(world.join("level.dat"), b"before").unwrap();
        std::fs::write(dir.join("server.toml"), crate::config::TEMPLATE).unwrap();
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        let zip_path = dir.join("backups").join("manual.zip");
        let f = std::fs::File::create(&zip_path).unwrap();
        let mut w = ZipWriter::new(std::io::BufWriter::new(f));
        w.start_file("world/level.dat", SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"before").unwrap();
        w.start_file("../../escape.txt", SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"pwned").unwrap();
        w.finish()
            .unwrap()
            .into_inner()
            .unwrap()
            .sync_all()
            .unwrap();
        let result = restore(&dir, &config, "manual.zip");
        assert!(
            result.is_err(),
            "zip-slip entry must be refused: {:?}",
            result.err()
        );
        assert!(
            !dir.join("escape.txt").exists(),
            "no file outside the scratch dir"
        );
        assert!(
            !dir.join(".horizon/restore").exists(),
            "failed extraction must clean its scratch directory"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restore_rolls_back_the_previous_world_if_install_fails() {
        let dir =
            std::env::temp_dir().join(format!("horizon-server-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let previous = dir.join("backups/replaced/world");
        let current = dir.join("world");
        let missing = dir.join(".horizon/restore/world");
        std::fs::create_dir_all(&previous).unwrap();
        std::fs::write(previous.join("level.dat"), b"before").unwrap();

        let error = put_restored_path_in_place(
            &missing,
            &current,
            Some(&previous),
            "putting the backup in place",
        )
        .unwrap_err();

        assert!(error.contains("putting the backup in place"));
        assert_eq!(std::fs::read(current.join("level.dat")).unwrap(), b"before");
        assert!(!previous.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restore_one_dimension_only() {
        let dir = std::env::temp_dir().join(format!("horizon-server-dim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config::default();
        let world = dir.join("world");
        let overworld = world.join("dimensions/minecraft/overworld/region/r.0.0.mca");
        let nether = world.join("dimensions/minecraft/the_nether/region/r.0.0.mca");
        for (file, bytes) in [(&overworld, "ow-before"), (&nether, "nether-before")] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, bytes).unwrap();
        }
        std::fs::write(world.join("level.dat"), b"level-before").unwrap();
        let name = file_name(SystemTime::now(), None).unwrap();
        create(&dir, &config, &name).unwrap();
        std::fs::write(&overworld, "ow-after").unwrap();
        std::fs::write(&nether, "nether-after").unwrap();
        std::fs::write(world.join("level.dat"), b"level-after").unwrap();

        // Unknown or malformed dimensions change nothing.
        assert!(restore_dimension(&dir, &config, &name, "minecraft:the_end").is_err());
        assert!(restore_dimension(&dir, &config, &name, "../world").is_err());
        assert!(restore_dimension(&dir, &config, &name, "minecraft:../x").is_err());
        assert_eq!(std::fs::read(&nether).unwrap(), b"nether-after");

        // `the_nether` means `minecraft:the_nether`.
        let moved = restore_dimension(&dir, &config, &name, "the_nether")
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&nether).unwrap(), b"nether-before");
        assert_eq!(std::fs::read(&overworld).unwrap(), b"ow-after");
        assert_eq!(
            std::fs::read(world.join("level.dat")).unwrap(),
            b"level-after"
        );
        assert_eq!(
            std::fs::read(moved.join("region/r.0.0.mca")).unwrap(),
            b"nether-after"
        );
        assert!(!dir.join(".horizon/restore").exists());

        // A dimension the world lost comes back too, with nothing to put aside.
        std::fs::remove_dir_all(world.join("dimensions/minecraft/the_nether")).unwrap();
        assert!(
            restore_dimension(&dir, &config, &name, "minecraft:the_nether")
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&nether).unwrap(), b"nether-before");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
