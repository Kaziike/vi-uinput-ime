use crate::config::ToneStyle;
use crate::engine::{diff_words_to_output, InputEngine};
use crate::input::emitter::OutputItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VowelShape {
    Plain,
    Circumflex, // â, ê, ô (6)
    Breve,      // ă (8)
    Horn,       // ơ, ư (7)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    None = 0,
    Acute = 1,     // sắc (1)
    Grave = 2,     // huyền (2)
    HookAbove = 3, // hỏi (3)
    Tilde = 4,     // ngã (4)
    DotBelow = 5,  // nặng (5)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransformOutcome {
    /// Diacritic/tone was applied to the current word.
    Applied,
    /// Repeated VNI modifier undid the previous diacritic/tone; caller must emit the undo diff + literal digit.
    Undone,
    /// Digit did not match any vowel/`d` in the current word; caller passes the digit through verbatim.
    NoMatch,
}

#[derive(Debug, Clone, Copy)]
struct Letter {
    base: char,
    upper: bool,
    shape: VowelShape,
    tone: Tone,
    d_bar: bool,
}

impl Letter {
    fn from_ascii(c: char) -> Self {
        Self {
            base: c.to_ascii_lowercase(),
            upper: c.is_ascii_uppercase(),
            shape: VowelShape::Plain,
            tone: Tone::None,
            d_bar: false,
        }
    }

    fn is_vowel(&self) -> bool {
        matches!(self.base, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
    }

    fn render(&self) -> char {
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
}

fn to_vn_upper(c: char) -> char {
    c.to_uppercase().next().unwrap_or(c)
}

fn compose_vn_vowel(base: char, shape: VowelShape, tone: Tone) -> Option<char> {
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

/// Pure VNI Vietnamese input engine:
/// - `1`=sắc, `2`=huyền, `3`=hỏi, `4`=ngã, `5`=nặng, `0`=xóa dấu
/// - `6`=mũ (â, ê, ô), `7`=móc (ơ, ư, uo→ươ), `8`=trăng (ă), `9`=đ
/// - Standalone digits (`123`, `2026`) or non-applicable digits (`b1`, `t6`, `a0`) return `None`
///   so the router forwards them untouched with full press/release tracking (zero stuck keys).
/// - Double-tapping a VNI digit (`a11`, `a66`, `u77`, `a88`, `d99`) cleanly undoes the mark and
///   emits the literal digit atomically.
pub struct VniEngine {
    tone_style: ToneStyle,
    letters: Vec<Letter>,
    rendered: String,
    disabled_for_word: bool,
}

impl VniEngine {
    pub fn new(tone_style: ToneStyle) -> Self {
        Self {
            tone_style,
            letters: Vec::with_capacity(16),
            rendered: String::with_capacity(32),
            disabled_for_word: false,
        }
    }

    #[allow(dead_code)]
    pub fn set_tone_style(&mut self, tone_style: ToneStyle) {
        self.tone_style = tone_style;
        self.reposition_existing_tone();
    }

    fn render_buffer(&self) -> String {
        self.letters.iter().map(Letter::render).collect()
    }

    fn nucleus_vowel_indices(&self) -> Vec<usize> {
        let mut raw: Vec<usize> = self
            .letters
            .iter()
            .enumerate()
            .filter_map(|(i, l)| if l.is_vowel() { Some(i) } else { None })
            .collect();

        if raw.len() >= 2 {
            let first = raw[0];
            if first > 0
                && self.letters[first - 1].base == 'q'
                && self.letters[first].base == 'u'
            {
                raw.remove(0);
            } else if first > 0
                && self.letters[first - 1].base == 'g'
                && self.letters[first].base == 'i'
                && !self.letters[first].d_bar
            {
                raw.remove(0);
            }
        }
        raw
    }

    fn has_coda(&self, vowels: &[usize]) -> bool {
        if let Some(&last_v) = vowels.last() {
            last_v + 1 < self.letters.len()
        } else {
            false
        }
    }

    fn pick_tone_target(&self) -> Option<usize> {
        let vowels = self.nucleus_vowel_indices();
        if vowels.is_empty() {
            return None;
        }
        if vowels.len() == 1 {
            return Some(vowels[0]);
        }

        let shaped: Vec<usize> = vowels
            .iter()
            .copied()
            .filter(|&i| self.letters[i].shape != VowelShape::Plain)
            .collect();
        if !shaped.is_empty() {
            return Some(*shaped.last().unwrap());
        }

        let has_coda = self.has_coda(&vowels);
        if vowels.len() >= 3 {
            return Some(vowels[1]);
        }

        let v0 = vowels[0];
        let v1 = vowels[1];
        let pair = (self.letters[v0].base, self.letters[v1].base);

        if has_coda {
            return Some(v1);
        }

        match pair {
            ('o', 'a') | ('o', 'e') | ('u', 'y') => match self.tone_style {
                ToneStyle::New => Some(v1),
                ToneStyle::Old => Some(v0),
            },
            _ => Some(v0),
        }
    }

    fn current_tone(&self) -> Tone {
        self.letters
            .iter()
            .find_map(|l| if l.tone != Tone::None { Some(l.tone) } else { None })
            .unwrap_or(Tone::None)
    }

    fn clear_all_tones(&mut self) {
        for l in &mut self.letters {
            l.tone = Tone::None;
        }
    }

    fn reposition_existing_tone(&mut self) {
        let tone = self.current_tone();
        if tone == Tone::None {
            return;
        }
        self.clear_all_tones();
        if let Some(idx) = self.pick_tone_target() {
            self.letters[idx].tone = tone;
        }
    }

    fn apply_tone(&mut self, tone: Tone) -> TransformOutcome {
        let Some(target) = self.pick_tone_target() else {
            return TransformOutcome::NoMatch;
        };
        if self.current_tone() == tone {
            self.clear_all_tones();
            self.disabled_for_word = true;
            return TransformOutcome::Undone;
        }
        self.clear_all_tones();
        self.letters[target].tone = tone;
        TransformOutcome::Applied
    }

    fn remove_tone(&mut self) -> TransformOutcome {
        if self.current_tone() == Tone::None {
            return TransformOutcome::NoMatch;
        }
        self.clear_all_tones();
        TransformOutcome::Applied
    }

    fn apply_d_bar(&mut self) -> TransformOutcome {
        for l in self.letters.iter_mut().rev() {
            if l.base == 'd' {
                if l.d_bar {
                    l.d_bar = false;
                    self.disabled_for_word = true;
                    return TransformOutcome::Undone;
                }
                l.d_bar = true;
                return TransformOutcome::Applied;
            }
        }
        TransformOutcome::NoMatch
    }

    fn apply_circumflex(&mut self) -> TransformOutcome {
        for i in (0..self.letters.len()).rev() {
            if matches!(self.letters[i].base, 'a' | 'e' | 'o') {
                if self.letters[i].shape == VowelShape::Circumflex {
                    self.letters[i].shape = VowelShape::Plain;
                    self.reposition_existing_tone();
                    self.disabled_for_word = true;
                    return TransformOutcome::Undone;
                }
                self.letters[i].shape = VowelShape::Circumflex;
                self.reposition_existing_tone();
                return TransformOutcome::Applied;
            }
        }
        TransformOutcome::NoMatch
    }

    fn apply_horn(&mut self) -> TransformOutcome {
        let len = self.letters.len();

        // Check `uo` cluster first (e.g., `du7o7` or `duo7` -> `dươ`)
        if len >= 2 {
            for i in (0..len - 1).rev() {
                if self.letters[i].base == 'u' && self.letters[i + 1].base == 'o' {
                    let is_qu = i > 0 && self.letters[i - 1].base == 'q';
                    if !is_qu {
                        if self.letters[i].shape == VowelShape::Horn
                            && self.letters[i + 1].shape == VowelShape::Horn
                        {
                            self.letters[i].shape = VowelShape::Plain;
                            self.letters[i + 1].shape = VowelShape::Plain;
                            self.reposition_existing_tone();
                            self.disabled_for_word = true;
                            return TransformOutcome::Undone;
                        }
                        self.letters[i].shape = VowelShape::Horn;
                        self.letters[i + 1].shape = VowelShape::Horn;
                        self.reposition_existing_tone();
                        return TransformOutcome::Applied;
                    }
                }
            }
        }

        // Single vowel horn (`o` -> `ơ`, `u` -> `ư`)
        for i in (0..len).rev() {
            if matches!(self.letters[i].base, 'o' | 'u') {
                if self.letters[i].base == 'u'
                    && i > 0
                    && self.letters[i - 1].base == 'q'
                    && i + 1 < len
                    && self.letters[i + 1].is_vowel()
                {
                    continue;
                }
                if self.letters[i].shape == VowelShape::Horn {
                    self.letters[i].shape = VowelShape::Plain;
                    self.reposition_existing_tone();
                    self.disabled_for_word = true;
                    return TransformOutcome::Undone;
                }
                self.letters[i].shape = VowelShape::Horn;
                self.reposition_existing_tone();
                return TransformOutcome::Applied;
            }
        }

        TransformOutcome::NoMatch
    }

    fn apply_breve(&mut self) -> TransformOutcome {
        for i in (0..self.letters.len()).rev() {
            if self.letters[i].base == 'a' {
                if self.letters[i].shape == VowelShape::Breve {
                    self.letters[i].shape = VowelShape::Plain;
                    self.reposition_existing_tone();
                    self.disabled_for_word = true;
                    return TransformOutcome::Undone;
                }
                self.letters[i].shape = VowelShape::Breve;
                self.reposition_existing_tone();
                return TransformOutcome::Applied;
            }
        }
        TransformOutcome::NoMatch
    }

    fn try_vni_digit(&mut self, c: char) -> TransformOutcome {
        if self.disabled_for_word || self.letters.is_empty() {
            return TransformOutcome::NoMatch;
        }
        match c {
            '1' => self.apply_tone(Tone::Acute),
            '2' => self.apply_tone(Tone::Grave),
            '3' => self.apply_tone(Tone::HookAbove),
            '4' => self.apply_tone(Tone::Tilde),
            '5' => self.apply_tone(Tone::DotBelow),
            '0' => self.remove_tone(),
            '6' => self.apply_circumflex(),
            '7' => self.apply_horn(),
            '8' => self.apply_breve(),
            '9' => self.apply_d_bar(),
            _ => TransformOutcome::NoMatch,
        }
    }
}

impl InputEngine for VniEngine {
    fn process_char(&mut self, c: char) -> Option<Vec<OutputItem>> {
        // Handle VNI modifier digits `0..9`
        if matches!(c, '0'..='9') {
            if self.letters.is_empty() {
                return None;
            }
            let old_rendered = self.rendered.clone();
            match self.try_vni_digit(c) {
                TransformOutcome::Applied => {
                    let new_rendered = self.render_buffer();
                    self.rendered = new_rendered.clone();
                    return Some(diff_words_to_output(&old_rendered, &new_rendered));
                }
                TransformOutcome::Undone => {
                    // Emit minimal undo diff (e.g., `á` -> `a`) + literal digit (`1`), then reset word
                    let new_rendered = self.render_buffer();
                    let mut batch = diff_words_to_output(&old_rendered, &new_rendered);
                    batch.push(OutputItem::AsciiChar(c));
                    self.reset();
                    return Some(batch);
                }
                TransformOutcome::NoMatch => {
                    // Non-applicable digit: reset active word and pass raw digit through verbatim
                    self.reset();
                    return None;
                }
            }
        }

        if !c.is_ascii_alphabetic() {
            self.reset();
            return None;
        }

        let old_rendered = self.rendered.clone();
        self.letters.push(Letter::from_ascii(c));

        // Auto-horn `o` -> `ơ` when typing a coda after `ưo` (e.g., `d9u7oc5` -> `được`)
        let len = self.letters.len();
        if len >= 3
            && self.letters[len - 3].base == 'u'
            && self.letters[len - 3].shape == VowelShape::Horn
            && self.letters[len - 2].base == 'o'
            && self.letters[len - 2].shape == VowelShape::Plain
        {
            self.letters[len - 2].shape = VowelShape::Horn;
        }
        self.reposition_existing_tone();

        let new_rendered = self.render_buffer();
        self.rendered = new_rendered.clone();
        Some(diff_words_to_output(&old_rendered, &new_rendered))
    }

    fn process_backspace(&mut self) {
        self.letters.pop();
        self.rendered = self.render_buffer();
        if self.letters.is_empty() {
            self.disabled_for_word = false;
        }
    }

    fn reset(&mut self) {
        self.letters.clear();
        self.rendered.clear();
        self.disabled_for_word = false;
    }

    fn current_word(&self) -> &str {
        &self.rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simulate_typing(engine: &mut VniEngine, input: &str) -> String {
        let mut screen: Vec<char> = Vec::new();
        for ch in input.chars() {
            match engine.process_char(ch) {
                Some(items) => {
                    for item in items {
                        match item {
                            OutputItem::Backspace => {
                                screen.pop();
                            }
                            OutputItem::VietnameseChar(c) | OutputItem::AsciiChar(c) => {
                                screen.push(c);
                            }
                            OutputItem::PassThrough { .. } => {}
                        }
                    }
                }
                None => screen.push(ch),
            }
        }
        screen.into_iter().collect()
    }

    #[test]
    fn vni_vietnamese_words_and_tones() {
        let mut engine = VniEngine::new(ToneStyle::New);
        assert_eq!(simulate_typing(&mut engine, "tie6ng1 vie6t5"), "tiếng việt");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "tra6u"), "trâu");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "Tra6u"), "Trâu");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "d9u7o7c5"), "được");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "d9uo7c5"), "được");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "toa1n"), "toán");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "hoa2"), "hoà");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "thuy1"), "thuý");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "to6i10"), "tôi");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "na8m 2026"), "năm 2026");
    }

    #[test]
    fn vni_old_tone_style() {
        let mut engine = VniEngine::new(ToneStyle::Old);
        assert_eq!(simulate_typing(&mut engine, "hoa2"), "hòa");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "thuy1"), "thúy");
    }

    #[test]
    fn vni_standalone_numbers_non_applicable_digits_and_double_tap_undo() {
        let mut engine = VniEngine::new(ToneStyle::New);
        // Standalone numbers pass straight through
        assert_eq!(simulate_typing(&mut engine, "1234567890"), "1234567890");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "2026"), "2026");
        engine.reset();
        // Non-applicable digits after consonants or without tone
        assert_eq!(simulate_typing(&mut engine, "b1"), "b1");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "t6"), "t6");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "k9"), "k9");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "a0"), "a0");
        engine.reset();
        // Double-tap undo cleanly restores base char and emits literal digit
        assert_eq!(simulate_typing(&mut engine, "a11"), "a1");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "a66"), "a6");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "u77"), "u7");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "a88"), "a8");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "d99"), "d9");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "user1123"), "user123");
    }
}
