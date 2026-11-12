use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    SevenZip,
    Zip,
    Rar,
}

impl ArchiveKind {
    pub fn label(self) -> &'static str {
        match self {
            ArchiveKind::SevenZip => "7z",
            ArchiveKind::Zip => "zip",
            ArchiveKind::Rar => "rar",
        }
    }
}

/// Detect the archive kind from a file name. Multi-volume archives are
/// recognized by their first volume naming conventions.
pub fn detect_kind(path: &Path) -> Option<ArchiveKind> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();

    if name.ends_with(".zip") || is_zip_split(&name) || is_zip_numbered(&name) {
        return Some(ArchiveKind::Zip);
    }

    if name.ends_with(".7z") || is_numbered_suffix(&name, ".7z.") {
        return Some(ArchiveKind::SevenZip);
    }

    if name.ends_with(".rar")
        || name.ends_with(".r00")
        || is_part_rar(&name)
        || is_numbered_suffix(&name, ".rar.")
    {
        return Some(ArchiveKind::Rar);
    }

    None
}

fn is_numbered_suffix(name: &str, prefix: &str) -> bool {
    if let Some(rest) = name.strip_suffix(".001") {
        rest.ends_with(prefix.trim_end_matches('.'))
    } else {
        false
    }
}

fn is_zip_split(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 4 {
        return false;
    }
    let n = bytes.len();
    bytes[n - 4] == b'.'
        && bytes[n - 3] == b'z'
        && bytes[n - 2].is_ascii_digit()
        && bytes[n - 1].is_ascii_digit()
}

fn is_zip_numbered(name: &str) -> bool {
    match name.rsplit_once(".zip.") {
        Some((_, num)) => !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

fn is_part_rar(name: &str) -> bool {
    if !name.ends_with(".rar") {
        return false;
    }
    let stem = name.trim_end_matches(".rar");
    let mut it = stem.rsplit('.');
    let Some(part) = it.next() else {
        return false;
    };
    part.len() > 4 && part.starts_with("part") && part[4..].chars().all(|c| c.is_ascii_digit())
}

/// Return every volume that belongs to the archive whose first volume is
/// `first`. The returned list is sorted in extraction order.
pub fn collect_volumes(first: &Path) -> Vec<PathBuf> {
    let Some(kind) = detect_kind(first) else {
        return vec![first.to_path_buf()];
    };

    let dir = first.parent().map(Path::to_path_buf).unwrap_or_default();
    // Directory entries are compared after lowercasing; normalize the base
    // name too, otherwise uppercase archive names never match their volumes.
    let name = first
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();

    let mut found: Vec<PathBuf> = match kind {
        ArchiveKind::SevenZip => {
            let base = base_stem(&name, &[".7z", ".7z.001"]);
            scan_dir(&dir, |n| {
                let n = n.to_ascii_lowercase();
                n == format!("{base}.7z")
                    || (n.starts_with(&format!("{base}.7z."))
                        && n[format!("{base}.7z.").len()..]
                            .chars()
                            .all(|c| c.is_ascii_digit()))
            })
        }
        ArchiveKind::Zip => {
            let base = if let Some(b) = name.strip_suffix(".zip") {
                b.to_string()
            } else if is_zip_numbered(&name) {
                name.rsplit_once(".zip.")
                    .map(|(b, _)| b.to_string())
                    .unwrap_or_default()
            } else if is_zip_split(&name) {
                name[..name.len() - 4].to_string()
            } else {
                name.to_string()
            };
            scan_dir(&dir, |n| {
                let n = n.to_ascii_lowercase();
                let prefix = format!("{base}.z");
                let numbered = format!("{base}.zip.");
                n == format!("{base}.zip")
                    || (n.starts_with(&prefix)
                        && n.len() == prefix.len() + 2
                        && n[prefix.len()..].chars().all(|c| c.is_ascii_digit()))
                    || (n.starts_with(&numbered)
                        && !n[numbered.len()..].is_empty()
                        && n[numbered.len()..].chars().all(|c| c.is_ascii_digit()))
            })
        }
        ArchiveKind::Rar => {
            let base = if is_part_rar(&name) {
                name.trim_end_matches(".rar")
                    .rsplit_once('.')
                    .map(|(b, _)| b.to_string())
                    .unwrap_or_else(|| name.trim_end_matches(".rar").to_string())
            } else if let Some(b) = name.strip_suffix(".rar") {
                b.to_string()
            } else if let Some(b) = name.strip_suffix(".r00") {
                b.to_string()
            } else {
                name.to_string()
            };

            scan_dir(&dir, |n| {
                let n = n.to_ascii_lowercase();
                n == format!("{base}.rar")
                    || (n.starts_with(&format!("{base}.part"))
                        && n.ends_with(".rar")
                        && n[format!("{base}.part").len()..n.len() - 4]
                            .chars()
                            .all(|c| c.is_ascii_digit()))
                    || (n.starts_with(&format!("{base}.r"))
                        && n.len() == format!("{base}.r").len() + 2
                        && n[format!("{base}.r").len()..]
                            .chars()
                            .all(|c| c.is_ascii_digit()))
            })
        }
    };

    found.sort_by_key(|p| volume_sort_key(p, kind));
    if !found.iter().any(|p| p == first) {
        found.insert(0, first.to_path_buf());
    }
    found
}

fn base_stem(name: &str, suffixes: &[&str]) -> String {
    for suffix in suffixes {
        if name.to_ascii_lowercase().ends_with(suffix) {
            return name[..name.len() - suffix.len()].to_string();
        }
    }
    name.to_string()
}

fn scan_dir(dir: &Path, predicate: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(n) = path.file_name().and_then(OsStr::to_str) {
                if predicate(n) {
                    out.push(path);
                }
            }
        }
    }
    out
}

fn volume_sort_key(path: &Path, kind: ArchiveKind) -> (u64, String) {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();

    let num = match kind {
        ArchiveKind::SevenZip => {
            if name.ends_with(".7z") {
                Some(0)
            } else if let Some((_, n)) = name.rsplit_once(".7z.") {
                n.parse::<u64>().ok()
            } else {
                None
            }
        }
        ArchiveKind::Zip => {
            if name.ends_with(".zip") {
                Some(0)
            } else if let Some((_, n)) = name.rsplit_once(".zip.") {
                n.parse::<u64>().ok()
            } else if let Some((_, n)) = name.rsplit_once(".z") {
                if n.len() == 2 && n.chars().all(|c| c.is_ascii_digit()) {
                    n.parse::<u64>().ok()
                } else {
                    None
                }
            } else {
                None
            }
        }
        ArchiveKind::Rar => {
            if name.ends_with(".rar") {
                if let Some((_, n)) = name.trim_end_matches(".rar").rsplit_once(".part") {
                    n.parse::<u64>().ok()
                } else {
                    Some(0)
                }
            } else if let Some((_, n)) = name.rsplit_once(".r") {
                // Old-style volumes: archive.r00 is the second volume.
                if n.len() == 2 && n.chars().all(|c| c.is_ascii_digit()) {
                    n.parse::<u64>().ok().map(|v| v + 1)
                } else {
                    None
                }
            } else {
                None
            }
        }
    }
    .unwrap_or(0);

    // Sort by the numeric volume index first, then by name for stability.
    (num, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("unpack-core-test-{}-{}", std::process::id(), name));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), b"x").unwrap();
    }

    #[test]
    fn detects_7z_volume() {
        assert_eq!(
            detect_kind(Path::new("game.7z.001")),
            Some(ArchiveKind::SevenZip)
        );
        assert_eq!(
            detect_kind(Path::new("game.7z")),
            Some(ArchiveKind::SevenZip)
        );
    }

    #[test]
    fn detects_zip_split() {
        assert_eq!(detect_kind(Path::new("movie.zip")), Some(ArchiveKind::Zip));
        assert_eq!(detect_kind(Path::new("movie.z01")), Some(ArchiveKind::Zip));
        assert_eq!(
            detect_kind(Path::new("movie.zip.001")),
            Some(ArchiveKind::Zip)
        );
    }

    #[test]
    fn detects_rar_part() {
        assert_eq!(
            detect_kind(Path::new("movie.part1.rar")),
            Some(ArchiveKind::Rar)
        );
        assert_eq!(
            detect_kind(Path::new("movie.part01.rar")),
            Some(ArchiveKind::Rar)
        );
        assert_eq!(detect_kind(Path::new("movie.rar")), Some(ArchiveKind::Rar));
        assert_eq!(detect_kind(Path::new("movie.r00")), Some(ArchiveKind::Rar));
    }

    #[test]
    fn collects_padded_rar_parts() {
        let dir = scratch("rar");
        touch(&dir, "movie.part01.rar");
        touch(&dir, "movie.part02.rar");
        touch(&dir, "movie.part03.rar");
        touch(&dir, "unrelated.txt");

        let volumes = collect_volumes(&dir.join("movie.part01.rar"));
        let names: Vec<String> = volumes
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "movie.part01.rar".to_string(),
                "movie.part02.rar".to_string(),
                "movie.part03.rar".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_uppercase_rar_parts() {
        let dir = scratch("rar-uppercase");
        touch(&dir, "PPSA03016-app.part01.rar");
        touch(&dir, "PPSA03016-app.part02.rar");
        touch(&dir, "PPSA03016-app.part10.rar");

        let volumes = collect_volumes(&dir.join("PPSA03016-app.part01.rar"));
        let names: Vec<String> = volumes
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "PPSA03016-app.part01.rar".to_string(),
                "PPSA03016-app.part02.rar".to_string(),
                "PPSA03016-app.part10.rar".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_7z_volumes() {
        let dir = scratch("7z");
        touch(&dir, "game.7z.001");
        touch(&dir, "game.7z.002");
        touch(&dir, "game.7z.010");
        touch(&dir, "game.7z.003");

        let volumes = collect_volumes(&dir.join("game.7z.001"));
        let names: Vec<String> = volumes
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "game.7z.001".to_string(),
                "game.7z.002".to_string(),
                "game.7z.003".to_string(),
                "game.7z.010".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_zip_volumes() {
        let dir = scratch("zip");
        touch(&dir, "movie.zip");
        touch(&dir, "movie.z01");
        touch(&dir, "movie.z02");
        touch(&dir, "movie.z10");
        touch(&dir, "unrelated.txt");

        let volumes = collect_volumes(&dir.join("movie.zip"));
        let names: Vec<String> = volumes
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "movie.zip".to_string(),
                "movie.z01".to_string(),
                "movie.z02".to_string(),
                "movie.z10".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_numbered_zip_volumes() {
        let dir = scratch("zip-numbered");
        touch(&dir, "movie.zip.001");
        touch(&dir, "movie.zip.002");
        touch(&dir, "movie.zip.010");

        let volumes = collect_volumes(&dir.join("movie.zip.001"));
        let names: Vec<String> = volumes
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "movie.zip.001".to_string(),
                "movie.zip.002".to_string(),
                "movie.zip.010".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
