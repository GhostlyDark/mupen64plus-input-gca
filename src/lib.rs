#[macro_use]
mod debug;
mod ffi;
pub mod gca;
#[macro_use]
mod static_cstr;

use ffi::*;
use gca::{GCAdapter, InputState};
use once_cell::sync::OnceCell;
use static_cstr::StaticCStr;
use std::{
    ffi::c_void,
    mem::ManuallyDrop,
    os::raw::{c_char, c_int, c_uchar},
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

#[cfg(unix)]
use libloading::os::unix::Library;
#[cfg(windows)]
use libloading::os::windows::Library;

struct PluginInfo {
    name: StaticCStr,
    version: c_int,
    target_api_version: c_int,
}

impl PluginInfo {
    const fn new() -> Self {
        Self {
            name: static_cstr!("GC Adapter (for Wii U or Switch) Input Plugin"),
            version: 0x000100,            // v0.1.0
            target_api_version: 0x020100, // v2.1.0
        }
    }
}

#[allow(dead_code)]
enum M64Message {
    Error = 1,
    Warning,
    Info,
    Status,
    Verbose,
}

static PLUGIN_INFO: PluginInfo = PluginInfo::new();

static ADAPTER_READ_THREAD: AtomicBool = AtomicBool::new(true);
static LAST_INPUT_STATE: OnceCell<Arc<Mutex<InputState>>> = OnceCell::new();

/// # Safety
///
/// None of the pointers can be null and must be valid
#[no_mangle]
pub unsafe extern "C" fn PluginStartup(
    core_lib_handle: m64p_dynlib_handle,
    context: *mut c_void,
    debug_callback: extern "C" fn(*mut c_void, c_int, *const c_char),
) -> m64p_error {
    if debug::init(debug_callback, context).is_err() {
        return m64p_error_M64ERR_ALREADY_INIT;
    }

    debug_print!(M64Message::Info, "PluginStartup called");

    // Make sure to NOT free the library associated with the handle.
    // That would make other plugins error.
    let lib = ManuallyDrop::new(Library::from_raw(core_lib_handle.cast()));

    let core_api_version_fn = if let Ok(sym) =
        lib.get::<extern "C" fn(*mut c_int, *mut c_int, *mut c_int, *mut c_int)>(
            b"CoreGetAPIVersions\0",
        ) {
        sym
    } else {
        return m64p_error_M64ERR_INPUT_INVALID;
    };

    let mut core_ver = 0;
    core_api_version_fn(
        &mut core_ver as *mut _,
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
    );

    debug_print!(
        M64Message::Info,
        "Core API reported version {:#08X}",
        core_ver
    );

    if core_ver < PLUGIN_INFO.target_api_version
        || core_ver & 0xfff0000 != PLUGIN_INFO.target_api_version & 0xfff0000
    {
        return m64p_error_M64ERR_INCOMPATIBLE;
    }

    let gc_adapter = if let Ok(gc) = GCAdapter::new() {
        gc
    } else {
        debug_print!(M64Message::Error, "Could not connect to GameCube adapter!");
        return m64p_error_M64ERR_PLUGIN_FAIL;
    };

    LAST_INPUT_STATE
        .set(Arc::new(Mutex::new(gc_adapter.read())))
        .unwrap();
    let last_state = LAST_INPUT_STATE.get().unwrap().clone();

    thread::spawn(move || {
        debug_print!(M64Message::Info, "Adapter thread started");

        while ADAPTER_READ_THREAD.load(Ordering::Relaxed) {
            *last_state
                .lock()
                .map_err(|_| debug_print!(M64Message::Error, "Adapter thread lock error!"))
                .unwrap() = gc_adapter.read();

            thread::sleep(Duration::from_millis(1));
        }

        debug_print!(M64Message::Info, "Adapter thread stopped");
    });

    m64p_error_M64ERR_SUCCESS
}

/// # Safety
///
/// Must be called after PluginStartup
#[no_mangle]
pub unsafe extern "C" fn PluginShutdown() -> m64p_error {
    debug_print!(M64Message::Info, "PluginShutdown called");

    ADAPTER_READ_THREAD.store(false, Ordering::Relaxed);

    m64p_error_M64ERR_SUCCESS
}

/// # Safety
///
/// None of the pointers can be null and must be valid
#[no_mangle]
pub unsafe extern "C" fn PluginGetVersion(
    plugin_type: *mut m64p_plugin_type,
    plugin_version: *mut c_int,
    api_version: *mut c_int,
    plugin_name_ptr: *mut *const c_char,
    capabilities: *mut c_int,
) -> m64p_error {
    debug_print!(M64Message::Info, "PluginGetVersion called");

    if !plugin_type.is_null() {
        *plugin_type = m64p_plugin_type_M64PLUGIN_INPUT;
    }
    if !plugin_version.is_null() {
        *plugin_version = PLUGIN_INFO.version;
    }
    if !api_version.is_null() {
        *api_version = PLUGIN_INFO.target_api_version;
    }
    if !plugin_name_ptr.is_null() {
        *plugin_name_ptr = PLUGIN_INFO.name.as_ptr();
    }
    if !capabilities.is_null() {
        *capabilities = 0;
    }

    m64p_error_M64ERR_SUCCESS
}

#[no_mangle]
pub extern "C" fn ControllerCommand(control: c_int, _command: *mut c_uchar) {
    if control == -1 {
        return;
    }

    debug_print!(
        M64Message::Info,
        "ControllerCommand called (control = {})",
        control
    );
}

/// # Safety
///
/// `keys` must point to an intialized `BUTTONS` union
#[no_mangle]
pub unsafe extern "C" fn GetKeys(control: c_int, keys: *mut BUTTONS) {
    debug_print!(
        M64Message::Info,
        "GetKeys called with control = {}",
        control
    );

    read_keys_from_adapter(control, keys);
}

/// # Safety
///
/// `control_info` must point to an initialized `CONTROL_INFO` struct
#[no_mangle]
pub unsafe extern "C" fn InitiateControllers(control_info: CONTROL_INFO) {
    debug_print!(M64Message::Info, "InitiateControllers called");

    let controls = control_info.Controls as *mut [CONTROL; 4];

    for i in 0..4 {
        (*controls)[i].RawData = 1;
        (*controls)[i].Present = 1;
    }
}

/// # Safety
///
/// `command` must be a u8 array with length at least 6
#[no_mangle]
pub unsafe extern "C" fn ReadController(control: c_int, command: *mut u8) {
    if control == -1 {
        return;
    }

    let cmd = ReadCommand::from(*command.add(2));
    match cmd {
        ReadCommand::GetStatus | ReadCommand::ResetController => {
            *command.add(3) = 0x04 | 0x01; // RD_GAMEPAD | RD_ABSOLUTE
            *command.add(4) = 0x00; // RD_NOEEPROM
            *command.add(5) = 0x02; // RD_NOPLUGIN | RD_NOTINITIALIZED
        }
        ReadCommand::ReadKeys => {
            let mut buttons = BUTTONS { Value: 0 };

            read_keys_from_adapter(control, &mut buttons as *mut _);

            *(command.add(3) as *mut u32) = buttons.Value;
        }
        ReadCommand::ReadEepRom => {}
        ReadCommand::WriteEepRom => {}
        ReadCommand::Unrecognized => {
            let c1 = *command.add(1);
            *command.add(1) = c1 | 0x80; // 0x80 = RD_ERROR
        }
    }
}

#[no_mangle]
pub extern "C" fn RomOpen() -> c_int {
    debug_print!(M64Message::Info, "RomOpen called");

    1
}

#[no_mangle]
pub extern "C" fn RomClosed() {
    debug_print!(M64Message::Info, "RomClosed called");
}

#[no_mangle]
pub extern "C" fn SDL_KeyDown(_keymod: c_int, _keysym: c_int) {
    debug_print!(M64Message::Info, "SDL_KeyDown called");
}

#[no_mangle]
pub extern "C" fn SDL_KeyUp(_keymod: c_int, _keysym: c_int) {
    debug_print!(M64Message::Info, "SDL_KeyUp called");
}

enum ReadCommand {
    GetStatus,
    ReadKeys,
    ResetController,
    ReadEepRom,
    WriteEepRom,

    Unrecognized,
}

impl From<u8> for ReadCommand {
    fn from(x: u8) -> Self {
        match x {
            0x00 => ReadCommand::GetStatus,
            0x01 => ReadCommand::ReadKeys,
            0xff => ReadCommand::ResetController,
            0x04 => ReadCommand::ReadEepRom,
            0x05 => ReadCommand::WriteEepRom,
            _ => ReadCommand::Unrecognized,
        }
    }
}

unsafe fn read_keys_from_adapter(control: c_int, keys: *mut BUTTONS) {
    let input_state = LAST_INPUT_STATE
        .get()
        .unwrap()
        .lock()
        .map_err(|_| {
            debug_print!(
                M64Message::Error,
                "Failed to acquire lock in read_keys_from_adapter"
            )
        })
        .unwrap();

    if !input_state.is_connected(control) {
        return;
    }

    let keys = &mut *keys;

    let s = input_state.controller_state(control as usize);

    let c_left = s.y || s.substick_x < 88;
    let c_right = s.x || s.substick_x > 168;
    let c_down = s.substick_y < 88;
    let c_up = s.substick_y > 168;

    const DEADZONE: i32 = 40;
    let (stick_x, stick_y) = {
        let x = s.stick_x.wrapping_add(128) as i8 as i32;
        let y = s.stick_y.wrapping_add(128) as i8 as i32;

        let pos = x.pow(2) + y.pow(2);
        if pos < DEADZONE.pow(2) {
            (0, 0)
        } else {
            (x, y)
        }
    };

    if s.right {
        keys.Value |= 0x0001;
    }
    if s.left {
        keys.Value |= 0x0002;
    }
    if s.down {
        keys.Value |= 0x0004;
    }
    if s.up {
        keys.Value |= 0x0008;
    }
    if s.start {
        keys.Value |= 0x0010;
    }
    // Use the L trigger for N64 Z
    if s.l || s.trigger_left > 148 {
        keys.Value |= 0x0020;
    }
    if s.b {
        keys.Value |= 0x0040;
    }
    if s.a {
        keys.Value |= 0x0080;
    }
    if c_right {
        keys.Value |= 0x0100;
    }
    if c_left {
        keys.Value |= 0x0200;
    }
    if c_down {
        keys.Value |= 0x0400;
    }
    if c_up {
        keys.Value |= 0x0800;
    }
    if s.r || s.trigger_right > 148 {
        keys.Value |= 0x1000;
    }
    // Use the Z button for N64 L
    if s.z {
        keys.Value |= 0x2000;
    }

    keys.__bindgen_anon_1.set_X_AXIS(stick_x as i32);
    keys.__bindgen_anon_1.set_Y_AXIS(stick_y as i32);
}
