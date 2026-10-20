use std::{ffi::c_void, fs, io::Read, sync::{Arc, Mutex, OnceLock}};

use anyhow::Result;
use fromsoftware_shared::{FromStatic, Program, singleton};
use pelite::pe::Pe;
use shared::utils::{self, LazyFileReader};
use windows::core::PCWSTR;
use zip::ZipArchive;

use crate::rva;

/// Loads icon textures from icons.zip if not yet loaded. This can be called
/// every frame.
///
/// This is a dynamic alternative to the static mod generating 00_solo.tpfbdt
/// which uses 1.3 GB of disk space. A zip file is used because the icons
/// are massive (1 MB per image, 8 MB per icon sheet) and compress very well.
pub fn load_tpf() {
    TextureLoader::get().load_textures();
}

#[repr(C)]
#[singleton("TpfRepository")]
pub struct TpfRepository {}

type LoadTpfResCapFn = unsafe extern "C" fn(*mut TpfRepository, PCWSTR, *const u8, usize, bool, u32) -> *mut c_void;

struct TextureLoader {
    // A lot of this complexity could be avoided This could be a synchronous implementation which reads the file and loads it
    // in the same frame, but try to avoid file I/O worst case.
    // mpsc makes this more complicated than just a simple list.
    load_queue: Mutex<Vec<(String, Vec<u8>)>>,
}

impl TextureLoader {
    /// Gets the loader if it exists
    pub fn get() -> &'static Self {
        static INSTANCE: OnceLock<TextureLoader> = OnceLock::new();
        INSTANCE.get_or_init(|| Self { load_queue: Mutex::new(vec![]) })
    }

    /// Loads textures from icons.zip if it exists and adds any textures in the queue.
    /// This file could also be included as bytes but there is still a file component
    /// required for the layout file and they should be updated together.
    fn load_textures(&self) {
        static READER: LazyFileReader = LazyFileReader::new("icons.zip");
        if let Some(data) = READER.take_data() {
            self.read_icon_data(data).expect("Failed to load icons.zip");
        }
        self.add_textures();
    }

    fn read_icon_data(&self, data: Vec<u8>) -> Result<()> {
        let reader = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(reader)?;
        let mut queue = self.load_queue.lock().unwrap();
        for i in 0..zip.len() {
            if let Ok(mut file) = zip.by_index(i)
                && let Ok(name) = file.name()
                && let Some(base_name) = name.strip_suffix(".tpf") {
                let base_name = base_name.to_owned();
                let mut data = Vec::with_capacity(file.size() as usize);
                file.read_to_end(&mut data)?;
                queue.push((base_name, data));
            }
        }
        Ok(())
    }

    fn add_textures(&self) {
        let mut queue = self.load_queue.lock().unwrap();
        if queue.is_empty() {
            return;
        }
        // When is this present? It should be available much earlier than this can get called.
        let Ok(tpf_repo) = (unsafe { TpfRepository::instance_mut() }) else {
            return;
        };
        let program = Program::current();
        let rvas = rva::get();
        let load_tpf_res_cap_addr = program.rva_to_va(rvas.load_tpf_res_cap).unwrap();
        let load_tpf_res_cap = unsafe {
            std::mem::transmute::<u64, LoadTpfResCapFn>(load_tpf_res_cap_addr)
        };
        // This can be limited to some number if it causes issues on loading in.
        // Alternatively, this can be done while in the main menu if the shared lib allows it.
        for (name, data) in queue.drain(..) {
            let mut name_buffer: Vec<u16> = name.encode_utf16().collect();
            name_buffer.push(0);
            unsafe {
                // Note the texture never gets deallocated!
                load_tpf_res_cap(
                    tpf_repo,
                    PCWSTR(name_buffer.as_ptr()),
                    data.as_ptr(),
                    data.len(),
                    false,
                    0,
                );
            }
        }
    }
}
