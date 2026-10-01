//! Runs the embedded CPython runtime in this process through `Py_Main`.

use std::ffi::OsString;
use std::iter;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{s, PCWSTR};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

use crate::plan::PYTHON_DLL;

/// `int Py_Main(int argc, wchar_t **argv)`.
type PyMain = unsafe extern "C" fn(argc: i32, argv: *mut *mut u16) -> i32;

/// Loads `<root>\python312.dll` (its own folder, then the default system folders, for it and
/// the DLLs it imports) and runs `Py_Main(argv)`; the result is the interpreter's exit
/// code. Err when the DLL or its `Py_Main` cannot be loaded.
pub(crate) fn run(root: &Path, argv: &[OsString]) -> Result<i32, String> {
    let dll: Vec<u16> = root
        .join(PYTHON_DLL)
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    // SAFETY: `dll` is a NUL-terminated absolute path that outlives the call. The module is
    // never freed: the interpreter runs until the process exits.
    let module = unsafe {
        LoadLibraryExW(
            PCWSTR(dll.as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
    }
    .map_err(|e| e.message())?;
    // SAFETY: `module` is a loaded module and the name is a NUL-terminated ANSI string.
    let entry = unsafe { GetProcAddress(module, s!("Py_Main")) }
        .ok_or_else(|| format!("{PYTHON_DLL} has no Py_Main"))?;
    // SAFETY: CPython exports Py_Main with the C signature `int Py_Main(int, wchar_t **)`,
    // which `PyMain` declares; on x86-64 the "C" and "system" calling conventions are the
    // same.
    let py_main =
        unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, PyMain>(entry) };
    let argc = i32::try_from(argv.len()).map_err(|_| "too many arguments".to_string())?;
    let mut strings: Vec<Vec<u16>> = argv
        .iter()
        .map(|arg| arg.encode_wide().chain(iter::once(0)).collect())
        .collect();
    let mut pointers: Vec<*mut u16> = strings.iter_mut().map(|s| s.as_mut_ptr()).collect();
    pointers.push(std::ptr::null_mut());
    // SAFETY: `pointers` holds `argc` pointers to NUL-terminated wide strings followed by a
    // null pointer, as Py_Main expects; `strings` and `pointers` outlive the call.
    Ok(unsafe { py_main(argc, pointers.as_mut_ptr()) })
}
