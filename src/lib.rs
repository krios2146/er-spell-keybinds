mod mapper;

use crate::mapper::{map_key, map_modifier};

use ini::ini;

use pelite::{
    pattern,
    pe32::headers::SectionHeader,
    pe64::{Pe, PeObject, PeView},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    ptr::read_unaligned,
    time::{Duration, Instant},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HMODULE, MAX_PATH},
        System::LibraryLoader::{
            GetModuleFileNameW,
            GetModuleHandleExW,
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        },
        UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEINPUT, VIRTUAL_KEY,
        },
    },
};

use eldenring::{
    cs::{CSTaskGroupIndex, CSTaskImp, GameDataMan, HudType, WorldChrMan},
    fd4::FD4TaskData,
    util::system::wait_for_system_init,
};

use fromsoftware_shared::{program::Program, task::*, FromStatic};

use device_query::{DeviceQuery, DeviceState, Keycode};
use keyboard_codes::{parse_input, Modifier, Shortcut};

const GAME_DATA_MAN_PATTERN_STR: &str = "48 8B 05 ? ? ? ? 48 85 C0 74 05 48 8B 40 58 C3 C3";

const OFFSET: usize = 3;
const ADDITIONAL: usize = 7;

enum Action {
    SetMemorySlot(u8),
    SetQuickSlot(u8),
    CycleBack,
    NoOp,
}

fn get_pe_view() -> PeView<'static> {
    let pe_view = match Program::current() {
        Program::Mapping(mapping) => mapping,
        Program::File(file) => PeView::from_bytes(file.image()).unwrap()
    };

    pe_view
}

fn get_text_header(pe: PeView<'_>) -> &SectionHeader {
    let text_header = match pe
        .section_headers()
        .iter()
        .find(|h| h.name() == Ok(".text"))
    {
        Some(h) => { h }
        None => { panic!() }
    };
    text_header
}

fn get_game_data_man() -> &'static mut GameDataMan {
    let pattern = match pattern::parse(GAME_DATA_MAN_PATTERN_STR) {
        Ok(p) => p,
        Err(_) => { panic!() }
    };

    let pe = get_pe_view();
    let text_header = get_text_header(pe);

    let scanner = pe.scanner();

    let mut rva = [0; 8];
    let mut matches = scanner.matches(&*pattern, text_header.file_range());

    let game_data_man = loop {
        if !matches.next(&mut rva) {
            panic!()
        }

        let rva = rva[0] as usize;

        let resolved_va = unsafe {
            let aob_va = pe.image().as_ptr().add(rva);

            let offset_value = read_unaligned(aob_va.add(OFFSET) as *const i32);

            let resolved_va = aob_va.add(ADDITIONAL).offset(offset_value as isize);

            resolved_va
        };

        let pointer: *const *mut GameDataMan = resolved_va as *const *mut GameDataMan;
        let game_data_man_ptr: *mut GameDataMan = unsafe { *pointer };
        break unsafe { &mut *game_data_man_ptr };
    };
    game_data_man
}

fn get_dll_path() -> String {
    unsafe {
        let mut module = HMODULE::default();

        let addr = get_dll_path as *const () as *const u16;

        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr),
            &mut module,
        ).expect("GetModuleHandleExW failed");

        let mut buf = [0u16; MAX_PATH as usize];
        let len = GetModuleFileNameW(Some(module), &mut buf);
        let path = String::from_utf16_lossy(&buf[..len as usize]);

        Path::new(&path)
            .parent()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }
}

fn config_key_to_action(key: &String) -> Action {
    if let Some(s) = key.strip_prefix("memory_slot_") {
        return Action::SetMemorySlot(s.parse().unwrap());
    }
    if let Some(s) = key.strip_prefix("quick_slot_") {
        return Action::SetQuickSlot(s.parse().unwrap());
    }
    if key.contains("cycle") {
        return Action::CycleBack;
    }
    Action::NoOp
}

fn read_config() -> HashMap<Shortcut, Action> {
    let config = ini!(&(get_dll_path() + "\\spell_keybinds.ini"));

    let config: HashMap<Shortcut, Action> = config["keybinds"].iter()
        .map(|(k, v)| { (k, parse_input(&v.clone().unwrap_or(String::new()))) })
        .filter(|kv| kv.1.is_ok())
        .map(|(k, v)| (k, v.unwrap()))
        .map(|(k, v)| { (config_key_to_action(k), v) })
        .filter(|(action, _)| !matches!(action, Action::NoOp))
        .map(|(k, v)| (v, k))
        .collect();

    config
}


fn ini_value(
    config: &HashMap<String, HashMap<String, Option<String>>>,
    section: &str,
    key: &str,
) -> Option<String> {
    config.get(section)?.get(key)?.clone()
}

#[derive(Clone, Copy)]
enum CastInput {
    MouseLeft,
    MouseRight,
    Key(u16),
}

fn parse_cast_input(value: &str) -> Option<CastInput> {
    if value.eq_ignore_ascii_case("off") {
        return None;
    }
    if value.eq_ignore_ascii_case("MouseLeft") {
        return Some(CastInput::MouseLeft);
    }
    if value.eq_ignore_ascii_case("MouseRight") {
        return Some(CastInput::MouseRight);
    }
    let upper = value.trim().to_ascii_uppercase();
    let vk = match upper.as_str() {
        "CTRL" | "CONTROL" | "LEFTCTRL" | "LEFTCONTROL" => 0xA2,
        "RIGHTCTRL" | "RIGHTCONTROL" => 0xA3,
        "SHIFT" | "LEFTSHIFT" => 0xA0,
        "RIGHTSHIFT" => 0xA1,
        "ALT" | "LEFTALT" => 0xA4,
        "RIGHTALT" => 0xA5,
        "TAB" => 0x09,
        _ => use_key_vk(&value),
    };
    Some(CastInput::Key(vk))
}

fn read_autocast() -> Option<CastInput> {
    let config = ini!(&(get_dll_path() + "\\spell_keybinds.ini"));
    let value = ini_value(&config, "keybinds", "auto_cast")?;
    parse_cast_input(&value)
}

fn read_second_catalyst() -> Option<CastInput> {
    let config = ini!(&(get_dll_path() + "\\spell_keybinds.ini"));
    let value = ini_value(&config, "keybinds", "2nd_catalyst")?;
    parse_cast_input(&value)
}

fn read_second_catalyst_slots() -> HashSet<u8> {
    let config = ini!(&(get_dll_path() + "\\spell_keybinds.ini"));
    ini_value(&config, "keybinds", "2nd_catalyst_slots")
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| part.trim().parse::<u8>().ok())
        .collect()
}

fn hold_cast(input: CastInput, down: bool) {
    match input {
        CastInput::MouseLeft => mouse_button(true, down),
        CastInput::MouseRight => mouse_button(false, down),
        CastInput::Key(vk) => key_button(vk, down),
    }
}

fn key_button(vk: u16, down: bool) {
    let scan = unsafe {
        windows::Win32::UI::Input::KeyboardAndMouse::MapVirtualKeyW(
            vk as u32,
            windows::Win32::UI::Input::KeyboardAndMouse::MAPVK_VK_TO_VSC,
        ) as u16
    };
    let flags = if down {
        windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_SCANCODE
    } else {
        windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP
    };
    let mut input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(std::slice::from_mut(&mut input), std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse_button(left_button: bool, down: bool) {
    let flags = match (left_button, down) {
        (true, true) => MOUSEEVENTF_LEFTDOWN,
        (true, false) => MOUSEEVENTF_LEFTUP,
        (false, true) => MOUSEEVENTF_RIGHTDOWN,
        (false, false) => MOUSEEVENTF_RIGHTUP,
    };
    let mut input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(std::slice::from_mut(&mut input), std::mem::size_of::<INPUT>() as i32);
    }
}

fn use_key_vk(name: &str) -> u16 {
    let upper = name.trim().to_ascii_uppercase();
    match upper.as_str() {
        "CTRL" | "CONTROL" | "LEFTCTRL" | "LEFTCONTROL" => return 0xA2,
        "RIGHTCTRL" | "RIGHTCONTROL" => return 0xA3,
        "SHIFT" | "LEFTSHIFT" => return 0xA0,
        "RIGHTSHIFT" => return 0xA1,
        "ALT" | "LEFTALT" => return 0xA4,
        "RIGHTALT" => return 0xA5,
        "TAB" => return 0x09,
        _ => {}
    }
    if upper.len() == 1 {
        let ch = upper.as_bytes()[0];
        if ch.is_ascii_alphanumeric() {
            return ch as u16;
        }
    }
    if let Some(rest) = upper.strip_prefix('F') {
        if let Ok(n) = rest.parse::<u16>() {
            if (1..=24).contains(&n) {
                return 0x6F + n;
            }
        }
    }
    0x52
}

fn read_autouse() -> Option<u16> {
    let config = ini!(&(get_dll_path() + "\\spell_keybinds.ini"));
    let Some(value) = ini_value(&config, "keybinds", "auto_use") else {
        return None;
    };
    if value.eq_ignore_ascii_case("off") {
        return None;
    }
    Some(use_key_vk(&value))
}

fn press_use_item(vk: u16) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
        let scan = unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::MapVirtualKeyW(
                vk as u32,
                windows::Win32::UI::Input::KeyboardAndMouse::MAPVK_VK_TO_VSC,
            ) as u16
        };
        let mut input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_SCANCODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe {
            SendInput(std::slice::from_mut(&mut input), std::mem::size_of::<INPUT>() as i32);
            std::thread::sleep(Duration::from_millis(40));
            input.Anonymous.ki.dwFlags = windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_SCANCODE
                | KEYEVENTF_KEYUP;
            SendInput(std::slice::from_mut(&mut input), std::mem::size_of::<INPUT>() as i32);
        }
    });
}

fn set_memory_slot(game_data_man: &mut GameDataMan, slot_index: u8) {
    unsafe {
        let gdm = game_data_man as *mut GameDataMan as *const u8;

        // Correct chain from TGA table: +0x08 → +0x530 → +0x80
        let p1 = *(gdm.add(0x08) as *const *mut u8);
        if p1.is_null() || (p1 as usize) < 0x10000 { return; }

        let equip_magic_data = *(p1.add(0x530) as *const *mut u8);
        if equip_magic_data.is_null() || (equip_magic_data as usize) < 0x10000 { return; }

        // selected_slot is at +0x80
        let target = slot_index.min(13);
        *(equip_magic_data.add(0x80) as *mut i32) = target as i32;
    }
}

fn set_quick_slot(game_data_man: &mut GameDataMan, slot_index: u8) {
    unsafe {
        let gdm = game_data_man as *mut GameDataMan as *const u8;

        // Chain from pointer scan: +0x8 → +0x5D8
        let p1 = *(gdm.add(0x8) as *const *mut u8);
        if p1.is_null() || (p1 as usize) < 0x10000 { return; }

        let target = slot_index.min(9);
        *(p1.add(0x5D8) as *mut i32) = target as i32;
    }
}

fn back_cycle_memory_slot(game_data_man: &mut GameDataMan) {
    unsafe {
        let gdm = game_data_man as *mut GameDataMan as *const u8;
        let p1 = *(gdm.add(0x08) as *const *mut u8);
        if p1.is_null() || (p1 as usize) < 0x10000 { return; }
        let equip_magic_data = *(p1.add(0x530) as *const *mut u8);
        if equip_magic_data.is_null() || (equip_magic_data as usize) < 0x10000 { return; }

        let current = *(equip_magic_data.add(0x80) as *const i32);
        let previous = if current <= 0 { 0 } else { current - 1 };
        *(equip_magic_data.add(0x80) as *mut i32) = previous;
    }
}

fn cartesian_product(keycodes: Vec<HashSet<Keycode>>) -> Vec<HashSet<Keycode>> {
    if keycodes.is_empty() {
        return vec![HashSet::new()];
    }
    let set = &keycodes[0];
    let cartesian = cartesian_product(keycodes[1..].to_vec());

    let mut product = Vec::new();

    for keycode in set {
        for set in &cartesian {
            let mut current_set = set.clone();
            current_set.insert(*keycode);
            product.push(current_set);
        }
    }

    product
}

fn expand_combinations(key: Keycode, modifiers: Vec<Modifier>, action: &Action) -> Vec<(HashSet<Keycode>, &Action)> {
    if modifiers.is_empty() {
        let mut set = HashSet::new();
        set.insert(key);
        return vec![(set, action)];
    }
    let mod_keycodes = modifiers.iter()
        .map(|m| {
            let mut set = HashSet::new();

            match map_modifier(m) {
                (base_modifier, None) => {
                    set.insert(base_modifier);
                }
                (base_modifier, Some(add_modifier)) => {
                    set.insert(base_modifier);
                    set.insert(add_modifier);
                }
            }
            set
        })
        .collect::<Vec<HashSet<Keycode>>>();

    let mut key_keycode = HashSet::new();
    key_keycode.insert(key);

    let mut joined_keycodes = mod_keycodes.clone();
    joined_keycodes.push(key_keycode);

    cartesian_product(joined_keycodes)
        .into_iter()
        .map(|p| (p, action))
        .collect()
}

fn is_all_keybinding_keys_pressed(keybindings: &HashSet<Keycode>, pressed_keys: &Vec<Keycode>) -> bool {
    for key in keybindings {
        if !pressed_keys.contains(key) {
            return false;
        }
    }
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn DllMain(_hmodule: u64, reason: u32) -> bool {
    if reason != 1 {
        return true;
    }

    std::thread::spawn(|| {
        wait_for_system_init(&Program::current(), Duration::MAX)
            .expect("Timeout waiting for system init");
        
        let device_state = DeviceState::new();
        let config = read_config();
        let autocast = read_autocast();
        let second_catalyst = read_second_catalyst();
        let second_catalyst_slots = read_second_catalyst_slots();
        let autouse_vk = read_autouse();

        let mut cycle_back_held = false;
        let mut cast_held = false;
        let mut cast_charging = false;
        let mut held_cast: Option<CastInput> = None;
        let mut quick_held = false;

        let mut last_hud_update_run = Instant::now();
        let hud_update_rebound = Duration::from_secs(3);
        let mut player_hud_type = None;
        let mut is_hud_restored = true;

        
        let cs_task = unsafe { CSTaskImp::instance().unwrap() };



        cs_task.run_recurring(
            move |_: &FD4TaskData| {
                let Some(main_player) = unsafe { WorldChrMan::instance() }
                    .ok()
                    .and_then(|wcm| wcm.main_player.as_ref())
                else {
                    return
                };

                if main_player.chr_ins.modules.data.hp <= 0 {
                    return;
                }

                if player_hud_type.is_none() {
                    player_hud_type = Some(get_game_data_man().game_settings.hud_type);
                }

                let is_need_to_restore_hud = !is_hud_restored &&
                    last_hud_update_run.elapsed() > hud_update_rebound &&
                    get_game_data_man().game_settings.hud_type != player_hud_type.unwrap();

                if is_need_to_restore_hud {
                    get_game_data_man().game_settings.hud_type = player_hud_type.unwrap();
                    is_hud_restored = true;
                }

                let mut keybindings = config.iter()
                    .map(|(s, a)| (s.key, s.modifiers.clone(), a))
                    .map(|(k, m, a)| (map_key(&k), m, a))
                    .filter(|kma| kma.0.is_some())
                    .map(|(k, m, a)| (k.unwrap(), m, a))
                    .flat_map(|(k, m, a)| expand_combinations(k, m, a))
                    .collect::<Vec<(HashSet<Keycode>, &Action)>>();

                keybindings.sort_by(|(x, _), (y, _)| y.len().cmp(&x.len()));

                let pressed_keys = device_state.get_keys();

                let cycle_back_down = keybindings.iter().any(|(keys, action)| {
                    matches!(action, Action::CycleBack)
                        && is_all_keybinding_keys_pressed(keys, &pressed_keys)
                });
                if !cycle_back_down {
                    cycle_back_held = false;
                }

                let cast_down = keybindings.iter().any(|(keys, action)| {
                    matches!(action, Action::SetMemorySlot(_))
                        && is_all_keybinding_keys_pressed(keys, &pressed_keys)
                });
                if !cast_down {
                    if cast_charging {
                        if let Some(input) = held_cast {
                            hold_cast(input, false);
                        }
                        held_cast = None;
                        cast_charging = false;
                    }
                    cast_held = false;
                }

                let quick_down = keybindings.iter().any(|(keys, action)| {
                    matches!(action, Action::SetQuickSlot(_))
                        && is_all_keybinding_keys_pressed(keys, &pressed_keys)
                });
                if !quick_down {
                    quick_held = false;
                }

                for (keybinds, action) in keybindings {
                    if !is_all_keybinding_keys_pressed(&keybinds, &pressed_keys) {
                        continue;
                    }
                    match action {
                        Action::SetMemorySlot(slot) => {
                            if cast_held {
                                break;
                            }
                            cast_held = true;

                            get_game_data_man().game_settings.hud_type = HudType::On;
                            last_hud_update_run = Instant::now();
                            is_hud_restored = false;

                            set_memory_slot(get_game_data_man(), slot - 1);
                            let input = if second_catalyst.is_some() && second_catalyst_slots.contains(slot) {
                                second_catalyst
                            } else {
                                autocast
                            };
                            if let Some(input) = input {
                                hold_cast(input, true);
                                held_cast = Some(input);
                                cast_charging = true;
                            }
                        }
                        Action::SetQuickSlot(slot) => {
                            if quick_held {
                                break;
                            }
                            quick_held = true;

                            get_game_data_man().game_settings.hud_type = HudType::On;
                            last_hud_update_run = Instant::now();
                            is_hud_restored = false;

                            set_quick_slot(get_game_data_man(), slot - 1);
                            if let Some(vk) = autouse_vk {
                                press_use_item(vk);
                            }
                        }
                        Action::CycleBack => {
                            if cycle_back_held {
                                break;
                            }
                            cycle_back_held = true;

                            get_game_data_man().game_settings.hud_type = HudType::On;
                            last_hud_update_run = Instant::now();
                            is_hud_restored = false;

                            back_cycle_memory_slot(get_game_data_man());
                        }
                        Action::NoOp => {}
                    }
                    break;
                }
            },
            CSTaskGroupIndex::FrameBegin,
        );
    });
    true
}
