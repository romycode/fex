use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
fn pins_file() -> io::Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_CONFIG_HOME / HOME"))?;
    Ok(config.join("fex/pins"))
}

#[cfg(target_os = "macos")]
fn pins_file() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/fex/pins"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME"))
}

fn read_pins(file: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read(file) {
        Ok(bytes) => Ok(bytes
            .split(|byte| *byte == 0)
            .filter(|path| path.starts_with(b"/"))
            .map(|path| PathBuf::from(OsString::from_vec(path.to_vec())))
            .collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub fn load_pins() -> io::Result<Vec<PathBuf>> {
    read_pins(&pins_file()?)
}

pub fn set_pinned(path: &Path, pinned: bool) -> io::Result<Vec<PathBuf>> {
    update_pins(&pins_file()?, path, pinned)
}

fn update_pins(file: &Path, path: &Path, pinned: bool) -> io::Result<Vec<PathBuf>> {
    fs::create_dir_all(file.parent().unwrap())?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(file.with_extension("lock"))?;
    lock.lock()?;

    let mut pins = read_pins(file)?;
    if pinned && !pins.iter().any(|existing| existing == path) {
        pins.push(path.to_path_buf());
    } else if !pinned {
        pins.retain(|existing| existing != path);
    }
    // NUL conserva rutas con saltos de línea o bytes no UTF-8.
    let bytes: Vec<_> = pins
        .iter()
        .flat_map(|pin| pin.as_os_str().as_bytes().iter().copied().chain([0]))
        .collect();
    let pending = file.with_extension("tmp");
    fs::write(&pending, bytes)?;
    fs::rename(pending, file)?;
    Ok(pins)
}

#[cfg(target_os = "linux")]
pub fn mounted_disks() -> io::Result<Vec<PathBuf>> {
    fs::read("/proc/self/mountinfo").map(|contents| parse_mounts(&contents))
}

#[cfg(target_os = "macos")]
pub use crate::macos::mounted_disks;

#[cfg(target_os = "linux")]
fn parse_mounts(contents: &[u8]) -> Vec<PathBuf> {
    let mut disks = contents
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let mut fields = line.split(|byte| *byte == b' ');
            let mount_root = fields.nth(3)?;
            let target = decode_mount_path(fields.next()?);
            let filesystem = fields
                .find(|field| *field == b"-")
                .and_then(|_| fields.next())?;
            let source = fields.next()?;
            if !target.is_absolute()
                || target == Path::new("/")
                || [b"tmpfs".as_slice(), b"autofs"].contains(&filesystem)
            {
                return None;
            }
            let removable = ["/media", "/run/media", "/mnt"]
                .iter()
                .any(|root| target != Path::new(root) && target.starts_with(root));
            let system = ["/boot", "/dev", "/proc", "/run", "/snap", "/sys", "/var"]
                .iter()
                .any(|root| target.starts_with(root));
            (removable || (source.starts_with(b"/dev/") && mount_root == b"/" && !system))
                .then_some(target)
        })
        .collect::<Vec<_>>();
    disks.sort_unstable();
    disks.dedup();
    disks
}

#[cfg(target_os = "linux")]
fn decode_mount_path(mut encoded: &[u8]) -> PathBuf {
    let mut bytes = Vec::with_capacity(encoded.len());
    while let Some((&byte, rest)) = encoded.split_first() {
        let escaped = match rest.get(..3) {
            Some(b"040") => Some(b' '),
            Some(b"011") => Some(b'\t'),
            Some(b"012") => Some(b'\n'),
            Some(b"134") => Some(b'\\'),
            _ => None,
        };
        if byte == b'\\'
            && let Some(decoded) = escaped
        {
            bytes.push(decoded);
            encoded = &rest[3..];
        } else {
            bytes.push(byte);
            encoded = rest;
        }
    }
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn mounted_disks_use_decoded_paths_and_skip_system_mounts() {
        let entries = br"1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 8:2 / /media/USB\040Drive rw shared:9 - exfat /dev/sdb1 rw
3 1 8:3 / /data rw - ext4 /dev/sdc1 rw
4 1 8:4 / /boot rw - ext4 /dev/sdd1 rw
5 1 0:1 / /run/user rw - tmpfs tmpfs rw
6 1 0:2 / /run/media/me/Shared\134Drive rw - fuse.rclone remote rw
7 1 8:1 /subdir /var/cache rw - ext4 /dev/sda1 rw
8 1 8:2 / /media/USB\040Drive rw - exfat /dev/sdb1 rw";
        assert_eq!(
            parse_mounts(entries),
            [
                PathBuf::from("/data"),
                PathBuf::from("/media/USB Drive"),
                PathBuf::from("/run/media/me/Shared\\Drive"),
            ]
        );
    }

    #[test]
    fn pinned_paths_survive_save_and_remove() {
        let dir = std::env::temp_dir().join(format!("fex-pins-test-{}", std::process::id()));
        let file = dir.join("pins");
        let odd = PathBuf::from(OsString::from_vec(b"/fex/new\nline\xff".to_vec()));
        assert_eq!(update_pins(&file, &odd, true).unwrap(), [odd.clone()]);
        assert_eq!(update_pins(&file, &odd, true).unwrap(), [odd.clone()]);
        assert_eq!(read_pins(&file).unwrap(), [odd.clone()]);
        assert!(update_pins(&file, &odd, false).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}
