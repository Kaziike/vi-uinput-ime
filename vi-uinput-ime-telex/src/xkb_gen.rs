use evdev::Key;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// Virtual uinput keyboard identity (must match compositor `device {}` block on Hyprland/Sway).
pub const VIRTUAL_KBD_NAME: &str = "vi-uinput-virtual-kbd";
pub const VIRTUAL_KBD_VENDOR_ID: u16 = 0x1209; // pid.codes open hardware
pub const VIRTUAL_KBD_PRODUCT_ID: u16 = 0x0001;

/// Modifier key used to activate XKB `ISO_Level5_Shift`.
/// Uses `KEY_HENKAN` (`<HENK>`, Linux EV_KEY code 92, XKB keycode 100) rather than `KEY_RIGHTALT`
/// because `<HENK>`:
///   1. Never carries `Mod1` (Alt), preventing GTK/Wayland/Mutter from swallowing keys as `Alt+<key>`.
///   2. Does not exist on standard US/EU physical keyboards, making it 100% safe even on seat-wide
///      XKB keymaps such as GNOME/Mutter and KDE/KWin where per-device `kb_file` is unsupported.
pub const LEVEL5_MODIFIER_KEY: Key = Key::KEY_HENKAN;

/// Key combination emitted to `/dev/uinput` to produce a single precomposed Vietnamese character
/// via our custom XKB keymap (`vn_virtual.xkb` or GNOME RMLVO `symbols/vi_uinput`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub key: Key,
    pub level5: bool,
    pub shift: bool,
}

/// Static descriptor for one lowercase/uppercase Vietnamese pair mapped to a physical keycode.
#[derive(Debug, Clone, Copy)]
pub struct VnKeySlot {
    pub lower: char,
    pub upper: char,
    pub key: Key,
    pub xkb_name: &'static str,
    pub base_l1: &'static str,
    pub base_l2: &'static str,
    pub is_alpha: bool,
}

/// Canonical 67-slot table covering all 134 precomposed Vietnamese diacritic characters:
///   - Group `a`: 17 pairs (à á ả ã ạ ă ằ ắ ẳ ẵ ặ â ầ ấ ẩ ẫ ậ)
///   - Group `e`: 11 pairs (è é ẻ ẽ ẹ ê ề ế ể ễ ệ)
///   - Group `i`:  5 pairs (ì í ỉ ĩ ị)
///   - Group `o`: 17 pairs (ò ó ỏ õ ọ ô ồ ố ổ ỗ ộ ơ ờ ớ ở ỡ ợ)
///   - Group `u`: 11 pairs (ù ú ủ ũ ụ ư ừ ứ ử ữ ự)
///   - Group `y`:  5 pairs (ỳ ý ỷ ỹ ỵ)
///   - Group `d`:  1 pair  (đ)
/// Total: 67 lowercase + 67 uppercase = 134 Vietnamese characters.
/// Physical keycodes used (all valid `linux/input-event-codes.h`):
///   - KEY_A..KEY_Z (26)
///   - KEY_1..KEY_0 (10)
///   - KEY_F1..KEY_F24 (24)
///   - 7 punctuation keys (MINUS, EQUAL, LEFTBRACE, RIGHTBRACE, SEMICOLON, APOSTROPHE, GRAVE)
pub const VN_KEY_SLOTS: [VnKeySlot; 67] = [
    // --- Group `a` (17 pairs) ---
    VnKeySlot { lower: 'à', upper: 'À', key: Key::KEY_A, xkb_name: "AC01", base_l1: "a", base_l2: "A", is_alpha: true },
    VnKeySlot { lower: 'á', upper: 'Á', key: Key::KEY_B, xkb_name: "AB05", base_l1: "b", base_l2: "B", is_alpha: true },
    VnKeySlot { lower: 'ả', upper: 'Ả', key: Key::KEY_C, xkb_name: "AB03", base_l1: "c", base_l2: "C", is_alpha: true },
    VnKeySlot { lower: 'ã', upper: 'Ã', key: Key::KEY_D, xkb_name: "AC03", base_l1: "d", base_l2: "D", is_alpha: true },
    VnKeySlot { lower: 'ạ', upper: 'Ạ', key: Key::KEY_E, xkb_name: "AD03", base_l1: "e", base_l2: "E", is_alpha: true },
    VnKeySlot { lower: 'ă', upper: 'Ă', key: Key::KEY_F, xkb_name: "AC04", base_l1: "f", base_l2: "F", is_alpha: true },
    VnKeySlot { lower: 'ằ', upper: 'Ằ', key: Key::KEY_G, xkb_name: "AC05", base_l1: "g", base_l2: "G", is_alpha: true },
    VnKeySlot { lower: 'ắ', upper: 'Ắ', key: Key::KEY_H, xkb_name: "AC06", base_l1: "h", base_l2: "H", is_alpha: true },
    VnKeySlot { lower: 'ẳ', upper: 'Ẳ', key: Key::KEY_I, xkb_name: "AD08", base_l1: "i", base_l2: "I", is_alpha: true },
    VnKeySlot { lower: 'ẵ', upper: 'Ẵ', key: Key::KEY_J, xkb_name: "AC07", base_l1: "j", base_l2: "J", is_alpha: true },
    VnKeySlot { lower: 'ặ', upper: 'Ặ', key: Key::KEY_K, xkb_name: "AC08", base_l1: "k", base_l2: "K", is_alpha: true },
    VnKeySlot { lower: 'â', upper: 'Â', key: Key::KEY_L, xkb_name: "AC09", base_l1: "l", base_l2: "L", is_alpha: true },
    VnKeySlot { lower: 'ầ', upper: 'Ầ', key: Key::KEY_M, xkb_name: "AB07", base_l1: "m", base_l2: "M", is_alpha: true },
    VnKeySlot { lower: 'ấ', upper: 'Ấ', key: Key::KEY_N, xkb_name: "AB06", base_l1: "n", base_l2: "N", is_alpha: true },
    VnKeySlot { lower: 'ẩ', upper: 'Ẩ', key: Key::KEY_O, xkb_name: "AD09", base_l1: "o", base_l2: "O", is_alpha: true },
    VnKeySlot { lower: 'ẫ', upper: 'Ẫ', key: Key::KEY_P, xkb_name: "AD10", base_l1: "p", base_l2: "P", is_alpha: true },
    VnKeySlot { lower: 'ậ', upper: 'Ậ', key: Key::KEY_Q, xkb_name: "AD01", base_l1: "q", base_l2: "Q", is_alpha: true },

    // --- Group `e` (11 pairs) ---
    VnKeySlot { lower: 'è', upper: 'È', key: Key::KEY_R, xkb_name: "AD04", base_l1: "r", base_l2: "R", is_alpha: true },
    VnKeySlot { lower: 'é', upper: 'É', key: Key::KEY_S, xkb_name: "AC02", base_l1: "s", base_l2: "S", is_alpha: true },
    VnKeySlot { lower: 'ẻ', upper: 'Ẻ', key: Key::KEY_T, xkb_name: "AD05", base_l1: "t", base_l2: "T", is_alpha: true },
    VnKeySlot { lower: 'ẽ', upper: 'Ẽ', key: Key::KEY_U, xkb_name: "AD07", base_l1: "u", base_l2: "U", is_alpha: true },
    VnKeySlot { lower: 'ẹ', upper: 'Ẹ', key: Key::KEY_V, xkb_name: "AB04", base_l1: "v", base_l2: "V", is_alpha: true },
    VnKeySlot { lower: 'ê', upper: 'Ê', key: Key::KEY_W, xkb_name: "AD02", base_l1: "w", base_l2: "W", is_alpha: true },
    VnKeySlot { lower: 'ề', upper: 'Ề', key: Key::KEY_X, xkb_name: "AB02", base_l1: "x", base_l2: "X", is_alpha: true },
    VnKeySlot { lower: 'ế', upper: 'Ế', key: Key::KEY_Y, xkb_name: "AD06", base_l1: "y", base_l2: "Y", is_alpha: true },
    VnKeySlot { lower: 'ể', upper: 'Ể', key: Key::KEY_Z, xkb_name: "AB01", base_l1: "z", base_l2: "Z", is_alpha: true },
    VnKeySlot { lower: 'ễ', upper: 'Ễ', key: Key::KEY_1, xkb_name: "AE01", base_l1: "1", base_l2: "exclam", is_alpha: false },
    VnKeySlot { lower: 'ệ', upper: 'Ệ', key: Key::KEY_2, xkb_name: "AE02", base_l1: "2", base_l2: "at", is_alpha: false },

    // --- Group `i` (5 pairs) ---
    VnKeySlot { lower: 'ì', upper: 'Ì', key: Key::KEY_3, xkb_name: "AE03", base_l1: "3", base_l2: "numbersign", is_alpha: false },
    VnKeySlot { lower: 'í', upper: 'Í', key: Key::KEY_4, xkb_name: "AE04", base_l1: "4", base_l2: "dollar", is_alpha: false },
    VnKeySlot { lower: 'ỉ', upper: 'Ỉ', key: Key::KEY_5, xkb_name: "AE05", base_l1: "5", base_l2: "percent", is_alpha: false },
    VnKeySlot { lower: 'ĩ', upper: 'Ĩ', key: Key::KEY_6, xkb_name: "AE06", base_l1: "6", base_l2: "asciicircum", is_alpha: false },
    VnKeySlot { lower: 'ị', upper: 'Ị', key: Key::KEY_7, xkb_name: "AE07", base_l1: "7", base_l2: "ampersand", is_alpha: false },

    // --- Group `o` (17 pairs) ---
    VnKeySlot { lower: 'ò', upper: 'Ò', key: Key::KEY_8, xkb_name: "AE08", base_l1: "8", base_l2: "asterisk", is_alpha: false },
    VnKeySlot { lower: 'ó', upper: 'Ó', key: Key::KEY_9, xkb_name: "AE09", base_l1: "9", base_l2: "parenleft", is_alpha: false },
    VnKeySlot { lower: 'ỏ', upper: 'Ỏ', key: Key::KEY_0, xkb_name: "AE10", base_l1: "0", base_l2: "parenright", is_alpha: false },
    VnKeySlot { lower: 'õ', upper: 'Õ', key: Key::KEY_F1, xkb_name: "FK01", base_l1: "F1", base_l2: "F1", is_alpha: false },
    VnKeySlot { lower: 'ọ', upper: 'Ọ', key: Key::KEY_F2, xkb_name: "FK02", base_l1: "F2", base_l2: "F2", is_alpha: false },
    VnKeySlot { lower: 'ô', upper: 'Ô', key: Key::KEY_F3, xkb_name: "FK03", base_l1: "F3", base_l2: "F3", is_alpha: false },
    VnKeySlot { lower: 'ồ', upper: 'Ồ', key: Key::KEY_F4, xkb_name: "FK04", base_l1: "F4", base_l2: "F4", is_alpha: false },
    VnKeySlot { lower: 'ố', upper: 'Ố', key: Key::KEY_F5, xkb_name: "FK05", base_l1: "F5", base_l2: "F5", is_alpha: false },
    VnKeySlot { lower: 'ổ', upper: 'Ổ', key: Key::KEY_F6, xkb_name: "FK06", base_l1: "F6", base_l2: "F6", is_alpha: false },
    VnKeySlot { lower: 'ỗ', upper: 'Ỗ', key: Key::KEY_F7, xkb_name: "FK07", base_l1: "F7", base_l2: "F7", is_alpha: false },
    VnKeySlot { lower: 'ộ', upper: 'Ộ', key: Key::KEY_F8, xkb_name: "FK08", base_l1: "F8", base_l2: "F8", is_alpha: false },
    VnKeySlot { lower: 'ơ', upper: 'Ơ', key: Key::KEY_F9, xkb_name: "FK09", base_l1: "F9", base_l2: "F9", is_alpha: false },
    VnKeySlot { lower: 'ờ', upper: 'Ờ', key: Key::KEY_F10, xkb_name: "FK10", base_l1: "F10", base_l2: "F10", is_alpha: false },
    VnKeySlot { lower: 'ớ', upper: 'Ớ', key: Key::KEY_F11, xkb_name: "FK11", base_l1: "F11", base_l2: "F11", is_alpha: false },
    VnKeySlot { lower: 'ở', upper: 'Ở', key: Key::KEY_F12, xkb_name: "FK12", base_l1: "F12", base_l2: "F12", is_alpha: false },
    VnKeySlot { lower: 'ỡ', upper: 'Ỡ', key: Key::KEY_F13, xkb_name: "FK13", base_l1: "F13", base_l2: "F13", is_alpha: false },
    VnKeySlot { lower: 'ợ', upper: 'Ợ', key: Key::KEY_F14, xkb_name: "FK14", base_l1: "F14", base_l2: "F14", is_alpha: false },

    // --- Group `u` (11 pairs) ---
    VnKeySlot { lower: 'ù', upper: 'Ù', key: Key::KEY_F15, xkb_name: "FK15", base_l1: "F15", base_l2: "F15", is_alpha: false },
    VnKeySlot { lower: 'ú', upper: 'Ú', key: Key::KEY_F16, xkb_name: "FK16", base_l1: "F16", base_l2: "F16", is_alpha: false },
    VnKeySlot { lower: 'ủ', upper: 'Ủ', key: Key::KEY_F17, xkb_name: "FK17", base_l1: "F17", base_l2: "F17", is_alpha: false },
    VnKeySlot { lower: 'ũ', upper: 'Ũ', key: Key::KEY_F18, xkb_name: "FK18", base_l1: "F18", base_l2: "F18", is_alpha: false },
    VnKeySlot { lower: 'ụ', upper: 'Ụ', key: Key::KEY_F19, xkb_name: "FK19", base_l1: "F19", base_l2: "F19", is_alpha: false },
    VnKeySlot { lower: 'ư', upper: 'Ư', key: Key::KEY_F20, xkb_name: "FK20", base_l1: "F20", base_l2: "F20", is_alpha: false },
    VnKeySlot { lower: 'ừ', upper: 'Ừ', key: Key::KEY_F21, xkb_name: "FK21", base_l1: "F21", base_l2: "F21", is_alpha: false },
    VnKeySlot { lower: 'ứ', upper: 'Ứ', key: Key::KEY_F22, xkb_name: "FK22", base_l1: "F22", base_l2: "F22", is_alpha: false },
    VnKeySlot { lower: 'ử', upper: 'Ử', key: Key::KEY_F23, xkb_name: "FK23", base_l1: "F23", base_l2: "F23", is_alpha: false },
    VnKeySlot { lower: 'ữ', upper: 'Ữ', key: Key::KEY_F24, xkb_name: "FK24", base_l1: "F24", base_l2: "F24", is_alpha: false },
    VnKeySlot { lower: 'ự', upper: 'Ự', key: Key::KEY_MINUS, xkb_name: "AE11", base_l1: "minus", base_l2: "underscore", is_alpha: false },

    // --- Group `y` (5 pairs) ---
    VnKeySlot { lower: 'ỳ', upper: 'Ỳ', key: Key::KEY_EQUAL, xkb_name: "AE12", base_l1: "equal", base_l2: "plus", is_alpha: false },
    VnKeySlot { lower: 'ý', upper: 'Ý', key: Key::KEY_LEFTBRACE, xkb_name: "AD11", base_l1: "bracketleft", base_l2: "braceleft", is_alpha: false },
    VnKeySlot { lower: 'ỷ', upper: 'Ỷ', key: Key::KEY_RIGHTBRACE, xkb_name: "AD12", base_l1: "bracketright", base_l2: "braceright", is_alpha: false },
    VnKeySlot { lower: 'ỹ', upper: 'Ỹ', key: Key::KEY_SEMICOLON, xkb_name: "AC10", base_l1: "semicolon", base_l2: "colon", is_alpha: false },
    VnKeySlot { lower: 'ỵ', upper: 'Ỵ', key: Key::KEY_APOSTROPHE, xkb_name: "AC11", base_l1: "apostrophe", base_l2: "quotedbl", is_alpha: false },

    // --- Group `d` (1 pair) ---
    VnKeySlot { lower: 'đ', upper: 'Đ', key: Key::KEY_GRAVE, xkb_name: "TLDE", base_l1: "grave", base_l2: "asciitilde", is_alpha: false },
];

fn vn_lookup_table() -> &'static HashMap<char, KeyCombo> {
    static TABLE: OnceLock<HashMap<char, KeyCombo>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut map = HashMap::with_capacity(VN_KEY_SLOTS.len() * 2);
        for slot in VN_KEY_SLOTS {
            map.insert(
                slot.lower,
                KeyCombo {
                    key: slot.key,
                    level5: true,
                    shift: false,
                },
            );
            map.insert(
                slot.upper,
                KeyCombo {
                    key: slot.key,
                    level5: true,
                    shift: true,
                },
            );
        }
        map
    })
}

/// Static O(1) lookup mapping any of the 134 Vietnamese accented characters to its
/// `(Key, ISO_Level5_Shift, Shift)` uinput combination.
pub fn lookup_vn_char(c: char) -> Option<KeyCombo> {
    vn_lookup_table().get(&c).copied()
}

/// Format a Unicode scalar value as an XKB Unicode keysym (`U00E0`, `U1EC7`, etc.).
pub fn format_xkb_keysym(c: char) -> String {
    format!("U{:04X}", c as u32)
}

/// Generate the complete, standalone `vn_virtual.xkb` keymap file for `vi-uinput-virtual-kbd`.
/// Compatible with `xkbcommon` per-device `kb_file` (Hyprland, Sway, River, Niri) and `xkbcomp` (X11).
pub fn generate_xkb_keymap() -> String {
    let mut out = String::with_capacity(14_336);
    out.push_str(
        r#"// Auto-generated by vi-uinput-ime (src/xkb_gen.rs)
// Standalone Custom XKB Keymap for virtual keyboard "vi-uinput-virtual-kbd" (VID:0x1209 PID:0x0001)
// Maps all 134 Vietnamese accented characters onto Level5 (lower) and Level6 (upper)
// using KEY_HENKAN (<HENK>, evdev 92 / xkb 100) as ISO_Level5_Shift (Mod3) so Alt/Mod1, Super/Mod4,
// Ctrl, and Level1/Level2 remain 100% untouched standard US QWERTY.

xkb_keymap {
    xkb_keycodes "evdev_vi_uinput" {
        include "evdev+aliases(qwerty)"
    };

    xkb_types "vi_uinput_types" {
        include "complete"

        // Dedicated 8-level type where CapsLock (Lock) capitalizes Level1/Level2 normally
        // while NEVER inverting synthetic Level5/Level6 Vietnamese injection.
        type "VI_EIGHT_LEVEL" {
            modifiers = Shift + Lock + LevelThree + LevelFive;
            map[None] = Level1;
            map[Shift] = Level2;
            map[Lock] = Level2;
            map[Shift+Lock] = Level1;
            map[LevelThree] = Level3;
            map[Shift+LevelThree] = Level4;
            map[LevelFive] = Level5;
            map[LevelFive+Lock] = Level5;
            map[Shift+LevelFive] = Level6;
            map[Shift+LevelFive+Lock] = Level6;
            map[LevelThree+LevelFive] = Level7;
            map[Shift+LevelThree+LevelFive] = Level8;
            level_name[Level1] = "Base";
            level_name[Level2] = "Shift";
            level_name[Level3] = "Alt Base";
            level_name[Level4] = "Shift Alt";
            level_name[Level5] = "Vietnamese Lower";
            level_name[Level6] = "Vietnamese Upper";
            level_name[Level7] = "Level7";
            level_name[Level8] = "Level8";
        };
    };

    xkb_compatibility "vi_uinput_compat" {
        include "complete"
    };

    xkb_symbols "vi_uinput_134" {
        include "pc+us+inet(evdev)"
        name[Group1] = "English (US) + Vietnamese Direct uinput (134 chars)";

        // Bind <HENK> (KEY_HENKAN) cleanly as LevelFive (Mod3) without touching Alt/Mod1
        // or relying on deprecated `level5(modifier_mapping)` (removed in xkeyboard-config >= 2.43).
        virtual_modifiers LevelFive;
        replace key <HENK> {
            type[Group1] = "ONE_LEVEL",
            symbols[Group1] = [ ISO_Level5_Shift ],
            vmods = LevelFive
        };
        modifier_map Mod3 { <HENK> };

"#,
    );

    for slot in VN_KEY_SLOTS {
        let l5 = format_xkb_keysym(slot.lower);
        let l6 = format_xkb_keysym(slot.upper);
        let key_type = if slot.is_alpha {
            "VI_EIGHT_LEVEL"
        } else {
            "EIGHT_LEVEL"
        };
        let _ = writeln!(
            out,
            "        replace key <{xkb}> {{ type[Group1]=\"{ty}\", [ {l1}, {l2}, {l1}, {l2}, {l5}, {l6}, NoSymbol, NoSymbol ] }}; // {lo} / {up}",
            xkb = slot.xkb_name,
            ty = key_type,
            l1 = slot.base_l1,
            l2 = slot.base_l2,
            l5 = l5,
            l6 = l6,
            lo = slot.lower,
            up = slot.upper
        );
    }

    out.push_str(
        r#"    };

    xkb_geometry "pc(pc104)" {
        include "pc(pc104)"
    };
};
"#,
    );

    out
}

/// Generate a standard XKB RMLVO symbols layout (`~/.config/xkb/symbols/vi_uinput`)
/// for compositors with centralized seat-wide keymaps (GNOME/Mutter, KDE/KWin).
/// Because `<HENK>` (`KEY_HENKAN`) does not exist on physical US/EU keyboards and never touches
/// `Mod1` (`Alt`), loading this layout seat-wide via `gsettings` leaves normal English typing
/// and every desktop shortcut 100% intact while unlocking all 134 Vietnamese UTF-8 characters.
pub fn generate_gnome_xkb_symbols() -> String {
    let mut out = String::with_capacity(12_288);
    out.push_str(
        r#"// Auto-generated by vi-uinput-ime (src/xkb_gen.rs)
// Install to: ~/.config/xkb/symbols/vi_uinput (or /usr/share/X11/xkb/symbols/vi_uinput)
// Enable on GNOME / Mutter Wayland:
//   gsettings set org.gnome.desktop.input-sources show-all-sources true
//   gsettings set org.gnome.desktop.input-sources sources "[('xkb', 'vi_uinput')]"

default partial alphanumeric_keys modifier_keys
xkb_symbols "basic" {
    include "us(basic)"

    name[Group1] = "English (US) — vi-uinput-ime (134 VN chars)";

    // Dedicated non-Alt Level5 trigger (<HENK> = KEY_HENKAN, evdev 92).
    // Declaring `virtual_modifiers LevelFive` + `vmods = LevelFive` inline avoids
    // `include "level5(modifier_mapping)"` (removed in Arch xkeyboard-config >= 2.43)
    // while keeping LevelFive -> Mod3 working on older xkeyboard-config (<= 2.42) too.
    virtual_modifiers LevelFive;
    replace key <HENK> {
        type[Group1] = "ONE_LEVEL",
        symbols[Group1] = [ ISO_Level5_Shift ],
        vmods = LevelFive
    };
    modifier_map Mod3 { <HENK> };

"#,
    );

    for slot in VN_KEY_SLOTS {
        let l5 = format_xkb_keysym(slot.lower);
        let l6 = format_xkb_keysym(slot.upper);
        // Use standard EIGHT_LEVEL_ALPHABETIC (for A-Z so physical CapsLock works) and
        // EIGHT_LEVEL (for digits, F1-F24, punctuation) — both are built into xkeyboard-config
        // `types/level5` on Arch Linux / Debian / Fedora without needing custom types files.
        let key_type = if slot.is_alpha {
            "EIGHT_LEVEL_ALPHABETIC"
        } else {
            "EIGHT_LEVEL"
        };
        let _ = writeln!(
            out,
            "    replace key <{xkb}> {{ type[Group1]=\"{ty}\", [ {l1}, {l2}, {l1}, {l2}, {l5}, {l6}, NoSymbol, NoSymbol ] }}; // {lo} / {up}",
            xkb = slot.xkb_name,
            ty = key_type,
            l1 = slot.base_l1,
            l2 = slot.base_l2,
            l5 = l5,
            l6 = l6,
            lo = slot.lower,
            up = slot.upper
        );
    }

    out.push_str("};\n");
    out
}

/// Generate `~/.config/xkb/rules/evdev.xml` so GNOME Settings and `localectl` / libxkbregistry
/// recognize the `vi_uinput` layout cleanly.
pub fn generate_gnome_evdev_xml() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE xkbConfigRegistry SYSTEM "xkb.dtd">
<xkbConfigRegistry version="1.1">
  <layoutList>
    <layout>
      <configItem>
        <name>vi_uinput</name>
        <shortDescription>vi</shortDescription>
        <description>Vietnamese Direct uinput (vi-uinput-ime)</description>
        <languageList>
          <iso639Id>vie</iso639Id>
          <iso639Id>eng</iso639Id>
        </languageList>
      </configItem>
    </layout>
  </layoutList>
</xkbConfigRegistry>
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn maps_all_134_vietnamese_chars_bijectively_without_collisions() {
        assert_eq!(VN_KEY_SLOTS.len(), 67);

        let mut seen_chars = HashSet::new();
        let mut seen_combos = HashSet::new();
        let mut seen_keys = HashSet::new();
        let mut seen_xkb_names = HashSet::new();

        for slot in VN_KEY_SLOTS {
            assert!(seen_keys.insert(slot.key), "Duplicate physical key {:?}", slot.key);
            assert!(seen_xkb_names.insert(slot.xkb_name), "Duplicate XKB name {}", slot.xkb_name);
            assert!(seen_chars.insert(slot.lower), "Duplicate lower char {}", slot.lower);
            assert!(seen_chars.insert(slot.upper), "Duplicate upper char {}", slot.upper);

            let c_lo = lookup_vn_char(slot.lower).expect("lower char must exist");
            let c_up = lookup_vn_char(slot.upper).expect("upper char must exist");

            assert!(c_lo.level5 && !c_lo.shift);
            assert!(c_up.level5 && c_up.shift);
            assert_eq!(c_lo.key, slot.key);
            assert_eq!(c_up.key, slot.key);

            assert!(seen_combos.insert(c_lo), "Duplicate KeyCombo {:?}", c_lo);
            assert!(seen_combos.insert(c_up), "Duplicate KeyCombo {:?}", c_up);
        }

        assert_eq!(seen_chars.len(), 134);
        assert_eq!(seen_combos.len(), 134);
        assert!(lookup_vn_char('a').is_none());
        assert!(lookup_vn_char('Z').is_none());
    }

    #[test]
    fn standalone_and_gnome_rmlvo_keymaps_compile_cleanly_with_xkbcli() {
        let keymap = generate_xkb_keymap();
        for slot in VN_KEY_SLOTS {
            assert!(keymap.contains(&format_xkb_keysym(slot.lower)));
            assert!(keymap.contains(&format_xkb_keysym(slot.upper)));
        }

        // 1. Validate standalone vn_virtual.xkb (Hyprland / Sway / X11) via `xkbcli compile-keymap --from-xkb`
        // Note: libxkbcommon 1.5.0 had an upstream bug returning exit code 1 on `--from-xkb` even
        // when stdout emits the full compiled keymap; verify compiled output & zero ERROR diagnostics.
        if let Ok(mut child) = Command::new("xkbcli")
            .args(["compile-keymap", "--from-xkb"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(keymap.as_bytes()).unwrap();
            }
            let output = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stdout.contains("xkb_keymap {") && stdout.contains("modifier_map Mod3 { <HENK>"),
                "xkbcli compile-keymap --from-xkb did not produce valid keymap:\nstderr: {}",
                stderr
            );
            assert!(
                !stderr.contains("ERROR:"),
                "xkbcli reported ERROR on standalone keymap:\n{}",
                stderr
            );
        }

        // 2. Validate GNOME / Mutter RMLVO layout (`~/.config/xkb/symbols/vi_uinput`)
        let tmp_dir = std::env::temp_dir().join("vi_uinput_xkb_test");
        let sym_dir = tmp_dir.join("symbols");
        std::fs::create_dir_all(&sym_dir).unwrap();
        std::fs::write(sym_dir.join("vi_uinput"), generate_gnome_xkb_symbols()).unwrap();

        if let Ok(output) = Command::new("xkbcli")
            .args([
                "compile-keymap",
                "--include",
                tmp_dir.to_str().unwrap(),
                "--include-defaults",
                "--layout",
                "vi_uinput",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success()
                    && stdout.contains("xkb_keymap {")
                    && stdout.contains("modifier_map Mod3 { <HENK>"),
                "xkbcli RMLVO compile failed for GNOME layout:\n{}",
                stderr
            );
        }
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
