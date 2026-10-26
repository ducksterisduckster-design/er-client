use std::io::Read;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::{ffi::OsString, io, sync::LazyLock};

use anyhow::{Error, Result};
use imgui::*;
use mint::Vector2;
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HMODULE, MAX_PATH};
use windows::Win32::System::LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW, GetModuleFileNameW};
use windows::core::PCWSTR;
use windows_result::Error as WindowsError;

/// Returns the path to the parent directory of the mod.
pub fn mod_directory<'a>() -> Result<&'a Path> {
    // We should use OnceLock.get_or_try_init once it's stable.
    static LOCK: LazyLock<Result<PathBuf>> = LazyLock::new(load_mod_directory);

    match *LOCK {
        Ok(ref path) => Ok(path.as_path()),
        // We can't reuse the existing error, because it's owned by the
        // LazyLock. Instead, try to reproduce it.
        Err(_) => match load_mod_directory() {
            // If we can't reproduce it, just provide a simple error.
            Ok(_) => Err(Error::msg("failed to locate mod directory")),
            Err(err) => Err(err),
        },
    }
}

/// Loads [mod_directory] without caching.
///
/// This previously looked for the location of me3_mod_host.dll, but it is
/// simpler to find the dll's own location and permits using the mod without
/// shipping me3 in a particular directory structure. The prior code could
/// still be used if me3-specific functionality requires it.
fn load_mod_directory() -> Result<PathBuf> {
    println!("Locating mod directory...");
    let module_handle = unsafe {
        fn in_module_dummy() {}
        let mut module_handle = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            PCWSTR(in_module_dummy as *const u16),
            &mut module_handle,
        )?;
        module_handle
    };
    let mut dll_path = get_module_path(module_handle)?;
    dll_path.pop();
    Ok(dll_path)
}

/// Returns the full path to [module].
fn get_module_path(module: HMODULE) -> Result<PathBuf> {
    // `GetModuleFileNameW` doesn't have any way to indicate how much room is
    // necessary for the file, so we have to progressively increase our
    // allocation until we hit the appropriate size.
    let mut size = usize::try_from(MAX_PATH)?;
    let mut filename: Vec<u16>;
    const GROWTH_FACTOR: f64 = 1.5;
    loop {
        filename = vec![0; size];
        let n = unsafe { GetModuleFileNameW(Some(module), &mut filename) } as usize;
        if n == 0 {
            return Err(WindowsError::from_thread().into());
        } else if n == filename.capacity()
            && io::Error::last_os_error()
                .raw_os_error()
                .is_some_and(|c| i32::try_from(ERROR_INSUFFICIENT_BUFFER.0).is_ok_and(|e| c == e))
        {
            size = (size as f64 * GROWTH_FACTOR) as usize;
        } else {
            filename.truncate(n);
            break;
        }
    }

    Ok(PathBuf::from(OsString::from_wide(&filename)))
}

pub trait PopupModalExt {
    /// Sets the size of the modal dialog.
    fn size(self, size: impl Into<Vector2<f32>>, condition: Condition) -> Self;
}

impl<Label> PopupModalExt for PopupModal<'_, '_, Label> {
    fn size(self, size: impl Into<Vector2<f32>>, condition: Condition) -> Self {
        unsafe { imgui_sys::igSetNextWindowSize(size.into().into(), condition as i32) };
        self
    }
}

/// Asynchronous loader for files in the mod's directory, to avoid blocking the
/// game thread on file reads.
///
/// As an alternate, there could be a method in `Core` for loading these things upfront.
pub struct LazyFileReader {
    name: &'static str,
    load_file: Once,
    // Why not
    data: LazyLock<Arc<Mutex<Option<Vec<u8>>>>>,
}

impl LazyFileReader {
    /// Creates a reader for the given file name. This can be done statically.
    pub const fn new(name: &'static str) -> Self {
        Self { name, load_file: Once::new(), data: LazyLock::new(|| Arc::new(Mutex::new(None))) }
    }

    /// Returns the contents of the file at most once. The first time this is called, it spawns
    /// a thread to read the file. Before and after the data is available, it returns `None`.
    /// If the file cannot be read, it logs a warning and will never return anything.
    pub fn take_data(&self) -> Option<Vec<u8>> {
        self.load_file.call_once(|| {
            let Ok(mod_dir) = mod_directory() else {
                return;
            };
            let mut path = mod_dir.to_path_buf();
            path.push(self.name);
            let data_lock = self.data.clone();
            std::thread::spawn(move || {
                match Self::read_data(&path) {
                    Ok(bytes) => {
                        let mut data = data_lock.lock().unwrap();
                        *data = Some(bytes);
                    },
                    Err(err) => {
                        log::warn!("Could not load {:?}: {}", path, err);
                    }
                }
            });
        });
        let mut data = self.data.lock().unwrap();
        data.take()
    }

    fn read_data(path: &PathBuf) -> Result<Vec<u8>> {
        let mut bytes = vec![];
        let mut file = std::fs::File::open(path)?;
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}
