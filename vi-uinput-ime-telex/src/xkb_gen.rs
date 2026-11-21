use evdev::Key;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// Virtual uinput keyboard identity (must match compositor `device {}` block on Hyprland/Sway).
pub const VIRTUAL_KBD_NAME: &str = "vi-uinput-virtual-kbd";
pub const VIRTUAL_KBD_VENDOR_ID: u16 = 0x1209; // pid.codes open hardware
pub const VIRTUAL_KBD_PRODUCT_ID: u16 = 0x0001;

/// Modifier key used to activate XKB `ISO_Level5_Shift` (`Mod3` / `LevelFive`).
/// Uses `KEY_MACRO` (`<I120>`, Linux EV_KEY 112, XKB 120) because:
///   1. `+inet(evdev)` does NOT map `<I120>`, so its `ISO_Level5_Shift` keysym is NEVER
///      overwritten (unlike `<HENK>`, which `+inet(evdev)` overwrites back to `Henkan_Mode`).
///   2. Never touches `Mod1` (`Alt`), `Mod4` (`Super`), or `Control`.
pub const LEVEL5_MODIFIER_KEY: Key = Key::KEY_MACRO;

/// Companion modifier key used together with `LEVEL5_MODIFIER_KEY` to reach XKB Level7/Level8
/// (`ISO_Level3_Shift` / `Mod5` / `LevelThree`).
/// Uses `KEY_SETUP` (`<I149>`, Linux EV_KEY 141, XKB 149) which is also completely untouched by
/// `pc`, `us`, and `+inet(evdev)`.
pub const LEVEL3_MODIFIER_KEY: Key = Key::KEY_SETUP;

/// Key combination emitted to `/dev/uinput` to produce a single precomposed Vietnamese character
/// via our custom XKB keymap (`vn_virtual.xkb` or GNOME/KDE RMLVO `symbols/vi_uinput`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub key: Key,
    pub level5: bool,
    pub level3: bool,
    pub shift: bool,
}

/// Static descriptor for one standard US 104-key printable carrier slot holding up to 4 Vietnamese
/// characters across XKB Level5/Level6 and Level7/Level8 (`EIGHT_LEVEL` for digits/punctuation,
/// `EIGHT_LEVEL_SEMIALPHABETIC` for US letters).
#[derive(Debug, Clone, Copy)]
pub struct VnKeySlot {
    pub key: Key,
    pub xkb_name: &'static str,
    pub base_l1: &'static str,
    pub base_l2: &'static str,
    /// True for alphabetic keys (`a..z`) so XKB uses `EIGHT_LEVEL_SEMIALPHABETIC` (preserving
    /// native CapsLock on Level1/Level2 without ever falling back to Level1 on Level5/6).
    pub is_alpha: bool,
    /// Level5 (lower) & Level6 (upper) via `LEVEL5_MODIFIER_KEY` (+ optional Shift)
    pub l5_lower: char,
    pub l6_upper: char,
    /// Optional Level7 (lower) & Level8 (upper) via `LEVEL5_MODIFIER_KEY` + `LEVEL3_MODIFIER_KEY` (+ optional Shift)
    pub l78: Option<(char, char)>,
}

impl VnKeySlot {
    pub const fn xkb_type(&self) -> &'static str {
        if self.is_alpha {
            "EIGHT_LEVEL_SEMIALPHABETIC"
        } else {
            "EIGHT_LEVEL"
        }
    }
}

/// Canonical 34-carrier table covering all 134 precomposed Vietnamese diacritic characters
/// (67 lowercase + 67 uppercase) across XKB Levels 5–8:
///   - 100% standard US 104-key printable keys ONLY (`evdev <= 53`, guaranteed Chromium `DomCode` + `VKEY`)
///   - ZERO `KEY_F1..KEY_F24` (eliminates `oo` -> `F3`, `owis` -> `F11`, `uw` -> `XF86AudioMicMute`)
///   - ZERO exotic/non-US/NoSymbol evdev codes (`AB11`, `JPCM`, `AE13`, `I154..I230`), which
///     Chromium/Ozone Wayland drops as `DomCode::NONE` / `VKEY_UNKNOWN` (fixing `đ`, `ư + dấu`, `i + dấu`)
///   - ZERO keys overwritten by `+inet(evdev)` or `srvr_ctrl(fkey2vt)`
///
/// Carrier keys used (34 total, all `evdev <= 53`):
///   - 10 top-row digits: `KEY_1..KEY_0` (`AE01..AE10`, `EIGHT_LEVEL`)
///   - 11 standard US punctuation keys: `- = [ ] ; ' \` \ , . /` (`EIGHT_LEVEL`)
///   - 13 standard US QWERTY letters: `Q W E R T Y U I O P A S D` (`EIGHT_LEVEL_SEMIALPHABETIC`)
pub const VN_KEY_SLOTS: [VnKeySlot; 34] = [
    // 1..10: Top-row digits (Level5/6: a-group 1..10 | Level7/8: o-group 1..10)
    VnKeySlot { key: Key::KEY_1, xkb_name: "AE01", base_l1: "1", base_l2: "exclam", is_alpha: false, l5_lower: 'à', l6_upper: 'À', l78: Some(('ò', 'Ò')) },
    VnKeySlot { key: Key::KEY_2, xkb_name: "AE02", base_l1: "2", base_l2: "at", is_alpha: false, l5_lower: 'á', l6_upper: 'Á', l78: Some(('ó', 'Ó')) },
    VnKeySlot { key: Key::KEY_3, xkb_name: "AE03", base_l1: "3", base_l2: "numbersign", is_alpha: false, l5_lower: 'ả', l6_upper: 'Ả', l78: Some(('ỏ', 'Ỏ')) },
    VnKeySlot { key: Key::KEY_4, xkb_name: "AE04", base_l1: "4", base_l2: "dollar", is_alpha: false, l5_lower: 'ã', l6_upper: 'Ã', l78: Some(('õ', 'Õ')) },
    VnKeySlot { key: Key::KEY_5, xkb_name: "AE05", base_l1: "5", base_l2: "percent", is_alpha: false, l5_lower: 'ạ', l6_upper: 'Ạ', l78: Some(('ọ', 'Ọ')) },
    VnKeySlot { key: Key::KEY_6, xkb_name: "AE06", base_l1: "6", base_l2: "asciicircum", is_alpha: false, l5_lower: 'ă', l6_upper: 'Ă', l78: Some(('ô', 'Ô')) },
    VnKeySlot { key: Key::KEY_7, xkb_name: "AE07", base_l1: "7", base_l2: "ampersand", is_alpha: false, l5_lower: 'ằ', l6_upper: 'Ằ', l78: Some(('ồ', 'Ồ')) },
    VnKeySlot { key: Key::KEY_8, xkb_name: "AE08", base_l1: "8", base_l2: "asterisk", is_alpha: false, l5_lower: 'ắ', l6_upper: 'Ắ', l78: Some(('ố', 'Ố')) },
    VnKeySlot { key: Key::KEY_9, xkb_name: "AE09", base_l1: "9", base_l2: "parenleft", is_alpha: false, l5_lower: 'ẳ', l6_upper: 'Ẳ', l78: Some(('ổ', 'Ổ')) },
    VnKeySlot { key: Key::KEY_0, xkb_name: "AE10", base_l1: "0", base_l2: "parenright", is_alpha: false, l5_lower: 'ẵ', l6_upper: 'Ẵ', l78: Some(('ỗ', 'Ỗ')) },

    // 11..21: Standard US punctuation (Level5/6: a-group 11..17 + e-group 1..4 | Level7/8: o-group 11..17 + u-group 1..4)
    VnKeySlot { key: Key::KEY_MINUS, xkb_name: "AE11", base_l1: "minus", base_l2: "underscore", is_alpha: false, l5_lower: 'ặ', l6_upper: 'Ặ', l78: Some(('ộ', 'Ộ')) },
    VnKeySlot { key: Key::KEY_EQUAL, xkb_name: "AE12", base_l1: "equal", base_l2: "plus", is_alpha: false, l5_lower: 'â', l6_upper: 'Â', l78: Some(('ơ', 'Ơ')) },
    VnKeySlot { key: Key::KEY_LEFTBRACE, xkb_name: "AD11", base_l1: "bracketleft", base_l2: "braceleft", is_alpha: false, l5_lower: 'ầ', l6_upper: 'Ầ', l78: Some(('ờ', 'Ờ')) },
    VnKeySlot { key: Key::KEY_RIGHTBRACE, xkb_name: "AD12", base_l1: "bracketright", base_l2: "braceright", is_alpha: false, l5_lower: 'ấ', l6_upper: 'Ấ', l78: Some(('ớ', 'Ớ')) },
    VnKeySlot { key: Key::KEY_SEMICOLON, xkb_name: "AC10", base_l1: "semicolon", base_l2: "colon", is_alpha: false, l5_lower: 'ẩ', l6_upper: 'Ẩ', l78: Some(('ở', 'Ở')) },
    VnKeySlot { key: Key::KEY_APOSTROPHE, xkb_name: "AC11", base_l1: "apostrophe", base_l2: "quotedbl", is_alpha: false, l5_lower: 'ẫ', l6_upper: 'Ẫ', l78: Some(('ỡ', 'Ỡ')) },
    VnKeySlot { key: Key::KEY_GRAVE, xkb_name: "TLDE", base_l1: "grave", base_l2: "asciitilde", is_alpha: false, l5_lower: 'ậ', l6_upper: 'Ậ', l78: Some(('ợ', 'Ợ')) },
    VnKeySlot { key: Key::KEY_BACKSLASH, xkb_name: "BKSL", base_l1: "backslash", base_l2: "bar", is_alpha: false, l5_lower: 'è', l6_upper: 'È', l78: Some(('ù', 'Ù')) },
    VnKeySlot { key: Key::KEY_COMMA, xkb_name: "AB08", base_l1: "comma", base_l2: "less", is_alpha: false, l5_lower: 'é', l6_upper: 'É', l78: Some(('ú', 'Ú')) },
    VnKeySlot { key: Key::KEY_DOT, xkb_name: "AB09", base_l1: "period", base_l2: "greater", is_alpha: false, l5_lower: 'ẻ', l6_upper: 'Ẻ', l78: Some(('ủ', 'Ủ')) },
    VnKeySlot { key: Key::KEY_SLASH, xkb_name: "AB10", base_l1: "slash", base_l2: "question", is_alpha: false, l5_lower: 'ẽ', l6_upper: 'Ẽ', l78: Some(('ũ', 'Ũ')) },

    // 22..34: Standard US QWERTY letters (Level5/6: e-group 5..11 + i-group 1..5 + đ | Level7/8: u-group 5..11 + y-group 1..5)
    VnKeySlot { key: Key::KEY_Q, xkb_name: "AD01", base_l1: "q", base_l2: "Q", is_alpha: true, l5_lower: 'ẹ', l6_upper: 'Ẹ', l78: Some(('ụ', 'Ụ')) },
    VnKeySlot { key: Key::KEY_W, xkb_name: "AD02", base_l1: "w", base_l2: "W", is_alpha: true, l5_lower: 'ê', l6_upper: 'Ê', l78: Some(('ư', 'Ư')) },
    VnKeySlot { key: Key::KEY_E, xkb_name: "AD03", base_l1: "e", base_l2: "E", is_alpha: true, l5_lower: 'ề', l6_upper: 'Ề', l78: Some(('ừ', 'Ừ')) },
    VnKeySlot { key: Key::KEY_R, xkb_name: "AD04", base_l1: "r", base_l2: "R", is_alpha: true, l5_lower: 'ế', l6_upper: 'Ế', l78: Some(('ứ', 'Ứ')) },
    VnKeySlot { key: Key::KEY_T, xkb_name: "AD05", base_l1: "t", base_l2: "T", is_alpha: true, l5_lower: 'ể', l6_upper: 'Ể', l78: Some(('ử', 'Ử')) },
    VnKeySlot { key: Key::KEY_Y, xkb_name: "AD06", base_l1: "y", base_l2: "Y", is_alpha: true, l5_lower: 'ễ', l6_upper: 'Ễ', l78: Some(('ữ', 'Ữ')) },
    VnKeySlot { key: Key::KEY_U, xkb_name: "AD07", base_l1: "u", base_l2: "U", is_alpha: true, l5_lower: 'ệ', l6_upper: 'Ệ', l78: Some(('ự', 'Ự')) },
    VnKeySlot { key: Key::KEY_I, xkb_name: "AD08", base_l1: "i", base_l2: "I", is_alpha: true, l5_lower: 'ì', l6_upper: 'Ì', l78: Some(('ỳ', 'Ỳ')) },
    VnKeySlot { key: Key::KEY_O, xkb_name: "AD09", base_l1: "o", base_l2: "O", is_alpha: true, l5_lower: 'í', l6_upper: 'Í', l78: Some(('ý', 'Ý')) },
    VnKeySlot { key: Key::KEY_P, xkb_name: "AD10", base_l1: "p", base_l2: "P", is_alpha: true, l5_lower: 'ỉ', l6_upper: 'Ỉ', l78: Some(('ỷ', 'Ỷ')) },
    VnKeySlot { key: Key::KEY_A, xkb_name: "AC01", base_l1: "a", base_l2: "A", is_alpha: true, l5_lower: 'ĩ', l6_upper: 'Ĩ', l78: Some(('ỹ', 'Ỹ')) },
    VnKeySlot { key: Key::KEY_S, xkb_name: "AC02", base_l1: "s", base_l2: "S", is_alpha: true, l5_lower: 'ị', l6_upper: 'Ị', l78: Some(('ỵ', 'Ỵ')) },
    VnKeySlot { key: Key::KEY_D, xkb_name: "AC03", base_l1: "d", base_l2: "D", is_alpha: true, l5_lower: 'đ', l6_upper: 'Đ', l78: None },
];

fn vn_lookup_table() -> &'static HashMap<char, KeyCombo> {
    static TABLE: OnceLock<HashMap<char, KeyCombo>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut map = HashMap::with_capacity(134);
        for slot in VN_KEY_SLOTS {
            map.insert(
                slot.l5_lower,
                KeyCombo {
                    key: slot.key,
                    level5: true,
                    level3: false,
                    shift: false,
                },
            );
            map.insert(
                slot.l6_upper,
                KeyCombo {
                    key: slot.key,
                    level5: true,
                    level3: false,
                    shift: true,
                },
            );
            if let Some((l7, l8)) = slot.l78 {
                map.insert(
                    l7,
                    KeyCombo {
                        key: slot.key,
                        level5: true,
                        level3: true,
                        shift: false,
                    },
                );
                map.insert(
                    l8,
                    KeyCombo {
                        key: slot.key,
                        level5: true,
                        level3: true,
                        shift: true,
                    },
                );
            }
        }
        map
    })
}

/// Static O(1) lookup mapping any of the 134 Vietnamese accented characters to its
/// `(Key, Level5, Level3, Shift)` uinput combination.
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
    let mut out = String::with_capacity(10_240);
    out.push_str(
        r#"// Auto-generated by vi-uinput-ime (src/xkb_gen.rs)
// Standalone Custom XKB Keymap for virtual keyboard "vi-uinput-virtual-kbd" (VID:0x1209 PID:0x0001)
// Maps all 134 Vietnamese accented characters onto Levels 5–8 of 34 standard US 104-key printable
// carriers using KEY_MACRO (<I120>, evdev 112) as ISO_Level5_Shift (Mod3) and KEY_SETUP (<I149>,
// evdev 141) as ISO_Level3_Shift (Mod5). Zero F1–F24, XF86 media, or exotic non-US keys are used.

xkb_keymap {
    xkb_keycodes "evdev_vi_uinput" {
        include "evdev+aliases(qwerty)"
    };

    xkb_types "vi_uinput_types" {
        include "complete"
    };

    xkb_compatibility "vi_uinput_compat" {
        include "complete"
    };

    xkb_symbols "vi_uinput_134" {
        include "pc+us+inet(evdev)"
        name[Group1] = "English (US) + Vietnamese Direct uinput (134 chars)";

        // Bind <I120> (KEY_MACRO) -> LevelFive (Mod3) and <I149> (KEY_SETUP) -> LevelThree (Mod5).
        // Neither keycode is mapped by `pc`, `us`, or `inet(evdev)`.
        virtual_modifiers LevelThree, LevelFive;
        replace key <I120> {
            type[Group1] = "ONE_LEVEL",
            symbols[Group1] = [ ISO_Level5_Shift ],
            vmods = LevelFive
        };
        modifier_map Mod3 { <I120> };

        replace key <I149> {
            type[Group1] = "ONE_LEVEL",
            symbols[Group1] = [ ISO_Level3_Shift ],
            vmods = LevelThree
        };
        modifier_map Mod5 { <I149> };

"#,
    );

    for slot in VN_KEY_SLOTS {
        let l5 = format_xkb_keysym(slot.l5_lower);
        let l6 = format_xkb_keysym(slot.l6_upper);
        let (l7, l8, comment_78) = match slot.l78 {
            Some((c7, c8)) => (
                format_xkb_keysym(c7),
                format_xkb_keysym(c8),
                format!(" | L7/8: {c7}/{c8}"),
            ),
            None => ("NoSymbol".to_string(), "NoSymbol".to_string(), String::new()),
        };
        let _ = writeln!(
            out,
            "        replace key <{xkb}> {{ type[Group1]=\"{ty}\", [ {l1}, {l2}, {l1}, {l2}, {l5}, {l6}, {l7}, {l8} ] }}; // L5/6: {lo5}/{up6}{c78}",
            xkb = slot.xkb_name,
            ty = slot.xkb_type(),
            l1 = slot.base_l1,
            l2 = slot.base_l2,
            l5 = l5,
            l6 = l6,
            l7 = l7,
            l8 = l8,
            lo5 = slot.l5_lower,
            up6 = slot.l6_upper,
            c78 = comment_78
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
/// Uses `<I120>` (`KEY_MACRO`) and `<I149>` (`KEY_SETUP`) which are completely uncontested by
/// `+inet(evdev)`, and zero `F1..F24` keys so browsers/apps never fire `F3`/`F11`/`XF86*`.
pub fn generate_gnome_xkb_symbols() -> String {
    let mut out = String::with_capacity(8_192);
    out.push_str(
        r#"// Auto-generated by vi-uinput-ime (src/xkb_gen.rs)
// Install to: ~/.config/xkb/symbols/vi_uinput (or /etc/xkb/symbols/vi_uinput)
// Enable on GNOME / Mutter Wayland:
//   gsettings set org.gnome.desktop.input-sources show-all-sources true
//   gsettings set org.gnome.desktop.input-sources sources "[('xkb', 'vi_uinput')]"

default partial alphanumeric_keys modifier_keys
xkb_symbols "basic" {
    include "us(basic)"

    name[Group1] = "English (US) — vi-uinput-ime (134 VN chars)";

    // Dedicated LevelFive (Mod3) and LevelThree (Mod5) triggers on uncontested evdev keycodes:
    //   <I120> = KEY_MACRO (evdev 112), <I149> = KEY_SETUP (evdev 141)
    // Neither keycode is touched by `pc`, `us`, or `+inet(evdev)`, and avoiding
    // `include "level5(modifier_mapping)"` keeps compatibility with xkeyboard-config >= 2.43.
    virtual_modifiers LevelThree, LevelFive;
    replace key <I120> {
        type[Group1] = "ONE_LEVEL",
        symbols[Group1] = [ ISO_Level5_Shift ],
        vmods = LevelFive
    };
    modifier_map Mod3 { <I120> };

    replace key <I149> {
        type[Group1] = "ONE_LEVEL",
        symbols[Group1] = [ ISO_Level3_Shift ],
        vmods = LevelThree
    };
    modifier_map Mod5 { <I149> };

"#,
    );

    for slot in VN_KEY_SLOTS {
        let l5 = format_xkb_keysym(slot.l5_lower);
        let l6 = format_xkb_keysym(slot.l6_upper);
        let (l7, l8, comment_78) = match slot.l78 {
            Some((c7, c8)) => (
                format_xkb_keysym(c7),
                format_xkb_keysym(c8),
                format!(" | L7/8: {c7}/{c8}"),
            ),
            None => ("NoSymbol".to_string(), "NoSymbol".to_string(), String::new()),
        };
        let _ = writeln!(
            out,
            "    replace key <{xkb}> {{ type[Group1]=\"{ty}\", [ {l1}, {l2}, {l1}, {l2}, {l5}, {l6}, {l7}, {l8} ] }}; // L5/6: {lo5}/{up6}{c78}",
            xkb = slot.xkb_name,
            ty = slot.xkb_type(),
            l1 = slot.base_l1,
            l2 = slot.base_l2,
            l5 = l5,
            l6 = l6,
            l7 = l7,
            l8 = l8,
            lo5 = slot.l5_lower,
            up6 = slot.l6_upper,
            c78 = comment_78
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
    fn maps_all_134_vietnamese_chars_bijectively_without_f_keys_or_collisions() {
        assert_eq!(VN_KEY_SLOTS.len(), 34);

        let mut seen_chars = HashSet::new();
        let mut seen_combos = HashSet::new();
        let mut seen_keys = HashSet::new();
        let mut seen_xkb_names = HashSet::new();

        for slot in VN_KEY_SLOTS {
            assert!(seen_keys.insert(slot.key), "Duplicate physical key {:?}", slot.key);
            assert!(seen_xkb_names.insert(slot.xkb_name), "Duplicate XKB name {}", slot.xkb_name);
            assert!(
                !slot.xkb_name.starts_with("FK"),
                "Function key {} must never be used as a Vietnamese carrier",
                slot.xkb_name
            );
            assert!(
                slot.key.code() <= Key::KEY_SLASH.code(),
                "Carrier {:?} ({}) must be a standard US 104-key printable key (evdev <= 53) for Chromium DomCode compatibility",
                slot.key,
                slot.xkb_name
            );
            assert_ne!(slot.base_l1, "NoSymbol", "Slot {} must not have NoSymbol on Level1", slot.xkb_name);
            assert_ne!(slot.base_l2, "NoSymbol", "Slot {} must not have NoSymbol on Level2", slot.xkb_name);

            assert!(seen_chars.insert(slot.l5_lower), "Duplicate char {}", slot.l5_lower);
            assert!(seen_chars.insert(slot.l6_upper), "Duplicate char {}", slot.l6_upper);

            let c5 = lookup_vn_char(slot.l5_lower).expect("L5 char must exist");
            let c6 = lookup_vn_char(slot.l6_upper).expect("L6 char must exist");
            assert!(c5.level5 && !c5.level3 && !c5.shift);
            assert!(c6.level5 && !c6.level3 && c6.shift);
            assert_eq!(c5.key, slot.key);
            assert_eq!(c6.key, slot.key);
            assert!(seen_combos.insert(c5), "Duplicate KeyCombo {:?}", c5);
            assert!(seen_combos.insert(c6), "Duplicate KeyCombo {:?}", c6);

            if let Some((l7, l8)) = slot.l78 {
                assert!(seen_chars.insert(l7), "Duplicate char {}", l7);
                assert!(seen_chars.insert(l8), "Duplicate char {}", l8);
                let c7 = lookup_vn_char(l7).expect("L7 char must exist");
                let c8 = lookup_vn_char(l8).expect("L8 char must exist");
                assert!(c7.level5 && c7.level3 && !c7.shift);
                assert!(c8.level5 && c8.level3 && c8.shift);
                assert_eq!(c7.key, slot.key);
                assert_eq!(c8.key, slot.key);
                assert!(seen_combos.insert(c7), "Duplicate KeyCombo {:?}", c7);
                assert!(seen_combos.insert(c8), "Duplicate KeyCombo {:?}", c8);
            }
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
            assert!(keymap.contains(&format_xkb_keysym(slot.l5_lower)));
            assert!(keymap.contains(&format_xkb_keysym(slot.l6_upper)));
            if let Some((l7, l8)) = slot.l78 {
                assert!(keymap.contains(&format_xkb_keysym(l7)));
                assert!(keymap.contains(&format_xkb_keysym(l8)));
            }
        }

        // 1. Validate standalone vn_virtual.xkb (Hyprland / Sway / X11)
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
                stdout.contains("xkb_keymap {")
                    && stdout.contains("modifier_map Mod3 { <I120>")
                    && stdout.contains("ISO_Level5_Shift")
                    && stdout.contains("ISO_Level3_Shift"),
                "xkbcli compile-keymap --from-xkb did not produce valid keymap:\nstderr: {}",
                stderr
            );
            assert!(
                !stderr.contains("ERROR:"),
                "xkbcli reported ERROR on standalone keymap:\n{}",
                stderr
            );
        }

        // 2. Validate GNOME / KDE RMLVO layout (`~/.config/xkb/symbols/vi_uinput`)
        //    Verify `+inet(evdev)` does NOT clobber <I120> or <I149>!
        let tmp_dir = std::env::temp_dir().join("vi_uinput_xkb_test_telex");
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
                    && stdout.contains("modifier_map Mod3 { <I120>")
                    && stdout.contains("ISO_Level5_Shift")
                    && stdout.contains("ISO_Level3_Shift"),
                "xkbcli RMLVO compile failed or clobbered modifiers for GNOME/KDE layout:\n{}",
                stderr
            );
        }
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
