//! Vietnamese syllable phonology validator for auto-restoring non-Vietnamese words.
//!
//! Splits a syllable into `(onset, vowels, coda)` (handling `qu` + vowel and `gi` + vowel
//! glides cleanly) and provides:
//! - [`validate_partial`]: real-time check while typing (catches `f/j/w/z`, English onset
//!   clusters, invalid codas, and vowel-after-coda immediately).
//! - [`validate_final`]: end-of-word check on `Space` (catches invalid checked-coda tones
//!   like `pỏt`/`tẽt`, invalid vowel clusters like `cáe`/`báe`, and closed open-diphthongs
//!   like `máin`).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VowelShape {
    Plain,
    Circumflex, // â, ê, ô
    Breve,      // ă
    Horn,       // ơ, ư
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    None = 0,
    Acute = 1,     // sắc (s / 1)
    Grave = 2,     // huyền (f / 2)
    HookAbove = 3, // hỏi (r / 3)
    Tilde = 4,     // ngã (x / 4)
    DotBelow = 5,  // nặng (j / 5)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Letter {
    pub base: char, // lowercase ASCII 'a'..'z'
    pub upper: bool,
    pub shape: VowelShape,
    pub tone: Tone,
    pub d_bar: bool, // true for đ/Đ
}

impl Letter {
    pub fn from_ascii(c: char) -> Self {
        Self {
            base: c.to_ascii_lowercase(),
            upper: c.is_ascii_uppercase(),
            shape: VowelShape::Plain,
            tone: Tone::None,
            d_bar: false,
        }
    }

    pub fn is_vowel(&self) -> bool {
        matches!(self.base, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
    }

    pub fn render(&self) -> char {
        let ch = if self.base == 'd' && self.d_bar {
            'đ'
        } else {
            compose_vn_vowel(self.base, self.shape, self.tone).unwrap_or(self.base)
        };
        if self.upper {
            to_vn_upper(ch)
        } else {
            ch
        }
    }

    /// Renders lowercase base + shape without tone mark (e.g. `á` -> `a`, `ắ` -> `ă`, `đ` -> `đ`).
    pub fn shape_char(&self) -> char {
        if self.base == 'd' && self.d_bar {
            'đ'
        } else {
            compose_vn_vowel(self.base, self.shape, Tone::None).unwrap_or(self.base)
        }
    }
}

pub fn to_vn_upper(c: char) -> char {
    c.to_uppercase().next().unwrap_or(c)
}

pub fn compose_vn_vowel(base: char, shape: VowelShape, tone: Tone) -> Option<char> {
    let idx = tone as usize;
    let table: Option<[char; 6]> = match (base, shape) {
        ('a', VowelShape::Plain) => Some(['a', 'á', 'à', 'ả', 'ã', 'ạ']),
        ('a', VowelShape::Breve) => Some(['ă', 'ắ', 'ằ', 'ẳ', 'ẵ', 'ặ']),
        ('a', VowelShape::Circumflex) => Some(['â', 'ấ', 'ầ', 'ẩ', 'ẫ', 'ậ']),
        ('e', VowelShape::Plain) => Some(['e', 'é', 'è', 'ẻ', 'ẽ', 'ẹ']),
        ('e', VowelShape::Circumflex) => Some(['ê', 'ế', 'ề', 'ể', 'ễ', 'ệ']),
        ('i', VowelShape::Plain) => Some(['i', 'í', 'ì', 'ỉ', 'ĩ', 'ị']),
        ('o', VowelShape::Plain) => Some(['o', 'ó', 'ò', 'ỏ', 'õ', 'ọ']),
        ('o', VowelShape::Circumflex) => Some(['ô', 'ố', 'ồ', 'ổ', 'ỗ', 'ộ']),
        ('o', VowelShape::Horn) => Some(['ơ', 'ớ', 'ờ', 'ở', 'ỡ', 'ợ']),
        ('u', VowelShape::Plain) => Some(['u', 'ú', 'ù', 'ủ', 'ũ', 'ụ']),
        ('u', VowelShape::Horn) => Some(['ư', 'ứ', 'ừ', 'ử', 'ữ', 'ự']),
        ('y', VowelShape::Plain) => Some(['y', 'ý', 'ỳ', 'ỷ', 'ỹ', 'ỵ']),
        _ => None,
    };
    table.map(|t| t[idx])
}

/// All valid Vietnamese initial consonant clusters (note: `k` is intentionally allowed
/// before any vowel to preserve common Vietnamese shorthand like `ko`, `k`).
pub const VALID_ONSETS: &[&str] = &[
    "", "b", "c", "d", "đ", "g", "h", "k", "l", "m", "n", "p", "q", "r", "s", "t", "v", "x",
    "ch", "gh", "gi", "kh", "ng", "nh", "ph", "qu", "th", "tr", "ngh",
];

/// All valid Vietnamese final consonants. Notice every prefix (`c`, `n`) is also in this list.
pub const VALID_CODAS: &[&str] = &["", "c", "ch", "m", "n", "ng", "nh", "p", "t"];

/// Checked codas (âm tắc cuối): only allow `Acute` (sắc) or `DotBelow` (nặng).
const CHECKED_CODAS: &[&str] = &["c", "ch", "p", "t"];

/// Single vowels valid both open (no coda) and closed (with coda).
const SINGLE_VOWELS_OPEN_OR_CLOSED: &[&str] = &["a", "e", "ê", "i", "o", "ô", "ơ", "u", "ư", "y"];

/// Single vowels that MUST be followed by a coda.
const SINGLE_VOWELS_CLOSED_ONLY: &[&str] = &["ă", "â"];

/// Diphthongs that MUST NOT have a coda (open syllable only).
const DIPHTHONGS_OPEN_ONLY: &[&str] = &[
    "ai", "ao", "au", "ay", "âu", "ây", "eo", "êu", "ia", "iu", "oi", "ôi", "ơi", "ua", "ui",
    "uơ", "ưa", "ưi", "ưu",
];

/// Diphthongs valid both with and without a coda.
const DIPHTHONGS_OPEN_OR_CLOSED: &[&str] = &["oa", "oe", "uê", "uy"];

/// Diphthongs that MUST be followed by a coda.
const DIPHTHONGS_CLOSED_ONLY: &[&str] = &["oă", "uâ", "iê", "yê", "uô", "ươ"];

/// Triphthongs that MUST NOT have a coda.
const TRIPHTHONGS_OPEN_ONLY: &[&str] = &[
    "iêu", "oai", "oao", "oay", "oeo", "uai", "uao", "uay", "uây", "uôi", "ươi", "ươu", "uya",
    "uyu", "yêu",
];

/// Triphthongs that MUST be followed by a coda.
const TRIPHTHONGS_CLOSED_ONLY: &[&str] = &["uyê"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitSyllable {
    pub onset: String,
    pub vowels: String,
    pub coda: String,
    pub tone: Tone,
    pub has_vowel_after_coda: bool,
}

fn is_non_vn_ascii_letter(base: char) -> bool {
    matches!(base, 'f' | 'j' | 'w' | 'z')
}

/// Splits `letters` into lowercase `(onset, vowels, coda)` with `qu`+vowel and `gi`+vowel glides.
pub fn split_syllable(letters: &[Letter]) -> SplitSyllable {
    let tone = letters
        .iter()
        .find_map(|l| if l.tone != Tone::None { Some(l.tone) } else { None })
        .unwrap_or(Tone::None);

    let Some(first_v) = letters.iter().position(Letter::is_vowel) else {
        let onset: String = letters.iter().map(Letter::shape_char).collect();
        return SplitSyllable {
            onset,
            vowels: String::new(),
            coda: String::new(),
            tone,
            has_vowel_after_coda: false,
        };
    };

    let mut onset_end = first_v;

    // Handle `qu` onset glide (`qu` alone or `qu` + vowel; `qư` is not a glide)
    if first_v == 1
        && letters[0].base == 'q'
        && letters[1].base == 'u'
        && letters[1].shape == VowelShape::Plain
    {
        onset_end = 2;
    } else if first_v == 1
        && letters[0].base == 'g'
        && letters[1].base == 'i'
        && letters[1].shape == VowelShape::Plain
        && letters.len() > 2
        && letters[2].is_vowel()
    {
        // `gi` + another vowel (e.g. `gia`, `giết`, `giường`) -> onset is `gi`
        // whereas `gi`, `gì`, `gìn` keep `g` as onset and `i` as vowel.
        onset_end = 2;
    }

    let mut vowel_end = onset_end;
    while vowel_end < letters.len() && letters[vowel_end].is_vowel() {
        vowel_end += 1;
    }

    let has_vowel_after_coda = letters[vowel_end..].iter().any(Letter::is_vowel);

    let onset: String = letters[..onset_end].iter().map(Letter::shape_char).collect();
    let vowels: String = letters[onset_end..vowel_end]
        .iter()
        .map(Letter::shape_char)
        .collect();
    let coda: String = letters[vowel_end..].iter().map(Letter::shape_char).collect();

    SplitSyllable {
        onset,
        vowels,
        coda,
        tone,
        has_vowel_after_coda,
    }
}

fn is_valid_onset_or_prefix(onset: &str) -> bool {
    VALID_ONSETS.iter().any(|&v| v.starts_with(onset))
}

fn is_valid_complete_onset(onset: &str) -> bool {
    // Bare `q` is only valid as a prefix while typing `qu`, never before another vowel.
    onset != "q" && VALID_ONSETS.contains(&onset)
}

fn is_valid_coda(coda: &str) -> bool {
    VALID_CODAS.contains(&coda)
}

fn is_valid_vowel_nucleus(vowels: &str, has_coda: bool) -> bool {
    if vowels.is_empty() {
        return false;
    }
    if SINGLE_VOWELS_OPEN_OR_CLOSED.contains(&vowels) || DIPHTHONGS_OPEN_OR_CLOSED.contains(&vowels)
    {
        return true;
    }
    if has_coda {
        SINGLE_VOWELS_CLOSED_ONLY.contains(&vowels)
            || DIPHTHONGS_CLOSED_ONLY.contains(&vowels)
            || TRIPHTHONGS_CLOSED_ONLY.contains(&vowels)
    } else {
        DIPHTHONGS_OPEN_ONLY.contains(&vowels) || TRIPHTHONGS_OPEN_ONLY.contains(&vowels)
    }
}

/// Immediate phonology check run while typing (ONLY after a plain letter is appended
/// and no Telex/VNI transformation occurred).
///
/// Returns `false` if the current buffer cannot possibly form a valid Vietnamese syllable.
pub fn validate_partial(letters: &[Letter]) -> bool {
    if letters.is_empty() {
        return true;
    }

    // Rule A.1 & A.5: `f`, `j`, `w`, `z` never occur in Vietnamese orthography.
    if is_non_vn_ascii_letter(letters[0].base)
        || letters.iter().any(|l| is_non_vn_ascii_letter(l.base))
    {
        return false;
    }

    let split = split_syllable(letters);

    // Rule A.3: Vowel appearing after a coda (e.g. `còni` from `confi`, `retu` from `return`).
    if split.has_vowel_after_coda {
        return false;
    }

    // Rule A.2: Still typing onset (no vowels yet).
    if split.vowels.is_empty() {
        // Consonant appearing after `qu` without a vowel (e.g. `qun`) is immediately invalid.
        if !split.coda.is_empty() {
            return false;
        }
        return is_valid_onset_or_prefix(&split.onset);
    }

    // Once a vowel has appeared, the onset is finalized and must be a complete valid onset.
    if !is_valid_complete_onset(&split.onset) {
        return false;
    }

    // Rule A.4: Coda must be in `VALID_CODAS` (all valid coda prefixes are also in `VALID_CODAS`).
    if !is_valid_coda(&split.coda) {
        return false;
    }

    true
}

/// End-of-word phonology check run on `Space` before committing the word to history.
///
/// Returns `false` if the completed syllable violates Vietnamese phonology rules and
/// should be restored to its exact raw ASCII keystrokes.
pub fn validate_final(letters: &[Letter]) -> bool {
    if letters.is_empty() {
        return true;
    }

    if letters.iter().any(|l| is_non_vn_ascii_letter(l.base)) {
        return false;
    }

    let split = split_syllable(letters);

    if split.has_vowel_after_coda {
        return false;
    }

    // Rule B.1: Complete onset must be valid (rejects bare `q` without `u` + vowel).
    if !is_valid_complete_onset(&split.onset) {
        return false;
    }

    // Rule B.2: Coda must be valid.
    if !is_valid_coda(&split.coda) {
        return false;
    }

    // Rule B.4: Vowel nucleus + coda compatibility check.
    let has_coda = !split.coda.is_empty();
    if !is_valid_vowel_nucleus(&split.vowels, has_coda) {
        return false;
    }

    // Rule B.3: Checked codas (`c`, `ch`, `p`, `t`) ONLY allow Acute (sắc) or DotBelow (nặng).
    if CHECKED_CODAS.contains(&split.coda.as_str())
        && !matches!(split.tone, Tone::Acute | Tone::DotBelow)
    {
        return false;
    }

    true
}
