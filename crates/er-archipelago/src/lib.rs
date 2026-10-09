use eldenring_extra::input::InputBlocker;
use windows::Win32::{Foundation::HINSTANCE, System::SystemServices::DLL_PROCESS_ATTACH};

mod checks;
mod core;
mod game;
mod icons;
mod item;
mod rva;
mod save_data;
mod slot_data;

use save_data::SaveData;

/// The DLL's entry point. Called when the DLL first loads.
///
/// Sets up the mod, then waits for the game to be ready enough to do real work.
#[unsafe(no_mangle)]
extern "C" fn DllMain(_: HINSTANCE, call_reason: u32) -> bool {
    if call_reason != DLL_PROCESS_ATTACH {
        return true;
    }

    shared::handle_panics::<game::EldenRing>();
    shared::start_logger();

    // Hooks go in on the main thread, so the game can't be running the code
    // while we patch it.

    // Safety: we only hook these functions, and only here.
    unsafe {
        SaveData::hook();
        item::hook_items();
        checks::hook_flag_changes();
    }

    let blocker =
        unsafe { InputBlocker::get_instance() }.expect("Failed to initialize input blocker");

    shared::initialize::<game::EldenRing>(game::EldenRingInputBlocker(blocker));

    true
}
