use pelite::pe64::{Pe, PeView};
use std::sync::LazyLock;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::core::PCSTR;

mod bundle;
mod rva_ww;

pub use bundle::RvaBundle;

const NAME: &str = "ELDEN RING™";
const WW_VERSION: &str = "2.7.2.0";
const LANG_ID_EN: u16 = 0x0009;

/// Returns the RVA bundle for the current executable region and version.
///
/// This will panic if the current executable isn't supported by this package.
pub fn get() -> &'static RvaBundle {
    static RVAS: LazyLock<RvaBundle> = LazyLock::new(|| {
        let module = unsafe {
            PeView::module(GetModuleHandleA(PCSTR(std::ptr::null())).unwrap().0 as *const u8)
        };
        detect_version_and_get_rvas(&module)
    });

    &RVAS
}

/// Determines the region and version of the current executable and returns the
/// [RvaBundle] for it. Panics if the version isn't known.
/// This is derived from (and duplicates) bundle extraction in fromsoftware-rs.
fn detect_version_and_get_rvas(module: &PeView) -> RvaBundle {
    let resources = module.resources().unwrap();
    let info = resources.version_info().unwrap();

    // Extract version info
    let product_version = info
        .fixed()
        .expect("Executable doesn't contain version metdata")
        .dwProductVersion;
    let version = format!(
        "{}.{}.{}.{}",
        product_version.Major, product_version.Minor, product_version.Patch, product_version.Build,
    );

    // Extract product name
    let language = *info
        .translation()
        .first()
        .expect("Executable doesn't contain language metdata");
    let mut product_name: Option<String> = None;
    info.strings(language, |k, v| {
        if k == "ProductName" {
            product_name = Some(v.to_string());
        }
    });

    let product = product_name.expect("Executable doesn't contain product name metadata");
    if product != NAME {
        panic!(
            "Expected executable name to be \"{}\", was \"{}\"",
            NAME, &product
        );
    }

    // Require English version. Add Japanese version (0x0011) if RVAs are generated for those.
    let lang_id_base = language.lang_id & 0x03FF;
    if lang_id_base != LANG_ID_EN {
        panic!(
            "Expected executable language ID to be {:#04x}, was {:#04x}",
            LANG_ID_EN, lang_id_base
        );
    }

    if version != WW_VERSION {
        panic!(
            "Expected executable version \"{}\", was \"{}\"",
            WW_VERSION, &version
        );
    }

    rva_ww::RVAS
}
