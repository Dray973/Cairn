//! Security descriptor checks for program files, folders and scheduled tasks.
//!
//! An elevated or unattended process may only run code that no standard user can change.
//! [`untrusted_writer`] judges one object's owner and DACL for a role; [`program_location_problem`]
//! and [`install_location_problem`] apply it to a program, the files it loads, its folder and
//! every folder above it. Only SYSTEM, Administrators and TrustedInstaller ([`TRUSTED_SIDS`])
//! may hold rights that change an object.
//!
//! The rules err on the safe side: an unknown owner, a NULL DACL and an allow ACE whose type
//! is not understood are all unsafe; deny ACEs and inherit-only ACEs (which grant nothing on
//! the object itself) are ignored.

use std::path::{Component, Path, Prefix};
use std::ptr;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetAce, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, ACE_HEADER, ACL,
    DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
use windows_core::BOOL;

use super::session::sid_string;
use super::{check, wide};
use crate::{Error, Result};

/// How the rights of an allow ACE are judged for one object in a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclRole {
    /// A program or a file it loads: nobody else may write, delete or re-permission it.
    File,
    /// The folder holding a program: nobody else may add, replace or delete its entries.
    Folder,
    /// A folder above the program's folder: nobody else may rename, delete or replace it.
    Ancestor,
    /// A registered scheduled task, judged like [`AclRole::File`].
    TaskObject,
    /// A Task Scheduler folder, judged like [`AclRole::Folder`].
    TaskFolder,
}

/// One ACE of a DACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AceInfo {
    /// Any ACE type that is not a deny type counts as allowing.
    pub allow: bool,
    /// The type is `ACCESS_ALLOWED_ACE_TYPE` or `ACCESS_DENIED_ACE_TYPE`, whose mask and SID
    /// are read; for other types both are left empty.
    pub known_type: bool,
    /// `ACE_HEADER::AceFlags` (`INHERIT_ONLY_ACE` is 0x08).
    pub flags: u8,
    pub mask: u32,
    /// `S-1-…` of the trustee; empty for an ACE of unknown type.
    pub sid: String,
}

/// The parts of a security descriptor the checks read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityInfo {
    /// `S-1-…` of the owner; `None` when the descriptor names none.
    pub owner: Option<String>,
    /// The DACL's ACEs in order; `None` for a NULL or absent DACL, which grants everyone
    /// full access.
    pub dacl: Option<Vec<AceInfo>>,
}

/// SIDs that may write: SYSTEM, Administrators, TrustedInstaller.
pub const TRUSTED_SIDS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x0;
const ACCESS_DENIED_ACE_TYPE: u8 = 0x1;
const ACCESS_DENIED_OBJECT_ACE_TYPE: u8 = 0x6;
const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 0xA;
const ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE: u8 = 0xC;
const INHERIT_ONLY_ACE: u8 = 0x08;

const WRITE_DATA: u32 = 0x2; // FILE_WRITE_DATA / FILE_ADD_FILE
const APPEND_DATA: u32 = 0x4; // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
const DELETE_CHILD: u32 = 0x40;
const DELETE: u32 = 0x0001_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// Rights that let a trustee change a file or task.
const FILE_DANGEROUS: u32 =
    WRITE_DATA | APPEND_DATA | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL;
/// Rights that let a trustee add, replace or delete a folder's entries.
const FOLDER_DANGEROUS: u32 = WRITE_DATA
    | APPEND_DATA
    | DELETE_CHILD
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;
/// Rights that let a trustee rename, delete or replace a folder above the program's one;
/// creating new entries in it (`C:\` grants Authenticated Users that) is harmless.
const ANCESTOR_DANGEROUS: u32 = DELETE | DELETE_CHILD | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;

impl AclRole {
    fn dangerous_mask(self) -> u32 {
        match self {
            AclRole::File | AclRole::TaskObject => FILE_DANGEROUS,
            AclRole::Folder | AclRole::TaskFolder => FOLDER_DANGEROUS,
            AclRole::Ancestor => ANCESTOR_DANGEROUS,
        }
    }

    fn what_they_can_do(self) -> &'static str {
        match self {
            AclRole::File | AclRole::TaskObject => "can change it",
            AclRole::Folder | AclRole::TaskFolder => "can add, replace or delete files in it",
            AclRole::Ancestor => "can rename or replace it",
        }
    }
}

fn is_trusted(sid: &str) -> bool {
    TRUSTED_SIDS.iter().any(|t| t.eq_ignore_ascii_case(sid))
}

/// A readable name for well-known SIDs, with the SID; other SIDs as they are.
fn describe_sid(sid: &str) -> String {
    let name = match sid.to_ascii_uppercase().as_str() {
        "S-1-1-0" => "Everyone",
        "S-1-3-0" => "CREATOR OWNER",
        "S-1-3-4" => "OWNER RIGHTS",
        "S-1-5-4" => "Interactive users",
        "S-1-5-11" => "Authenticated Users",
        "S-1-5-19" => "LOCAL SERVICE",
        "S-1-5-20" => "NETWORK SERVICE",
        "S-1-5-32-545" => "Users",
        "S-1-5-32-547" => "Power Users",
        "S-1-15-2-1" => "ALL APPLICATION PACKAGES",
        _ => return sid.to_string(),
    };
    format!("{name} ({sid})")
}

/// Pure: `None` when only [`TRUSTED_SIDS`] can change the object in `role`, else the reason
/// (a sentence fragment about "it").
///
/// The owner must be trusted, since an owner may always rewrite the DACL. A NULL DACL is
/// unsafe. Inherit-only and deny ACEs are ignored; an allow ACE of unknown type is unsafe;
/// an allow ACE for an untrusted SID is unsafe when its mask holds a right that is dangerous
/// for `role`.
pub fn untrusted_writer(info: &SecurityInfo, role: AclRole) -> Option<String> {
    match &info.owner {
        None => return Some("its owner is not known".to_string()),
        Some(owner) if !is_trusted(owner) => {
            return Some(format!(
                "its owner {} is not SYSTEM, Administrators or TrustedInstaller",
                describe_sid(owner)
            ))
        }
        Some(_) => {}
    }
    let Some(dacl) = &info.dacl else {
        return Some("it has no access control list, so anyone can change it".to_string());
    };
    for ace in dacl {
        if !ace.allow || ace.flags & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        if !ace.known_type {
            return Some(
                "an access rule of a type this check does not know may let others change it"
                    .to_string(),
            );
        }
        if ace.mask & role.dangerous_mask() != 0 && !is_trusted(&ace.sid) {
            return Some(format!(
                "{} {}",
                describe_sid(&ace.sid),
                role.what_they_can_do()
            ));
        }
    }
    None
}

/// Owner and DACL of a file or folder (`GetNamedSecurityInfoW`). A reparse point is followed.
pub fn file_security(path: &Path) -> Result<SecurityInfo> {
    let name = wide(&path.to_string_lossy());
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `name` is NUL-terminated; only the descriptor is requested, which is freed
    // below with LocalFree.
    let err = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &mut sd,
        )
    };
    let descriptor = LocalDescriptor(sd);
    check(err)?;
    from_descriptor(descriptor.0)
}

/// Owner and DACL of an SDDL string (`ConvertStringSecurityDescriptorToSecurityDescriptorW`).
pub fn sddl_security(sddl: &str) -> Result<SecurityInfo> {
    let text = wide(sddl);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `text` is NUL-terminated and `sd` a valid out pointer; the descriptor is freed
    // with LocalFree when `descriptor` drops.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(text.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?
    };
    let descriptor = LocalDescriptor(sd);
    from_descriptor(descriptor.0)
}

/// A security descriptor allocated with LocalAlloc by the system, freed on drop.
struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            // SAFETY: the descriptor was allocated by the API that returned it and is freed
            // exactly once.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0 .0)));
            }
        }
    }
}

/// Reads the owner and DACL of a valid security descriptor.
fn from_descriptor(sd: PSECURITY_DESCRIPTOR) -> Result<SecurityInfo> {
    let mut owner = PSID::default();
    let mut owner_defaulted = BOOL::default();
    // SAFETY: `sd` is a valid descriptor that outlives this function's reads; the owner
    // pointer refers into it.
    unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut owner_defaulted)? };
    let owner = if owner.is_invalid() {
        None
    } else {
        Some(sid_string(owner)?)
    };
    let mut present = BOOL::default();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut dacl_defaulted = BOOL::default();
    // SAFETY: as above; the DACL pointer refers into the descriptor.
    unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut dacl_defaulted)? };
    let dacl = if !present.as_bool() || dacl.is_null() {
        None
    } else {
        Some(read_acl(dacl)?)
    };
    Ok(SecurityInfo { owner, dacl })
}

/// The ACEs of an ACL in order.
fn read_acl(acl: *const ACL) -> Result<Vec<AceInfo>> {
    // SAFETY: `acl` points to a valid ACL inside a live security descriptor.
    let count = unsafe { (*acl).AceCount };
    let mut aces = Vec::with_capacity(count as usize);
    for index in 0..u32::from(count) {
        let mut raw: *mut std::ffi::c_void = ptr::null_mut();
        // SAFETY: `index` is below the ACL's ACE count; `raw` receives a pointer into the ACL.
        unsafe { GetAce(acl, index, &mut raw)? };
        if raw.is_null() {
            return Err(Error::Other(format!("ACE {index} of an ACL is missing")));
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { ptr::read_unaligned(raw as *const ACE_HEADER) };
        let kind = header.AceType;
        let allow = !matches!(
            kind,
            ACCESS_DENIED_ACE_TYPE
                | ACCESS_DENIED_OBJECT_ACE_TYPE
                | ACCESS_DENIED_CALLBACK_ACE_TYPE
                | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE
        );
        let known_type = matches!(kind, ACCESS_ALLOWED_ACE_TYPE | ACCESS_DENIED_ACE_TYPE);
        let (mask, sid) = if known_type && usize::from(header.AceSize) >= 8 + 8 {
            // ACCESS_ALLOWED_ACE and ACCESS_DENIED_ACE: header, 4-byte mask, then the SID.
            // SAFETY: the ACE is at least 16 bytes long, which covers the mask and the fixed
            // part of the SID that follows it.
            let mask = unsafe { ptr::read_unaligned((raw as *const u8).add(4) as *const u32) };
            // SAFETY: the SID starts at offset 8 inside the ACE, which lives in the ACL.
            let sid = sid_string(PSID(unsafe { (raw as *mut u8).add(8) } as *mut _))?;
            (mask, sid)
        } else {
            (0, String::new())
        };
        aces.push(AceInfo {
            allow,
            known_type: known_type && !sid.is_empty(),
            flags: header.AceFlags,
            mask,
            sid,
        });
    }
    Ok(aces)
}

/// `None` when `program` and every folder above it can be changed only by trusted SIDs, else
/// the first problem found:
///
/// 1. `program` must be an absolute path to an existing file;
/// 2. its canonical path (`\\?\` removed) must equal the spelled path ignoring case, so no
///    component is a link;
/// 3. its drive must be a fixed local drive;
/// 4. the program passes the [`AclRole::File`] check, its folder the [`AclRole::Folder`]
///    check, and every folder above that the [`AclRole::Ancestor`] check.
///
/// Fails only when a security descriptor cannot be read.
pub fn program_location_problem(program: &Path) -> Result<Option<String>> {
    if !program.is_absolute() {
        return Ok(Some(format!(
            "{} is not an absolute path",
            program.display()
        )));
    }
    if !program.is_file() {
        return Ok(Some(format!("{} does not exist", program.display())));
    }
    if let Some(problem) = link_problem(program) {
        return Ok(Some(problem));
    }
    let Some(root) = drive_root(program) else {
        return Ok(Some(format!(
            "{} is not on a local drive",
            program.display()
        )));
    };
    let root_w = wide(&root);
    // SAFETY: `root_w` is a NUL-terminated root path such as `C:\`.
    if unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) } != DRIVE_FIXED {
        return Ok(Some(format!(
            "{} is not on a fixed local drive",
            program.display()
        )));
    }
    if let Some(problem) = object_problem(program, AclRole::File)? {
        return Ok(Some(problem));
    }
    let mut ancestors = program.ancestors().skip(1);
    if let Some(folder) = ancestors.next() {
        if let Some(problem) = object_problem(folder, AclRole::Folder)? {
            return Ok(Some(problem));
        }
    }
    for folder in ancestors {
        if let Some(problem) = object_problem(folder, AclRole::Ancestor)? {
            return Ok(Some(problem));
        }
    }
    Ok(None)
}

/// [`program_location_problem`] of `program`; then, relative to the program's folder, the
/// [`AclRole::File`] check of every existing `companions` file and the [`AclRole::Folder`]
/// check (of the folder itself, not its contents) of every existing `folders` folder, each
/// also refused when it goes through a link. `None` when all pass.
pub fn install_location_problem(
    program: &Path,
    companions: &[&str],
    folders: &[&str],
) -> Result<Option<String>> {
    if let Some(problem) = program_location_problem(program)? {
        return Ok(Some(problem));
    }
    let Some(dir) = program.parent() else {
        return Ok(Some(format!("{} has no folder", program.display())));
    };
    let entries = companions
        .iter()
        .map(|c| (dir.join(c), AclRole::File))
        .chain(folders.iter().map(|f| (dir.join(f), AclRole::Folder)));
    for (path, role) in entries {
        if !path.exists() {
            continue;
        }
        if let Some(problem) = link_problem(&path) {
            return Ok(Some(problem));
        }
        if let Some(problem) = object_problem(&path, role)? {
            return Ok(Some(problem));
        }
    }
    Ok(None)
}

/// The [`untrusted_writer`] reason for `path`, prefixed with the path.
fn object_problem(path: &Path, role: AclRole) -> Result<Option<String>> {
    let info = file_security(path)?;
    Ok(untrusted_writer(&info, role).map(|reason| format!("{}: {reason}", path.display())))
}

/// `Some` when the canonical form of `path` is not the spelled path (ignoring case): a
/// component is a symbolic link, junction or other name for another place.
fn link_problem(path: &Path) -> Option<String> {
    let resolved = match std::fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(e) => return Some(format!("{} cannot be resolved: {e}", path.display())),
    };
    if same_spelling(&resolved, path) {
        None
    } else {
        Some(format!("{} goes through a link", path.display()))
    }
}

/// True when `canonical` (with its `\\?\` prefix removed) equals `spelled`, ignoring case.
fn same_spelling(canonical: &Path, spelled: &Path) -> bool {
    let canonical = canonical.to_string_lossy();
    let canonical = canonical.strip_prefix(r"\\?\").unwrap_or(&canonical);
    canonical.to_lowercase() == spelled.to_string_lossy().to_lowercase()
}

/// `C:\` of a path on drive C:; `None` for any other kind of path.
fn drive_root(path: &Path) -> Option<String> {
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some(format!("{}:\\", char::from(letter)))
            }
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::paths::system_dir;

    const TI: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
    const USER: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

    fn info(sddl: &str) -> SecurityInfo {
        sddl_security(sddl).unwrap_or_else(|e| panic!("{sddl}: {e}"))
    }

    fn safe(sddl: &str, role: AclRole) -> bool {
        untrusted_writer(&info(sddl), role).is_none()
    }

    /// `C:\` as probed on Windows 11.
    fn drive_root_sddl() -> String {
        format!(
            "O:{TI}G:{TI}D:PAI(A;;LC;;;AU)(A;OICIIO;SDGXGWGR;;;AU)(A;OICI;FA;;;SY)\
             (A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)"
        )
    }

    /// `C:\Program Files` and `C:\Windows\System32` as probed on Windows 11.
    fn program_files_sddl() -> String {
        format!(
            "O:{TI}G:{TI}D:PAI(A;OICIIO;GA;;;CO)(A;OICIIO;GA;;;SY)(A;;0x1301bf;;;SY)\
             (A;OICIIO;GA;;;BA)(A;;0x1301bf;;;BA)(A;OICIIO;GXGR;;;BU)(A;;0x1200a9;;;BU)\
             (A;CIIO;GA;;;{TI})(A;;FA;;;{TI})(A;;0x1200a9;;;AC)(A;OICIIO;GXGR;;;AC)\
             (A;;0x1200a9;;;S-1-15-2-2)(A;OICIIO;GXGR;;;S-1-15-2-2)"
        )
    }

    #[test]
    fn the_drive_root_passes_as_an_ancestor_only() {
        let root = drive_root_sddl();
        assert!(safe(&root, AclRole::Ancestor));
        // Authenticated Users may create folders there (LC = 0x4), which a program folder
        // must not allow.
        let reason = untrusted_writer(&info(&root), AclRole::Folder).unwrap();
        assert!(
            reason.contains("Authenticated Users (S-1-5-11)"),
            "{reason}"
        );
    }

    #[test]
    fn program_files_passes_as_folder_and_ancestor() {
        let sddl = program_files_sddl();
        assert!(safe(&sddl, AclRole::Folder));
        assert!(safe(&sddl, AclRole::Ancestor));
        assert!(safe(&sddl, AclRole::File));
    }

    #[test]
    fn a_task_folder_writable_by_users_is_unsafe() {
        // The root Task Scheduler folder: Authenticated Users hold inheritable FW.
        let root = "O:SYD:PAI(A;CI;FA;;;BA)(A;OI;0x1f019f;;;BA)(A;CI;FA;;;SY)(A;OI;0x1f019f;;;SY)\
                    (A;CI;FW;;;AU)(A;CI;FW;;;NS)(A;CI;FW;;;LS)(A;OICIIO;FA;;;CO)";
        let reason = untrusted_writer(&info(root), AclRole::TaskFolder).unwrap();
        assert!(reason.contains("S-1-5-11"), "{reason}");
        // The protected folder a maintenance task is registered in, as created by an
        // administrator (the creator's default owner is Administrators).
        let folder = "O:BAD:P(A;CI;FA;;;BA)(A;OI;0x1f019f;;;BA)(A;CI;FA;;;SY)(A;OI;0x1f019f;;;SY)\
                      (A;OICI;FR;;;AU)";
        assert!(safe(folder, AclRole::TaskFolder));
    }

    #[test]
    fn task_objects_are_judged_like_files() {
        assert!(safe(
            "O:SYD:(A;;FA;;;BA)(A;;FA;;;SY)(A;;0x1200a9;;;LS)(A;;FR;;;AU)",
            AclRole::TaskObject
        ));
        assert!(safe(
            "O:BAD:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FR;;;AU)",
            AclRole::TaskObject
        ));
        // A task a user registered for itself: owned by and writable for that user.
        let user_task = format!("O:{USER}D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FA;;;{USER})");
        let reason = untrusted_writer(&info(&user_task), AclRole::TaskObject).unwrap();
        assert!(reason.contains(USER), "{reason}");
        assert!(reason.starts_with("its owner"), "{reason}");
        // Owned by Administrators but writable for the user.
        let writable = format!("O:BAD:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FA;;;{USER})");
        let reason = untrusted_writer(&info(&writable), AclRole::TaskObject).unwrap();
        assert_eq!(reason, format!("{USER} can change it"));
    }

    #[test]
    fn a_drive_root_like_ancestor_with_add_subdir_and_inherit_only_modify_is_safe() {
        assert!(safe(
            "O:BAD:(A;;0x4;;;AU)(A;OICIIO;0x1301bf;;;AU)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
            AclRole::Ancestor
        ));
        // The same rights without inherit-only reach the folder itself.
        assert!(!safe(
            "O:BAD:(A;;0x4;;;AU)(A;;0x1301bf;;;AU)(A;OICI;FA;;;SY)",
            AclRole::Ancestor
        ));
    }

    #[test]
    fn inherit_only_creator_owner_is_ignored_and_deny_aces_do_not_help() {
        assert!(safe(
            "O:BAD:(A;OICIIO;GA;;;CO)(A;;FA;;;BA)(A;;0x1200a9;;;BU)",
            AclRole::Folder
        ));
        // A deny ACE never makes an allow ACE safe.
        assert!(!safe(
            "O:BAD:(D;;FA;;;BU)(A;;FA;;;BU)(A;;FA;;;BA)",
            AclRole::File
        ));
        let denied = info("O:BAD:(D;;FA;;;BU)(A;;FA;;;BA)");
        assert!(!denied.dacl.as_ref().unwrap()[0].allow);
        assert!(untrusted_writer(&denied, AclRole::File).is_none());
    }

    #[test]
    fn null_dacl_untrusted_owner_and_unknown_types_are_unsafe() {
        let null = info("O:BAD:NO_ACCESS_CONTROL");
        assert_eq!(null.dacl, None);
        assert!(untrusted_writer(&null, AclRole::File)
            .unwrap()
            .contains("no access control list"));

        let owner = format!("O:{USER}D:(A;;FA;;;BA)");
        assert!(!safe(&owner, AclRole::File));

        let object_ace = "O:BAD:(OA;;GA;bf967aba-0de6-11d0-a285-00aa003049e2;;AU)(A;;FA;;;BA)";
        let parsed = info(object_ace);
        let first = &parsed.dacl.as_ref().unwrap()[0];
        assert!(first.allow && !first.known_type, "{first:?}");
        assert!(untrusted_writer(&parsed, AclRole::File)
            .unwrap()
            .contains("type this check does not know"));

        // An inherit-only ACE of unknown type grants nothing on the object itself.
        let inherit_only = SecurityInfo {
            owner: Some("S-1-5-32-544".into()),
            dacl: Some(vec![AceInfo {
                allow: true,
                known_type: false,
                flags: INHERIT_ONLY_ACE,
                mask: 0,
                sid: String::new(),
            }]),
        };
        assert_eq!(untrusted_writer(&inherit_only, AclRole::File), None);
    }

    #[test]
    fn a_descriptor_without_an_owner_is_unsafe() {
        let parsed = info("D:P(A;;FA;;;BA)(A;;FA;;;SY)");
        assert_eq!(parsed.owner, None);
        assert_eq!(
            untrusted_writer(&parsed, AclRole::TaskFolder).as_deref(),
            Some("its owner is not known")
        );
    }

    #[test]
    fn read_only_rights_for_everyone_are_safe_for_every_role() {
        let sddl = format!("O:{TI}D:P(A;;FA;;;{TI})(A;;0x1200a9;;;WD)(A;;GRGX;;;BU)");
        for role in [
            AclRole::File,
            AclRole::Folder,
            AclRole::Ancestor,
            AclRole::TaskObject,
            AclRole::TaskFolder,
        ] {
            assert!(safe(&sddl, role), "{role:?}");
        }
    }

    #[test]
    fn each_dangerous_right_is_refused_for_its_roles() {
        let cases: [(u32, &[AclRole], &[AclRole]); 8] = [
            (
                WRITE_DATA,
                &[AclRole::File, AclRole::Folder],
                &[AclRole::Ancestor],
            ),
            (
                APPEND_DATA,
                &[AclRole::File, AclRole::Folder],
                &[AclRole::Ancestor],
            ),
            (
                DELETE_CHILD,
                &[AclRole::Folder, AclRole::Ancestor],
                &[AclRole::File],
            ),
            (
                DELETE,
                &[AclRole::File, AclRole::Folder, AclRole::Ancestor],
                &[],
            ),
            (
                WRITE_DAC,
                &[AclRole::File, AclRole::Folder, AclRole::Ancestor],
                &[],
            ),
            (
                WRITE_OWNER,
                &[AclRole::File, AclRole::Folder, AclRole::Ancestor],
                &[],
            ),
            (
                GENERIC_WRITE,
                &[AclRole::File, AclRole::Folder],
                &[AclRole::Ancestor],
            ),
            (
                GENERIC_ALL,
                &[AclRole::File, AclRole::Folder, AclRole::Ancestor],
                &[],
            ),
        ];
        for (mask, unsafe_for, safe_for) in cases {
            let sddl = format!("O:BAD:(A;;FA;;;BA)(A;;{mask:#x};;;BU)");
            for &role in unsafe_for {
                assert!(!safe(&sddl, role), "{mask:#x} {role:?}");
            }
            for &role in safe_for {
                assert!(safe(&sddl, role), "{mask:#x} {role:?}");
            }
        }
    }

    #[test]
    fn invalid_sddl_is_an_error() {
        assert!(sddl_security("not an sddl").is_err());
    }

    #[test]
    fn cmd_exe_and_its_folders_are_trusted() {
        let cmd = system_dir().unwrap().join("cmd.exe");
        assert_eq!(program_location_problem(&cmd).unwrap(), None);
        let info = file_security(&cmd).unwrap();
        assert_eq!(info.owner.as_deref(), Some(TI));
        // Companion files and folders that exist are checked too; missing ones are skipped.
        assert_eq!(
            install_location_problem(&cmd, &["kernel32.dll", "no-such.dll"], &["drivers"]).unwrap(),
            None
        );
    }

    #[test]
    fn a_program_in_a_user_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("cairn-maintenance.exe");
        std::fs::write(&program, b"MZ").unwrap();
        let problem = program_location_problem(&program).unwrap().unwrap();
        assert!(
            problem.starts_with(&program.display().to_string()),
            "{problem}"
        );
    }

    #[test]
    fn a_trusted_program_with_an_untrusted_companion_is_refused() {
        // System32 is trusted, so the companion list decides: a user-owned file reached by
        // an absolute companion path (join keeps absolute paths as they are).
        let dir = tempfile::tempdir().unwrap();
        let companion = dir.path().join("vcruntime140.dll");
        std::fs::write(&companion, b"MZ").unwrap();
        let cmd = system_dir().unwrap().join("cmd.exe");
        let companion_text = companion.to_string_lossy().into_owned();
        let problem = install_location_problem(&cmd, &[companion_text.as_str()], &[])
            .unwrap()
            .unwrap();
        assert!(problem.starts_with(&companion_text), "{problem}");
    }

    #[test]
    fn spelling_rules_refuse_relative_missing_and_linked_paths() {
        assert_eq!(
            program_location_problem(Path::new(r"relative\cairn.exe")).unwrap(),
            Some(r"relative\cairn.exe is not an absolute path".to_string())
        );
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.exe");
        assert!(program_location_problem(&missing)
            .unwrap()
            .unwrap()
            .ends_with("does not exist"));

        // A junction in the path makes the canonical path differ from the spelled one.
        let target = dir.path().join("real");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("app.exe"), b"MZ").unwrap();
        let link = dir.path().join("link");
        let status = crate::win::process::system_command("cmd.exe")
            .unwrap()
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let through_link = link.join("app.exe");
        let problem = program_location_problem(&through_link).unwrap().unwrap();
        assert!(problem.ends_with("goes through a link"), "{problem}");
    }

    #[test]
    fn spelling_comparison_ignores_case_and_the_verbatim_prefix() {
        assert!(same_spelling(
            Path::new(r"\\?\C:\Windows\System32\cmd.exe"),
            Path::new(r"c:\windows\system32\CMD.EXE")
        ));
        assert!(!same_spelling(
            Path::new(r"\\?\C:\Windows\System32\cmd.exe"),
            Path::new(r"C:\Windows\SysWOW64\cmd.exe")
        ));
        assert_eq!(
            drive_root(Path::new(r"C:\Program Files\Cairn\Cairn.exe")).as_deref(),
            Some(r"C:\")
        );
        assert_eq!(drive_root(Path::new(r"\\server\share\Cairn.exe")), None);
    }

    #[test]
    fn well_known_sids_are_named() {
        assert_eq!(describe_sid("S-1-5-11"), "Authenticated Users (S-1-5-11)");
        assert_eq!(describe_sid(USER), USER);
        assert!(is_trusted("s-1-5-18"));
        assert!(is_trusted(TI));
        assert!(!is_trusted("S-1-5-11"));
    }
}
