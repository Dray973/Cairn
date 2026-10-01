//! Registry access: `RegOpenKeyExW` / `RegCreateKeyExW` / `RegQueryValueExW` /
//! `RegSetValueExW` / `RegDeleteValueW` / `RegDeleteKeyExW`, always through the
//! 64-bit view (`KEY_WOW64_64KEY`).

use serde::{Deserialize, Serialize};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS};
#[cfg(test)]
use windows::Win32::System::Registry::RegDeleteTreeW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyExW, RegDeleteValueW, RegEnumKeyExW, RegOpenKeyExW,
    RegQueryInfoKeyW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_READ, KEY_WOW64_64KEY, KEY_WRITE, REG_BINARY,
    REG_CREATED_NEW_KEY, REG_CREATE_KEY_DISPOSITION, REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ,
    REG_OPTION_NON_VOLATILE, REG_QWORD, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
};

use super::{check, wide};
use crate::{Error, Result};

// ───────────────────────────── Hive ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Hive {
    #[serde(rename = "HKLM")]
    LocalMachine,
    #[serde(rename = "HKCU")]
    CurrentUser,
    #[serde(rename = "HKCR")]
    ClassesRoot,
    #[serde(rename = "HKU")]
    Users,
}

impl Hive {
    pub fn raw(self) -> HKEY {
        match self {
            Hive::LocalMachine => HKEY_LOCAL_MACHINE,
            Hive::CurrentUser => HKEY_CURRENT_USER,
            Hive::ClassesRoot => HKEY_CLASSES_ROOT,
            Hive::Users => HKEY_USERS,
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Hive::LocalMachine => "HKLM",
            Hive::CurrentUser => "HKCU",
            Hive::ClassesRoot => "HKCR",
            Hive::Users => "HKU",
        }
    }

    pub fn parse(s: &str) -> Option<Hive> {
        match s.trim().to_ascii_uppercase().as_str() {
            "HKLM" | "HKEY_LOCAL_MACHINE" => Some(Hive::LocalMachine),
            "HKCU" | "HKEY_CURRENT_USER" => Some(Hive::CurrentUser),
            "HKCR" | "HKEY_CLASSES_ROOT" => Some(Hive::ClassesRoot),
            "HKU" | "HKEY_USERS" => Some(Hive::Users),
            _ => None,
        }
    }

    /// Writing anywhere but HKCU needs an elevated token. This ignores the key path; HKCU
    /// has a read-only branch as well, so decisions about a concrete key use
    /// [`write_requires_elevation`].
    pub fn requires_elevation(self) -> bool {
        !matches!(self, Hive::CurrentUser)
    }
}

/// HKCU branch whose DACL grants the user read access only (Administrators and SYSTEM
/// hold full control, and subkeys inherit that), so writing below it needs elevation.
const USER_POLICIES: &str = r"Software\Policies";

/// True when writing under `key_path` in `hive` needs an elevated token: every hive but
/// HKCU, and HKCU\Software\Policies with everything below it.
pub fn write_requires_elevation(hive: Hive, key_path: &str) -> bool {
    match hive {
        Hive::CurrentUser => {
            let path = key_path.trim_start_matches(['\\', '/']).as_bytes();
            let prefix = USER_POLICIES.as_bytes();
            path.len() >= prefix.len()
                && path[..prefix.len()]
                    .iter()
                    .zip(prefix)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b) || (*a == b'/' && *b == b'\\'))
                && matches!(path.get(prefix.len()), None | Some(b'\\' | b'/'))
        }
        _ => true,
    }
}

/// True when `path` is `root` or lies below it. Components are separated by `\` and
/// compared ignoring ASCII case, as the registry does.
pub fn path_within(path: &str, root: &str) -> bool {
    let (path, root) = (path.as_bytes(), root.as_bytes());
    if root.is_empty() || path.len() < root.len() {
        return false;
    }
    path[..root.len()].eq_ignore_ascii_case(root)
        && (path.len() == root.len() || path[root.len()] == b'\\')
}

/// Splits `HKLM\SOFTWARE\Foo` into `(Hive::LocalMachine, "SOFTWARE\Foo")`.
pub fn split_path(full: &str) -> Result<(Hive, String)> {
    let full = full.trim().trim_start_matches(['\\', '/']);
    let (hive, rest) = full
        .split_once(['\\', '/'])
        .ok_or_else(|| Error::InvalidPath(full.to_string()))?;
    let hive = Hive::parse(hive).ok_or_else(|| Error::InvalidPath(full.to_string()))?;
    Ok((hive, rest.replace('/', "\\")))
}

// ───────────────────────────── Values ─────────────────────────────

/// Exact on-disk representation of a value. This is what the journal stores so a
/// rollback is byte-for-byte faithful, including odd types and trailing NULs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawValue {
    pub kind: u32,
    #[serde(with = "hex_bytes")]
    pub data: Vec<u8>,
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() % 2 != 0 {
            return Err(serde::de::Error::custom("odd-length hex"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect()
    }
}

impl RawValue {
    pub fn decode(&self) -> RegValue {
        RegValue::from_raw(self.kind, &self.data)
    }
}

/// Typed registry value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum RegValue {
    Dword(u32),
    Qword(u64),
    Sz(String),
    ExpandSz(String),
    MultiSz(Vec<String>),
    Binary(Vec<u8>),
    Other { kind: u32, data: Vec<u8> },
}

impl RegValue {
    pub fn kind(&self) -> u32 {
        match self {
            RegValue::Dword(_) => REG_DWORD.0,
            RegValue::Qword(_) => REG_QWORD.0,
            RegValue::Sz(_) => REG_SZ.0,
            RegValue::ExpandSz(_) => REG_EXPAND_SZ.0,
            RegValue::MultiSz(_) => REG_MULTI_SZ.0,
            RegValue::Binary(_) => REG_BINARY.0,
            RegValue::Other { kind, .. } => *kind,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            RegValue::Dword(v) => v.to_le_bytes().to_vec(),
            RegValue::Qword(v) => v.to_le_bytes().to_vec(),
            RegValue::Sz(s) | RegValue::ExpandSz(s) => utf16z(s),
            RegValue::MultiSz(items) => {
                let mut out = Vec::new();
                for s in items {
                    out.extend(utf16z(s));
                }
                out.extend([0u8, 0u8]); // list terminator
                out
            }
            RegValue::Binary(b) => b.clone(),
            RegValue::Other { data, .. } => data.clone(),
        }
    }

    pub fn to_raw(&self) -> RawValue {
        RawValue {
            kind: self.kind(),
            data: self.to_bytes(),
        }
    }

    pub fn from_raw(kind: u32, data: &[u8]) -> RegValue {
        match REG_VALUE_TYPE(kind) {
            REG_DWORD if data.len() >= 4 => {
                RegValue::Dword(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
            }
            REG_QWORD if data.len() >= 8 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&data[..8]);
                RegValue::Qword(u64::from_le_bytes(b))
            }
            REG_SZ => RegValue::Sz(from_utf16z(data)),
            REG_EXPAND_SZ => RegValue::ExpandSz(from_utf16z(data)),
            REG_MULTI_SZ => RegValue::MultiSz(from_multi_utf16z(data)),
            REG_BINARY => RegValue::Binary(data.to_vec()),
            _ => RegValue::Other {
                kind,
                data: data.to_vec(),
            },
        }
    }

    /// Short human-readable form for logs and the UI.
    pub fn display(&self) -> String {
        match self {
            RegValue::Dword(v) => format!("0x{v:08x} ({v})"),
            RegValue::Qword(v) => format!("0x{v:016x} ({v})"),
            RegValue::Sz(s) | RegValue::ExpandSz(s) => format!("\"{s}\""),
            RegValue::MultiSz(v) => format!("{v:?}"),
            RegValue::Binary(b) => format!("<{} bytes>", b.len()),
            RegValue::Other { kind, data } => format!("<type {kind}, {} bytes>", data.len()),
        }
    }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn utf16_units(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn from_utf16z(data: &[u8]) -> String {
    let units = utf16_units(data);
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

fn from_multi_utf16z(data: &[u8]) -> Vec<String> {
    let units = utf16_units(data);
    units
        .split(|&u| u == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

// ───────────────────────────── Key handle ─────────────────────────────

fn sam(write: bool) -> REG_SAM_FLAGS {
    if write {
        KEY_READ | KEY_WRITE | KEY_WOW64_64KEY
    } else {
        KEY_READ | KEY_WOW64_64KEY
    }
}

/// An open registry key. Closed on drop.
#[derive(Debug)]
pub struct Key {
    h: HKEY,
    pub hive: Hive,
    pub path: String,
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: handle was returned by RegOpenKeyExW/RegCreateKeyExW and is closed exactly once.
        unsafe {
            let _ = RegCloseKey(self.h);
        }
    }
}

impl Key {
    /// Opens an existing key. Returns `Ok(None)` when it does not exist.
    pub fn open(hive: Hive, path: &str, write: bool) -> Result<Option<Key>> {
        let w = wide(path);
        let mut h = HKEY::default();
        // SAFETY: all pointers are valid for the duration of the call.
        let err =
            unsafe { RegOpenKeyExW(hive.raw(), PCWSTR(w.as_ptr()), None, sam(write), &mut h) };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(err)?;
        Ok(Some(Key {
            h,
            hive,
            path: path.to_string(),
        }))
    }

    /// Opens or creates a key (non-volatile). Second element is `true` when it was created.
    pub fn create(hive: Hive, path: &str) -> Result<(Key, bool)> {
        let w = wide(path);
        let mut h = HKEY::default();
        let mut disp = REG_CREATE_KEY_DISPOSITION::default();
        // SAFETY: as above.
        let err = unsafe {
            RegCreateKeyExW(
                hive.raw(),
                PCWSTR(w.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                sam(true),
                None,
                &mut h,
                Some(&mut disp),
            )
        };
        check(err)?;
        Ok((
            Key {
                h,
                hive,
                path: path.to_string(),
            },
            disp == REG_CREATED_NEW_KEY,
        ))
    }

    /// Raw bytes + type of a value. `Ok(None)` if the value is absent. Empty name = default value.
    pub fn query_raw(&self, name: &str) -> Result<Option<RawValue>> {
        let w = wide(name);
        let mut kind = REG_VALUE_TYPE(0);
        let mut len: u32 = 0;
        // SAFETY: size probe; lpdata None is permitted.
        let err = unsafe {
            RegQueryValueExW(
                self.h,
                PCWSTR(w.as_ptr()),
                None,
                Some(&mut kind),
                None,
                Some(&mut len),
            )
        };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(err)?;
        loop {
            let mut buf = vec![0u8; len as usize];
            let mut cb = len;
            // SAFETY: buf has exactly cb bytes available.
            let err = unsafe {
                RegQueryValueExW(
                    self.h,
                    PCWSTR(w.as_ptr()),
                    None,
                    Some(&mut kind),
                    Some(buf.as_mut_ptr()),
                    Some(&mut cb),
                )
            };
            if err == ERROR_MORE_DATA {
                len = cb; // value grew between calls; retry with the new size
                continue;
            }
            check(err)?;
            buf.truncate(cb as usize);
            return Ok(Some(RawValue {
                kind: kind.0,
                data: buf,
            }));
        }
    }

    pub fn query(&self, name: &str) -> Result<Option<RegValue>> {
        Ok(self.query_raw(name)?.map(|r| r.decode()))
    }

    /// `(REG_* type, data size in bytes)` of a value, read with a null data pointer so the
    /// data itself is never read. `Ok(None)` when the value is absent. Empty name = default
    /// value.
    pub fn value_info(&self, name: &str) -> Result<Option<(u32, u32)>> {
        let w = wide(name);
        let mut kind = REG_VALUE_TYPE(0);
        let mut len: u32 = 0;
        // SAFETY: size and type probe; lpdata None is permitted and nothing is copied.
        let err = unsafe {
            RegQueryValueExW(
                self.h,
                PCWSTR(w.as_ptr()),
                None,
                Some(&mut kind),
                None,
                Some(&mut len),
            )
        };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(err)?;
        Ok(Some((kind.0, len)))
    }

    pub fn set_raw(&self, name: &str, kind: u32, data: &[u8]) -> Result<()> {
        let w = wide(name);
        // SAFETY: data slice is valid for the call.
        let err = unsafe {
            RegSetValueExW(
                self.h,
                PCWSTR(w.as_ptr()),
                None,
                REG_VALUE_TYPE(kind),
                Some(data),
            )
        };
        check(err)
    }

    pub fn set(&self, name: &str, value: &RegValue) -> Result<()> {
        self.set_raw(name, value.kind(), &value.to_bytes())
    }

    /// Deletes a value. Returns `false` if it was already absent.
    pub fn delete_value(&self, name: &str) -> Result<bool> {
        let w = wide(name);
        // SAFETY: valid handle and string.
        let err = unsafe { RegDeleteValueW(self.h, PCWSTR(w.as_ptr())) };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        check(err)?;
        Ok(true)
    }

    /// True when the key has neither subkeys nor values.
    pub fn is_empty(&self) -> Result<bool> {
        let mut subkeys = 0u32;
        let mut values = 0u32;
        // SAFETY: only the two counters we care about are requested.
        let err = unsafe {
            RegQueryInfoKeyW(
                self.h,
                None,
                None,
                None,
                Some(&mut subkeys),
                None,
                None,
                Some(&mut values),
                None,
                None,
                None,
                None,
            )
        };
        check(err)?;
        Ok(subkeys == 0 && values == 0)
    }
}

/// Deletes `path` under `hive` only if it is completely empty. Returns whether it was deleted.
pub fn delete_key_if_empty(hive: Hive, path: &str) -> Result<bool> {
    let Some(key) = Key::open(hive, path, false)? else {
        return Ok(false);
    };
    if !key.is_empty()? {
        return Ok(false);
    }
    drop(key);
    let w = wide(path);
    // SAFETY: valid string; KEY_WOW64_64KEY selects the 64-bit view.
    let err = unsafe { RegDeleteKeyExW(hive.raw(), PCWSTR(w.as_ptr()), KEY_WOW64_64KEY.0, None) };
    if err == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    check(err)?;
    Ok(true)
}

pub fn exists(hive: Hive, path: &str) -> Result<bool> {
    Ok(Key::open(hive, path, false)?.is_some())
}

/// Root of the per-user test sandbox; tests write registry values only below it.
#[cfg(test)]
pub(crate) const SANDBOX_ROOT: &str = r"Software\PCOptimizer\SelfTest";

/// Deletes the HKCU key `path` with all its subkeys and values (`RegDeleteTreeW`). Only
/// paths below [`SANDBOX_ROOT`] are accepted (anything else, the root itself included,
/// panics); a missing key is `Ok`.
#[cfg(test)]
pub(crate) fn delete_sandbox_tree(path: &str) -> Result<()> {
    assert!(
        path_within(path, SANDBOX_ROOT) && path.len() > SANDBOX_ROOT.len(),
        "{path} is outside the test sandbox"
    );
    let w = wide(path);
    // SAFETY: valid NUL-terminated string; HKCU\Software is not redirected in a 64-bit
    // process, so the 64-bit view is the one deleted.
    let err = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(w.as_ptr())) };
    if err == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(err)
}

/// Convenience read: `Ok(None)` when key or value is absent.
pub fn read_value(hive: Hive, path: &str, name: &str) -> Result<Option<RegValue>> {
    match Key::open(hive, path, false)? {
        Some(k) => k.query(name),
        None => Ok(None),
    }
}

/// Longest key name the registry allows, in UTF-16 units, plus the terminating NUL.
const MAX_KEY_NAME: usize = 256;

/// Names of the direct subkeys of `path`, in registry order. Empty when the key does not
/// exist. Read-only.
pub fn subkey_names(hive: Hive, path: &str) -> Result<Vec<String>> {
    let Some(key) = Key::open(hive, path, false)? else {
        return Ok(Vec::new());
    };
    let mut name_buf = vec![0u16; MAX_KEY_NAME];
    let mut out = Vec::new();
    let mut index = 0u32;
    loop {
        let mut name_len = name_buf.len() as u32;
        // SAFETY: `name_buf` is writable for the length passed alongside it; every other
        // out parameter is omitted.
        let err = unsafe {
            RegEnumKeyExW(
                key.h,
                index,
                Some(PWSTR(name_buf.as_mut_ptr())),
                &mut name_len,
                None,
                None,
                None,
                None,
            )
        };
        if err == ERROR_NO_MORE_ITEMS {
            break;
        }
        check(err)?;
        index += 1;
        out.push(String::from_utf16_lossy(&name_buf[..name_len as usize]));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_roundtrip() {
        for v in [
            RegValue::Dword(0xDEAD_BEEF),
            RegValue::Qword(u64::MAX - 1),
            RegValue::Sz("héllo".into()),
            RegValue::ExpandSz("%SystemRoot%\\x".into()),
            RegValue::MultiSz(vec!["a".into(), "bb".into()]),
            RegValue::Binary(vec![1, 2, 3]),
        ] {
            let raw = v.to_raw();
            assert_eq!(raw.decode(), v);
        }
    }

    #[test]
    fn split_paths() {
        let (h, p) = split_path(r"HKLM\SOFTWARE\Policies").unwrap();
        assert_eq!(h, Hive::LocalMachine);
        assert_eq!(p, r"SOFTWARE\Policies");
        let (h, p) = split_path("HKEY_CURRENT_USER/Software/X").unwrap();
        assert_eq!(h, Hive::CurrentUser);
        assert_eq!(p, r"Software\X");
        assert!(split_path("NOPE").is_err());
    }

    #[test]
    fn elevation_depends_on_hive_and_path() {
        let hkcu = |path: &str| write_requires_elevation(Hive::CurrentUser, path);
        assert!(hkcu(r"Software\Policies\Microsoft\Windows\Explorer"));
        assert!(hkcu(r"software\policies"));
        assert!(hkcu(r"SOFTWARE\POLICIES\"));
        assert!(hkcu(r"\Software\Policies\Microsoft"));
        assert!(hkcu("Software/Policies/Microsoft"));
        assert!(!hkcu(r"Software\PoliciesX\Foo"));
        assert!(!hkcu(r"Software\Microsoft\Siuf\Rules"));
        assert!(!hkcu(r"Software\Microsoft\Windows\CurrentVersion\Run"));
        assert!(!hkcu("Software"));
        assert!(!hkcu(""));
        for hive in [Hive::LocalMachine, Hive::ClassesRoot, Hive::Users] {
            assert!(write_requires_elevation(hive, r"Software\Anything"));
            assert!(write_requires_elevation(hive, ""));
        }
    }

    #[test]
    fn path_containment() {
        assert!(path_within(r"Software\A\B", r"Software\A"));
        assert!(path_within(r"software\a\b", r"Software\A"));
        assert!(path_within(r"Software\A", r"SOFTWARE\a"));
        assert!(!path_within(r"Software\AB", r"Software\A"));
        assert!(!path_within(r"Software", r"Software\A"));
        assert!(!path_within(r"Software\A", ""));
        assert!(path_within("Kéy\\Sub", "Kéy"));
        assert!(!path_within("Ké", "Kéy"));
    }

    #[test]
    fn subkey_names_lists_children_and_tolerates_a_missing_key() {
        let names = subkey_names(Hive::LocalMachine, "SOFTWARE").unwrap();
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("Microsoft")));
        assert!(
            subkey_names(Hive::CurrentUser, r"Software\PCOptimizerNoSuchKey")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn value_info_reads_type_and_size_without_the_data() {
        let key = Key::open(Hive::CurrentUser, r"Control Panel\Mouse", false)
            .unwrap()
            .expect("the mouse settings key exists");
        let (kind, size) = key.value_info("MouseSpeed").unwrap().expect("MouseSpeed");
        assert_eq!(kind, REG_SZ.0);
        assert!(size >= 2, "{size}");
        assert_eq!(key.value_info("PCOptimizerNoSuchValue").unwrap(), None);
    }

    #[test]
    fn sandbox_tree_is_deleted_with_its_subkeys() {
        let root = format!(r"{SANDBOX_ROOT}\DeleteTree{}", std::process::id());
        let (sub, _) = Key::create(Hive::CurrentUser, &format!(r"{root}\A\B")).unwrap();
        sub.set("Value", &RegValue::Dword(1)).unwrap();
        drop(sub);
        assert!(exists(Hive::CurrentUser, &root).unwrap());
        delete_sandbox_tree(&root).unwrap();
        assert!(!exists(Hive::CurrentUser, &root).unwrap());
        // Missing is fine.
        delete_sandbox_tree(&root).unwrap();
    }

    #[test]
    #[should_panic(expected = "outside the test sandbox")]
    fn sandbox_tree_refuses_paths_outside_the_sandbox() {
        let _ = delete_sandbox_tree(r"Software\PCOptimizer");
    }

    #[test]
    #[should_panic(expected = "outside the test sandbox")]
    fn sandbox_tree_refuses_the_sandbox_root_itself() {
        let _ = delete_sandbox_tree(SANDBOX_ROOT);
    }

    #[test]
    fn raw_hex_serde() {
        let raw = RawValue {
            kind: 4,
            data: vec![1, 0, 0, 0],
        };
        let s = serde_json::to_string(&raw).unwrap();
        assert!(s.contains("01000000"));
        let back: RawValue = serde_json::from_str(&s).unwrap();
        assert_eq!(back, raw);
    }
}
