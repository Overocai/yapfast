//! The plugins installed where VST plugins go, found without loading them:
//! each file's PE header says whether it's 64-bit and what it exports, so a
//! 32-bit plugin, or a library that isn't a plugin, is told apart before
//! anything of it runs.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// A plugin's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Vst2,
    Vst3,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vst2 => "VST2",
            Self::Vst3 => "VST3",
        }
    }

    /// How `yapfast --vst-try` names it.
    pub fn argument(self) -> &'static str {
        match self {
            Self::Vst2 => "vst2",
            Self::Vst3 => "vst3",
        }
    }

    pub fn from_argument(argument: &str) -> Option<Self> {
        [Self::Vst2, Self::Vst3]
            .into_iter()
            .find(|format| format.argument() == argument)
    }
}

/// A plugin file found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The file, or a VST3 bundle's folder.
    pub path: PathBuf,
    pub format: Format,
    /// Its file's name, without its ending.
    pub name: String,
    /// Why Yapfast can't load it, such as a 32-bit plugin's; none when it
    /// may.
    pub unusable: Option<&'static str>,
}

/// How deep a folder's plugins are looked for.
const DEPTH: usize = 6;

/// The folders VST plugins are installed in, with the format each holds:
/// on Windows the VST3 folders Steinberg names and the VST2 folders hosts
/// look in; elsewhere, the Linux ones.
pub fn folders() -> Vec<(PathBuf, Format)> {
    let mut folders = Vec::new();
    if cfg!(windows) {
        let env = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let common = env("CommonProgramFiles")
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Common Files"));
        let programs = env("ProgramFiles").unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        folders.push((common.join("VST3"), Format::Vst3));
        if let Some(local) = env("LOCALAPPDATA") {
            folders.push((local.join(r"Programs\Common\VST3"), Format::Vst3));
        }
        for folder in [
            programs.join("VSTPlugins"),
            programs.join(r"Steinberg\VSTPlugins"),
            programs.join("VST2"),
            programs.join(r"Common Files\VST2"),
            common.join("VST2"),
            common.join(r"Steinberg\VST2"),
        ] {
            folders.push((folder, Format::Vst2));
        }
        // VST2 plugins installed beside VST3 ones, as some installers do.
        folders.push((common.join("VST3"), Format::Vst2));
    } else if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        folders.push((home.join(".vst3"), Format::Vst3));
        folders.push((PathBuf::from("/usr/lib/vst3"), Format::Vst3));
        folders.push((home.join(".vst"), Format::Vst2));
        folders.push((PathBuf::from("/usr/lib/vst"), Format::Vst2));
    }
    folders.dedup();
    folders
}

/// The plugins in [`folders`] and in `extra` (folders of either format),
/// by name. Nothing is loaded.
pub fn scan(extra: &[PathBuf]) -> Vec<Found> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let extra = extra.iter().flat_map(|folder| {
        [
            (folder.clone(), Format::Vst3),
            (folder.clone(), Format::Vst2),
        ]
    });
    for (folder, format) in folders().into_iter().chain(extra) {
        walk(&folder, format, DEPTH, &mut |path| {
            if seen.insert((path.to_path_buf(), format))
                && let Some(plugin) = look(path, format)
            {
                found.push(plugin);
            }
        });
    }
    found.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.format.label().cmp(b.format.label()))
    });
    found
}

/// Calls `each` with each file or VST3 bundle in `folder` that may hold a
/// plugin of `format`, down to `depth` folders.
fn walk(folder: &Path, format: Format, depth: usize, each: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ending = path
            .extension()
            .map(|ending| ending.to_string_lossy().to_lowercase());
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        match (format, ending.as_deref()) {
            // A VST3 bundle is a folder; an older one a file.
            (Format::Vst3, Some("vst3")) => each(&path),
            (Format::Vst2, Some("dll" | "so")) if !kind.is_dir() => each(&path),
            _ if kind.is_dir() && depth > 0 => walk(&path, format, depth - 1, each),
            _ => {}
        }
    }
}

/// What a file or bundle at `path` holds, as `format`: none when it isn't
/// a plugin of that format.
pub fn look(path: &Path, format: Format) -> Option<Found> {
    let name = path.file_stem()?.to_string_lossy().into_owned();
    let binary = match format {
        Format::Vst3 => vst3_binary(path)?,
        Format::Vst2 => path.to_path_buf(),
    };
    let unusable = if cfg!(windows) {
        let pe = Pe::read(&binary)?;
        let entry = match format {
            Format::Vst3 => pe.exports("GetPluginFactory"),
            Format::Vst2 => pe.exports("VSTPluginMain") || pe.exports("main"),
        };
        if !entry {
            return None;
        }
        match pe.machine {
            AMD64 => None,
            I386 => Some("32-bit, which a 64-bit Yapfast can't load"),
            _ => Some("built for another processor"),
        }
    } else {
        Some("Voice effects run on Windows")
    };
    Some(Found {
        path: path.to_path_buf(),
        format,
        name,
        unusable,
    })
}

/// The plugins `query` names, by their place in `found`, best first: those
/// whose name starts with it, then those whose name holds it, then those
/// whose name, format and folder hold each of its words; ignoring case. An
/// empty query takes them all, in their order.
pub fn search<'a>(found: &'a [Found], query: &str) -> Vec<(usize, &'a Found)> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return found.iter().enumerate().collect();
    }
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut hits: Vec<(u8, usize, &Found)> = found
        .iter()
        .enumerate()
        .filter_map(|(at, plugin)| {
            let name = plugin.name.to_lowercase();
            let rank = if name.starts_with(&query) {
                0
            } else if name.contains(&query) {
                1
            } else {
                let all = format!(
                    "{name} {} {}",
                    plugin.format.label().to_lowercase(),
                    folder(plugin).unwrap_or_default().to_lowercase()
                );
                if !words.iter().all(|word| all.contains(word)) {
                    return None;
                }
                2
            };
            Some((rank, at, plugin))
        })
        .collect();
    hits.sort_by_key(|&(rank, at, _)| (rank, at));
    hits.into_iter()
        .map(|(_, at, plugin)| (at, plugin))
        .collect()
}

/// The folder a plugin sits in when it isn't one plugins are looked for in:
/// often its maker's, as AIR Music Technology.
pub fn folder(plugin: &Found) -> Option<String> {
    let name = plugin.path.parent()?.file_name()?.to_string_lossy();
    let looked = ["vst3", "vst2", "vst", "vstplugins", "steinberg", "plugins"];
    (!looked
        .iter()
        .any(|folder| name.eq_ignore_ascii_case(folder)))
    .then(|| name.into_owned())
}

/// The library inside a VST3 plugin: the file itself, or in a bundle's
/// folder the one for this processor.
pub fn vst3_binary(path: &Path) -> Option<PathBuf> {
    if !path.is_dir() {
        return path.is_file().then(|| path.to_path_buf());
    }
    let name = path.file_name()?;
    let folder = if cfg!(windows) {
        "x86_64-win"
    } else {
        "x86_64-linux"
    };
    let mut binary = path.join("Contents").join(folder).join(name);
    if !cfg!(windows) {
        binary.set_extension("so");
    }
    binary.is_file().then_some(binary)
}

const I386: u16 = 0x14c;
const AMD64: u16 = 0x8664;

/// What a Windows library's headers say: the processor it's for, and the
/// names it exports.
struct Pe {
    machine: u16,
    exports: Vec<String>,
}

impl Pe {
    /// Reads a PE file's headers and its export directory's names.
    fn read(path: &Path) -> Option<Self> {
        let mut file = File::open(path).ok()?;
        let length = file.metadata().ok()?.len();
        let mut dos = [0u8; 64];
        file.read_exact(&mut dos).ok()?;
        if &dos[..2] != b"MZ" {
            return None;
        }
        let pe_at = u64::from(u32::from_le_bytes(dos[60..64].try_into().ok()?));
        if pe_at + 24 > length {
            return None;
        }
        file.seek(SeekFrom::Start(pe_at)).ok()?;
        let mut header = [0u8; 24];
        file.read_exact(&mut header).ok()?;
        if &header[..4] != b"PE\0\0" {
            return None;
        }
        let machine = u16::from_le_bytes([header[4], header[5]]);
        let sections = usize::from(u16::from_le_bytes([header[6], header[7]]));
        let optional_size = usize::from(u16::from_le_bytes([header[20], header[21]]));
        let mut optional = vec![0u8; optional_size];
        file.read_exact(&mut optional).ok()?;
        let magic = u16::from_le_bytes(optional.get(..2)?.try_into().ok()?);
        // The data directories' start: PE32's or PE32+'s.
        let directories = match magic {
            0x10b => 96,
            0x20b => 112,
            _ => return None,
        };
        let export = optional.get(directories..directories + 8)?;
        let export_rva = u32::from_le_bytes(export[..4].try_into().ok()?);
        let mut table = vec![0u8; sections * 40];
        file.read_exact(&mut table).ok()?;
        let sections: Vec<(u32, u32, u32)> = table
            .as_chunks::<40>()
            .0
            .iter()
            .map(|section| {
                let word = |at: usize| u32::from_le_bytes(section[at..at + 4].try_into().unwrap());
                // Its address, size in memory, and where it lies in the file.
                (word(12), word(8).max(word(16)), word(20))
            })
            .collect();
        let offset = |rva: u32| -> Option<u64> {
            sections.iter().find_map(|&(address, size, raw)| {
                (rva >= address && rva < address + size).then(|| u64::from(raw + (rva - address)))
            })
        };
        let mut exports = Vec::new();
        if export_rva != 0 {
            let at = offset(export_rva)?;
            file.seek(SeekFrom::Start(at)).ok()?;
            let mut directory = [0u8; 40];
            file.read_exact(&mut directory).ok()?;
            let word = |at: usize| u32::from_le_bytes(directory[at..at + 4].try_into().unwrap());
            let (count, names) = (word(24).min(4096), word(32));
            if let Some(names_at) = offset(names) {
                let mut rvas = vec![0u8; count as usize * 4];
                file.seek(SeekFrom::Start(names_at)).ok()?;
                file.read_exact(&mut rvas).ok()?;
                for rva in rvas.as_chunks::<4>().0 {
                    let Some(name_at) = offset(u32::from_le_bytes(*rva)) else {
                        continue;
                    };
                    let mut name = [0u8; 64];
                    file.seek(SeekFrom::Start(name_at)).ok()?;
                    let read = file.read(&mut name).ok()?;
                    let end = name[..read]
                        .iter()
                        .position(|&byte| byte == 0)
                        .unwrap_or(read);
                    exports.push(String::from_utf8_lossy(&name[..end]).into_owned());
                }
            }
        }
        Some(Self { machine, exports })
    }

    fn exports(&self, name: &str) -> bool {
        self.exports.iter().any(|export| export == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugins_are_searched_by_name_then_format_and_folder() {
        let plugin = |path: &str, format| Found {
            path: PathBuf::from(path),
            format,
            name: Path::new(path)
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            unusable: None,
        };
        let found = [
            plugin("/vst3/AIR Music Technology/Xpand!2.vst3", Format::Vst3),
            plugin("/vst3/Wider.vst3", Format::Vst3),
            plugin("/vst/Wider.dll", Format::Vst2),
            plugin("/vst/OldWide.dll", Format::Vst2),
        ];
        let names = |query: &str| -> Vec<(usize, Format)> {
            search(&found, query)
                .into_iter()
                .map(|(at, plugin)| (at, plugin.format))
                .collect()
        };
        assert_eq!(
            names("wid"),
            [(1, Format::Vst3), (2, Format::Vst2), (3, Format::Vst2)],
            "starting with it first, then holding it"
        );
        assert_eq!(names("wider vst2"), [(2, Format::Vst2)]);
        assert_eq!(names("air"), [(0, Format::Vst3)], "by its maker's folder");
        assert_eq!(names("  "), names(""), "nothing typed takes them all");
        assert_eq!(names("").len(), 4);
        assert!(names("reverb").is_empty());
    }

    #[test]
    fn a_file_that_isnt_a_library_is_no_plugin() {
        let dir = std::env::temp_dir().join(format!("yapfast-vst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.dll");
        std::fs::write(&path, b"not a library").unwrap();
        assert!(Pe::read(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plugins_are_found_by_their_folders_and_endings() {
        let dir = std::env::temp_dir().join(format!("yapfast-vst-walk-{}", std::process::id()));
        let bundle = dir.join("Wide.vst3");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::create_dir_all(dir.join("Vendor")).unwrap();
        std::fs::write(dir.join("Vendor").join("Comp.dll"), b"").unwrap();
        let mut vst3 = Vec::new();
        walk(&dir, Format::Vst3, 2, &mut |path| {
            vst3.push(path.to_path_buf());
        });
        assert_eq!(vst3, [bundle]);
        let mut vst2 = Vec::new();
        walk(&dir, Format::Vst2, 2, &mut |path| {
            vst2.push(path.to_path_buf());
        });
        assert_eq!(vst2, [dir.join("Vendor").join("Comp.dll")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
