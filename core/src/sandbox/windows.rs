//! Confinement on Windows, ported from DeepSeek Harness
//! `packages/sandbox/sandbox-windows-acl/src/` (token.ts, acl.ts, index.ts,
//! runner.ts, workspace-sid.ts, path-boundary.ts) and
//! `packages/subprocess/win32-process/src/process.ts`.
//!
//! The runner starts the command with a WRITE_RESTRICTED, Low-integrity copy
//! of its own token. The restricting SIDs are the logon SID, Everyone, a SID
//! derived from the project folder and a SID derived from the temporary
//! folder; each folder grants its SID write access, denies Everyone
//! `FILE_DELETE_CHILD` on subfolders and carries a Low no-write-up label, so
//! the command can write and delete only inside those two folders. Reads, the
//! network and other processes are not restricted, hard links alias the
//! granted files, and the grants stay on the folders after the command ends.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{addr_of, null, null_mut};

use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, SetHandleInformation, ERROR_ACCESS_DENIED, HANDLE, HANDLE_FLAG_INHERIT,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    AddMandatoryAce, CopySid, CreateRestrictedToken, CreateWellKnownSid, EqualSid, GetAce,
    GetLengthSid, GetTokenInformation, InitializeAcl, SetTokenInformation, TokenDefaultDacl,
    TokenGroups, TokenIntegrityLevel, TokenUser, WinLowLabelSid, WinMediumLabelSid, WinWorldSid,
    ACE_HEADER, ACL, ACL_REVISION, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
    DISABLE_MAX_PRIVILEGE, INHERITED_ACE, LABEL_SECURITY_INFORMATION, PSID, SECURITY_MAX_SID_SIZE,
    SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_ADJUST_DEFAULT,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TOKEN_USER, WELL_KNOWN_SID_TYPE, WRITE_RESTRICTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ALL_ACCESS, FILE_DELETE_CHILD, FILE_GENERIC_WRITE, WRITE_OWNER,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, SetConsoleCtrlHandler, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, SE_GROUP_INTEGRITY, SE_GROUP_LOGON_ID,
    SYSTEM_MANDATORY_LABEL_ACE_TYPE, SYSTEM_MANDATORY_LABEL_NO_READ_UP,
    SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, ResumeThread,
    TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED, INFINITE, PROCESS_INFORMATION,
    STARTF_USESHOWWINDOW, STARTF_USESTDHANDLES, STARTUPINFOW,
};

use super::{Roots, RUNNER_ARGUMENT};

/// Write and delete, without `WRITE_DAC` or `WRITE_OWNER`, so the command
/// cannot rewrite the grants.
const GRANT_MASK: u32 = (FILE_GENERIC_WRITE | DELETE | FILE_DELETE_CHILD) & !0x0002_0000;
const SW_HIDE: u16 = 0;

pub(super) fn confine(
    argv: &[String],
    roots: &Roots,
    runner: &Path,
) -> Result<Vec<String>, String> {
    let text = |path: &Path| {
        path.to_str()
            .map(String::from)
            .ok_or_else(|| format!("{} is not valid Unicode", path.display()))
    };
    let mut confined = vec![
        text(runner)?,
        RUNNER_ARGUMENT.to_string(),
        "--workspace".to_string(),
        text(&roots.folder)?,
        "--temp".to_string(),
        text(&roots.temp)?,
        "--".to_string(),
    ];
    confined.extend(argv.iter().cloned());
    Ok(confined)
}

pub(super) fn run(args: &[String]) -> Result<i32, String> {
    let (workspace, temp, command) = parse(args)?;
    let workspace = canonical_folder("--workspace", workspace)?;
    let temp_canonical = canonical_folder("--temp", temp)?;
    if workspace.starts_with(&temp_canonical) || temp_canonical.starts_with(&workspace) {
        return Err(format!(
            "the temporary folder must be outside the project folder and not contain it: workspace={}; temp={}",
            workspace.display(),
            temp_canonical.display()
        ));
    }

    // The command shares this console and handles its own Ctrl+C; the runner
    // must stay alive to report the command's exit code.
    if unsafe { SetConsoleCtrlHandler(None, 1) } == 0 {
        return Err(last_error("SetConsoleCtrlHandler", "ignore Ctrl+C"));
    }

    let mut current: HANDLE = null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY,
            &mut current,
        )
    } == 0
    {
        return Err(last_error("OpenProcessToken", "this process"));
    }
    let current = Handle(current);
    let user = user_sid(current.0)?;
    let world = well_known_sid(WinWorldSid)?;
    let low = well_known_sid(WinLowLabelSid)?;
    let workspace_sid = parse_sid(&write_sid(&workspace, false))?;
    let temp_sid = parse_sid(&write_sid(&temp_canonical, true))?;
    let known = KnownSids {
        user: user.ptr(),
        world: world.ptr(),
        low: low.ptr(),
    };
    grant_write(&workspace, workspace_sid.0, &known)?;
    grant_write(&temp_canonical, temp_sid.0, &known)?;

    let token = restricted_token(&current, &world, &low, &workspace_sid, &temp_sid)?;
    std::env::set_var("TMP", temp);
    std::env::set_var("TEMP", temp);
    spawn_and_wait(&token, command)
}

fn parse(args: &[String]) -> Result<(&str, &str, &[String]), String> {
    let mut workspace = None;
    let mut temp = None;
    let mut index = 0;
    loop {
        match args.get(index).map(String::as_str) {
            Some("--") => break,
            Some(flag @ ("--workspace" | "--temp")) => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("missing value after {flag}"))?;
                if flag == "--workspace" {
                    workspace = Some(value.as_str());
                } else {
                    temp = Some(value.as_str());
                }
                index += 2;
            }
            Some(other) => return Err(format!("unknown argument: {other}")),
            None => return Err("missing -- before the command".into()),
        }
    }
    let command = &args[index + 1..];
    if command.is_empty() {
        return Err("missing command after --".into());
    }
    Ok((
        workspace.ok_or("missing --workspace")?,
        temp.ok_or("missing --temp")?,
        command,
    ))
}

fn canonical_folder(label: &str, path: &str) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("{label} {path} cannot be resolved: {error}"))?;
    if !canonical.is_dir() {
        return Err(format!("{label} is not an existing directory: {path}"));
    }
    Ok(canonical)
}

/// `S-1-4-x-y` for the project folder, `S-1-4-x-y-1` for the temporary
/// folder: the same folder always gets the same SID, so a repeated grant
/// finds its ACE already in place.
fn write_sid(folder: &Path, temp: bool) -> String {
    let mut hasher = Sha256::new();
    if temp {
        hasher.update(b"temp\0");
    }
    hasher.update(folder.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let part = |at: usize| {
        let value =
            u32::from_le_bytes([digest[at], digest[at + 1], digest[at + 2], digest[at + 3]]);
        value % ((1 << 30) - 1) + 1
    };
    let sid = format!("S-1-4-{}-{}", part(0), part(4));
    if temp {
        sid + "-1"
    } else {
        sid
    }
}

fn last_error(api: &str, context: &str) -> String {
    format!("{api} failed ({context}): {}", io::Error::last_os_error())
}

fn win32_error(api: &str, code: u32, context: &str) -> String {
    format!(
        "{api} failed ({context}): {}",
        io::Error::from_raw_os_error(code as i32)
    )
}

fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}

/// Memory the system allocated with `LocalAlloc`.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A SID copied into memory this process owns.
struct SidBuffer(Vec<u32>);

impl SidBuffer {
    fn ptr(&self) -> PSID {
        self.0.as_ptr() as PSID
    }
}

fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<SidBuffer, String> {
    let mut buffer = vec![0u32; SECURITY_MAX_SID_SIZE as usize / 4];
    let mut size = SECURITY_MAX_SID_SIZE;
    if unsafe { CreateWellKnownSid(kind, null_mut(), buffer.as_mut_ptr().cast(), &mut size) } == 0 {
        return Err(last_error("CreateWellKnownSid", &format!("type {kind}")));
    }
    Ok(SidBuffer(buffer))
}

fn parse_sid(text: &str) -> Result<Local, String> {
    let mut sid: PSID = null_mut();
    if unsafe { ConvertStringSidToSidW(wide(text).as_ptr(), &mut sid) } == 0 || sid.is_null() {
        return Err(last_error("ConvertStringSidToSidW", text));
    }
    Ok(Local(sid))
}

fn explicit_access(sid: PSID, mode: i32, mask: u32, inheritance: u32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    }
}

/// Whether `acl` carries an ACE with exactly this type, flags, mask and SID.
/// Allowed, denied and mandatory-label ACEs share the header, mask, SID
/// layout.
fn has_exact_ace(acl: *const ACL, kind: u32, flags: u32, mask: u32, sid: PSID) -> bool {
    let count = unsafe { (*acl).AceCount };
    (0..u32::from(count)).any(|index| {
        let mut ace: *mut c_void = null_mut();
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return false;
        }
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        let ace_mask = unsafe { *(ace.cast::<u8>().add(4) as *const u32) };
        let ace_sid = unsafe { ace.cast::<u8>().add(8) } as PSID;
        u32::from(header.AceType) == kind
            && u32::from(header.AceFlags) == flags
            && ace_mask == mask
            && unsafe { EqualSid(ace_sid, sid) } != 0
    })
}

/// An ACL holding one inheritable Low no-write-up mandatory label.
fn low_label_acl(low: PSID) -> Result<Vec<u64>, String> {
    label_acl(low, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP)
}

/// An ACL holding one inheritable mandatory label of `level` with `policy`.
fn label_acl(level: PSID, policy: u32) -> Result<Vec<u64>, String> {
    let length = (8 + 8 + unsafe { GetLengthSid(level) } + 3) & !3;
    let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
    let acl = buffer.as_mut_ptr() as *mut ACL;
    if unsafe { InitializeAcl(acl, length, ACL_REVISION) } == 0 {
        return Err(last_error("InitializeAcl", "mandatory label"));
    }
    if unsafe {
        AddMandatoryAce(
            acl,
            ACL_REVISION,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            policy,
            level,
        )
    } == 0
    {
        return Err(last_error("AddMandatoryAce", "mandatory label"));
    }
    Ok(buffer)
}

/// The label that keeps Low-integrity processes, which confined commands
/// are, from reading or writing `folder` and what it holds. Files made in it
/// later inherit the label; those already there are labelled one by one,
/// since setting a folder's label does not reach them. A folder inside that
/// carries its own label (a command's Low temporary folder) is left as it
/// is. Nothing is written when `folder` already has the label.
pub(super) fn hide(folder: &Path) -> Result<(), String> {
    let medium = well_known_sid(WinMediumLabelSid)?;
    let policy = SYSTEM_MANDATORY_LABEL_NO_WRITE_UP | SYSTEM_MANDATORY_LABEL_NO_READ_UP;
    if label_state(folder, medium.ptr(), policy)?.0 {
        return Ok(());
    }
    let label = label_acl(medium.ptr(), policy)?;
    set_label(folder, &label)?;
    label_inside(folder, &label, medium.ptr(), policy)
}

fn label_inside(folder: &Path, label: &[u64], medium: PSID, policy: u32) -> Result<(), String> {
    let entries = std::fs::read_dir(folder)
        .map_err(|error| format!("could not list {}: {error}", folder.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", folder.display()))?;
        let path = entry.path();
        if label_state(&path, medium, policy)?.1 {
            continue;
        }
        set_label(&path, label)?;
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            label_inside(&path, label, medium, policy)?;
        }
    }
    Ok(())
}

/// Whether `path` carries exactly the hiding label, and whether it carries
/// a mandatory label of its own (not inherited).
fn label_state(path: &Path, medium: PSID, policy: u32) -> Result<(bool, bool), String> {
    let name = wide(path);
    let mut sacl: *mut ACL = null_mut();
    let mut descriptor: *mut c_void = null_mut();
    let code = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sacl,
            &mut descriptor,
        )
    };
    if code != 0 {
        return Err(win32_error(
            "GetNamedSecurityInfoW",
            code,
            &path.display().to_string(),
        ));
    }
    let _descriptor = Local(descriptor);
    if sacl.is_null() {
        return Ok((false, false));
    }
    let exact = has_exact_ace(
        sacl,
        SYSTEM_MANDATORY_LABEL_ACE_TYPE,
        SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        policy,
        medium,
    );
    let count = unsafe { (*sacl).AceCount };
    let own = (0..u32::from(count)).any(|index| {
        let mut ace: *mut c_void = null_mut();
        if unsafe { GetAce(sacl, index, &mut ace) } == 0 {
            return false;
        }
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        u32::from(header.AceType) == SYSTEM_MANDATORY_LABEL_ACE_TYPE
            && u32::from(header.AceFlags) & INHERITED_ACE == 0
    });
    Ok((exact, own))
}

fn set_label(path: &Path, label: &[u64]) -> Result<(), String> {
    let name = wide(path);
    let set = || unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null(),
            label.as_ptr() as *const ACL,
        )
    };
    let mut code = set();
    if code == ERROR_ACCESS_DENIED {
        // As in `grant_write`: what the user owns but holds only Modify on
        // needs `WRITE_OWNER` granted before it can be labelled.
        grant_write_owner(path)?;
        code = set();
    }
    if code != 0 {
        return Err(win32_error(
            "SetNamedSecurityInfoW",
            code,
            &path.display().to_string(),
        ));
    }
    Ok(())
}

/// Grants this process's user `WRITE_OWNER` on `folder` itself.
fn grant_write_owner(folder: &Path) -> Result<(), String> {
    let context = folder.display().to_string();
    let mut current: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut current) } == 0 {
        return Err(last_error("OpenProcessToken", "this process"));
    }
    let current = Handle(current);
    let user = user_sid(current.0)?;
    let name = wide(folder);
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: *mut c_void = null_mut();
    let code = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if code != 0 {
        return Err(win32_error("GetNamedSecurityInfoW", code, &context));
    }
    let _descriptor = Local(descriptor);
    let entries = [explicit_access(user.ptr(), GRANT_ACCESS, WRITE_OWNER, 0)];
    let mut merged: *mut ACL = null_mut();
    let code =
        unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), dacl, &mut merged) };
    if code != 0 {
        return Err(win32_error("SetEntriesInAclW", code, &context));
    }
    let merged = Local(merged.cast());
    let code = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            merged.0 as *const ACL,
            null(),
        )
    };
    if code != 0 {
        return Err(win32_error("SetNamedSecurityInfoW", code, &context));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct KnownSids {
    user: PSID,
    world: PSID,
    low: PSID,
}

/// Grants `sid` write access to `folder`, denies Everyone the ambient
/// `FILE_DELETE_CHILD` right on its subfolders and labels it Low; nothing is
/// written when all three are already in place. Setting the label needs
/// `WRITE_OWNER`, which a folder owned by the user but granting only Modify
/// (the usual ACL of a non-system drive) does not give, so the DACL written
/// first also grants the user `WRITE_OWNER` on the folder itself. Each folder
/// only ever receives the one SID derived from its path, so runners granting
/// the same folder at once write the same DACL.
fn grant_write(folder: &Path, sid: PSID, known: &KnownSids) -> Result<(), String> {
    let KnownSids { user, world, low } = *known;
    let context = folder.display().to_string();
    let name = wide(folder);
    let mut dacl: *mut ACL = null_mut();
    let mut sacl: *mut ACL = null_mut();
    let mut descriptor: *mut c_void = null_mut();
    let code = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            &mut sacl,
            &mut descriptor,
        )
    };
    if code != 0 {
        return Err(win32_error("GetNamedSecurityInfoW", code, &context));
    }
    let _descriptor = Local(descriptor);
    if !dacl.is_null()
        && !sacl.is_null()
        && has_exact_ace(
            dacl,
            ACCESS_ALLOWED_ACE_TYPE,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            GRANT_MASK,
            sid,
        )
        && has_exact_ace(
            dacl,
            ACCESS_DENIED_ACE_TYPE,
            CONTAINER_INHERIT_ACE,
            FILE_DELETE_CHILD,
            world,
        )
        && has_exact_ace(
            sacl,
            SYSTEM_MANDATORY_LABEL_ACE_TYPE,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
            low,
        )
    {
        return Ok(());
    }
    let label = low_label_acl(low)?;
    let entries = [
        explicit_access(world, DENY_ACCESS, FILE_DELETE_CHILD, CONTAINER_INHERIT_ACE),
        explicit_access(
            sid,
            GRANT_ACCESS,
            GRANT_MASK,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        ),
        explicit_access(user, GRANT_ACCESS, WRITE_OWNER, 0),
    ];
    let mut merged: *mut ACL = null_mut();
    let code =
        unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), dacl, &mut merged) };
    if code != 0 {
        return Err(win32_error("SetEntriesInAclW", code, &context));
    }
    let merged = Local(merged.cast());
    for (information, dacl, sacl) in [
        (DACL_SECURITY_INFORMATION, merged.0 as *const ACL, null()),
        (
            LABEL_SECURITY_INFORMATION,
            null(),
            label.as_ptr() as *const ACL,
        ),
    ] {
        let code = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                information,
                null_mut(),
                null_mut(),
                dacl,
                sacl,
            )
        };
        if code != 0 {
            return Err(win32_error("SetNamedSecurityInfoW", code, &context));
        }
    }
    Ok(())
}

fn user_sid(token: HANDLE) -> Result<SidBuffer, String> {
    let buffer = token_information(token, TokenUser, "TokenUser")?;
    let user = unsafe { (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid };
    copy_sid(user)
}

fn copy_sid(sid: PSID) -> Result<SidBuffer, String> {
    let length = unsafe { GetLengthSid(sid) };
    let mut copy = vec![0u32; (length as usize).div_ceil(4)];
    if unsafe { CopySid(length, copy.as_mut_ptr().cast(), sid) } == 0 {
        return Err(last_error("CopySid", "token SID"));
    }
    Ok(SidBuffer(copy))
}

fn token_information(token: HANDLE, class: i32, label: &str) -> Result<Vec<u64>, String> {
    let mut needed = 0u32;
    unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(last_error("GetTokenInformation", label));
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    if unsafe {
        GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(last_error("GetTokenInformation", label));
    }
    Ok(buffer)
}

/// The logon session SID, without which the restricted process cannot
/// initialize.
fn logon_sid(token: HANDLE) -> Result<SidBuffer, String> {
    let buffer = token_information(token, TokenGroups, "TokenGroups")?;
    let groups = buffer.as_ptr() as *const TOKEN_GROUPS;
    let count = unsafe { (*groups).GroupCount } as usize;
    let first = unsafe { addr_of!((*groups).Groups) } as *const SID_AND_ATTRIBUTES;
    for index in 0..count {
        let group = unsafe { &*first.add(index) };
        let logon = SE_GROUP_LOGON_ID as u32;
        if group.Attributes & logon != logon || group.Sid.is_null() {
            continue;
        }
        return copy_sid(group.Sid);
    }
    Err(format!(
        "no logon SID among the {count} groups of this process's token"
    ))
}

/// The WRITE_RESTRICTED, Low-integrity copy of this process's token whose
/// restricting SIDs are the logon SID, Everyone and the two write SIDs. The
/// logon SID and Everyone keep DLL initialization and CNG working; the Low
/// level makes every folder without the Low label unwritable.
fn restricted_token(
    current: &Handle,
    world: &SidBuffer,
    low: &SidBuffer,
    workspace_sid: &Local,
    temp_sid: &Local,
) -> Result<Handle, String> {
    let logon = logon_sid(current.0)?;
    let restricting: Vec<SID_AND_ATTRIBUTES> =
        [logon.ptr(), world.ptr(), workspace_sid.0, temp_sid.0]
            .into_iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: 0,
            })
            .collect();
    let mut restricted: HANDLE = null_mut();
    if unsafe {
        CreateRestrictedToken(
            current.0,
            DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
            0,
            null(),
            0,
            null(),
            restricting.len() as u32,
            restricting.as_ptr(),
            &mut restricted,
        )
    } == 0
    {
        return Err(last_error(
            "CreateRestrictedToken",
            "write-restricted token",
        ));
    }
    let restricted = Handle(restricted);

    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: low.ptr(),
            Attributes: SE_GROUP_INTEGRITY as u32,
        },
    };
    let size =
        std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + unsafe { GetLengthSid(low.ptr()) };
    if unsafe {
        SetTokenInformation(
            restricted.0,
            TokenIntegrityLevel,
            addr_of!(label).cast(),
            size,
        )
    } == 0
    {
        return Err(last_error("SetTokenInformation", "Low integrity level"));
    }

    // New objects the command creates (anonymous pipes among them) take the
    // token's default DACL, which names no restricting SID and so would fail
    // the restricted write check; the temporary folder's SID is added to it.
    let buffer = token_information(restricted.0, TokenDefaultDacl, "TokenDefaultDacl")?;
    let current_dacl = unsafe { (*(buffer.as_ptr() as *const TOKEN_DEFAULT_DACL)).DefaultDacl };
    if current_dacl.is_null() {
        return Err("the token has no default DACL to extend".into());
    }
    let entry = explicit_access(
        temp_sid.0,
        GRANT_ACCESS,
        FILE_ALL_ACCESS,
        SUB_CONTAINERS_AND_OBJECTS_INHERIT,
    );
    let mut merged: *mut ACL = null_mut();
    let code = unsafe { SetEntriesInAclW(1, &entry, current_dacl, &mut merged) };
    if code != 0 {
        return Err(win32_error("SetEntriesInAclW", code, "default DACL"));
    }
    let merged = Local(merged.cast());
    let default_dacl = TOKEN_DEFAULT_DACL {
        DefaultDacl: merged.0.cast(),
    };
    if unsafe {
        SetTokenInformation(
            restricted.0,
            TokenDefaultDacl,
            addr_of!(default_dacl).cast(),
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    } == 0
    {
        return Err(last_error("SetTokenInformation", "TokenDefaultDacl"));
    }
    Ok(restricted)
}

/// Quotes one argument the way `CommandLineToArgvW` splits it.
fn quote(argument: &str) -> String {
    if argument.is_empty() {
        return "\"\"".into();
    }
    if !argument.contains(|c: char| c.is_whitespace() || c == '"') {
        return argument.into();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for c in argument.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(c);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

/// Starts `command` with `token`, sharing this process's console and standard
/// handles, inside a job that kills it (and everything it starts) when this
/// process ends; waits for it and returns its exit code.
fn spawn_and_wait(token: &Handle, command: &[String]) -> Result<i32, String> {
    let job = Handle(unsafe { CreateJobObjectW(null(), null()) });
    if job.0.is_null() {
        return Err(last_error("CreateJobObjectW", "kill-on-close job"));
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            addr_of!(limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(last_error("SetInformationJobObject", "kill on job close"));
    }

    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW;
    startup.wShowWindow = SW_HIDE;
    for (kind, slot, label) in [
        (STD_INPUT_HANDLE, &mut startup.hStdInput, "stdin"),
        (STD_OUTPUT_HANDLE, &mut startup.hStdOutput, "stdout"),
        (STD_ERROR_HANDLE, &mut startup.hStdError, "stderr"),
    ] {
        let handle = unsafe { GetStdHandle(kind) };
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(last_error("SetHandleInformation", label));
        }
        *slot = handle;
    }

    let line = command
        .iter()
        .map(|argument| quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let mut line = wide(&line);
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessAsUserW(
            token.0,
            null(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED,
            null(),
            null(),
            &startup,
            &mut info,
        )
    } == 0
    {
        return Err(last_error("CreateProcessAsUserW", &command[0]));
    }
    let process = Handle(info.hProcess);
    let thread = Handle(info.hThread);
    if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
        let error = last_error("AssignProcessToJobObject", &command[0]);
        unsafe { TerminateProcess(process.0, 1) };
        return Err(error);
    }
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        return Err(last_error("ResumeThread", &command[0]));
    }
    drop(thread);
    if unsafe { WaitForSingleObject(process.0, INFINITE) } != WAIT_OBJECT_0 {
        return Err(last_error("WaitForSingleObject", &command[0]));
    }
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
        return Err(last_error("GetExitCodeProcess", &command[0]));
    }
    Ok(code as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_for_command_line_to_argv() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote("C:\\dir with space\\"), "\"C:\\dir with space\\\\\"");
    }
}
