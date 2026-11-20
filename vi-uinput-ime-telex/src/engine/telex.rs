use crate::config::ToneStyle;
use crate::engine::{diff_words_to_output, InputEngine};
use crate::input::emitter::OutputItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VowelShape {
    Plain,
    Circumflex, // â, ê, ô
    Breve,      // ă
    Horn,       // ơ, ư
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    None = 0,
    Acute = 1,     // sắc (s)
    Grave = 2,     // huyền (f)
    HookAbove = 3, // hỏi (r)
    Tilde = 4,     // ngã (x)
    DotBelow = 5,  // nặng (j)
}

#[derive(Debug, Clone, Copy)]
struct Letter {
    base: char, // lowercase ASCII 'a'..'z'
    upper: bool,
    shape: VowelShape,
    tone: Tone,
    d_bar: bool, // true for đ/Đ
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

/// Pure Telex Vietnamese input engine.
/// Only consumes ASCII letters (`a..z`, `A..Z`); digits (`0..9`) and symbols are never intercepted.
pub struct TelexEngine {
    tone_style: ToneStyle,
    letters: Vec<Letter>,
    rendered: String,
    disabled_for_word: bool,
}

impl TelexEngine {
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

    fn apply_tone(&mut self, tone: Tone) -> bool {
        let Some(target) = self.pick_tone_target() else {
            return false;
        };
        let existing = self.current_tone();
        if existing == tone {
            self.clear_all_tones();
            self.disabled_for_word = true;
            return false;
        }
        self.clear_all_tones();
        self.letters[target].tone = tone;
        true
    }

    fn remove_tone(&mut self) -> bool {
        if self.current_tone() == Tone::None {
            return false;
        }
        self.clear_all_tones();
        true
    }

    fn apply_d_bar(&mut self) -> bool {
        for l in self.letters.iter_mut().rev() {
            if l.base == 'd' {
                if l.d_bar {
                    l.d_bar = false;
                    self.disabled_for_word = true;
                    return false;
                }
                l.d_bar = true;
                return true;
            }
        }
        false
    }

    fn apply_circumflex(&mut self, target_base: char) -> bool {
        for i in (0..self.letters.len()).rev() {
            if self.letters[i].base == target_base {
                if self.letters[i].shape == VowelShape::Circumflex {
                    self.letters[i].shape = VowelShape::Plain;
                    self.reposition_existing_tone();
                    self.disabled_for_word = true;
                    return false;
                }
                self.letters[i].shape = VowelShape::Circumflex;
                self.reposition_existing_tone();
                return true;
            }
        }
        false
    }

    fn apply_horn_or_breve(&mut self) -> bool {
        let len = self.letters.len();

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
                            return false;
                        }
                        self.letters[i].shape = VowelShape::Horn;
                        self.letters[i + 1].shape = VowelShape::Horn;
                        self.reposition_existing_tone();
                        return true;
                    }
                }
            }
        }

        for i in (0..len).rev() {
            match self.letters[i].base {
                'a' => {
                    if self.letters[i].shape == VowelShape::Breve {
                        self.letters[i].shape = VowelShape::Plain;
                        self.reposition_existing_tone();
                        self.disabled_for_word = true;
                        return false;
                    }
                    self.letters[i].shape = VowelShape::Breve;
                    self.reposition_existing_tone();
                    return true;
                }
                'o' | 'u' => {
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
                        return false;
                    }
                    self.letters[i].shape = VowelShape::Horn;
                    self.reposition_existing_tone();
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    fn try_transform(&mut self, c: char) -> bool {
        if self.disabled_for_word {
            return false;
        }
        match c.to_ascii_lowercase() {
            's' => self.apply_tone(Tone::Acute),
            'f' => self.apply_tone(Tone::Grave),
            'r' => self.apply_tone(Tone::HookAbove),
            'x' => self.apply_tone(Tone::Tilde),
            'j' => self.apply_tone(Tone::DotBelow),
            'z' => self.remove_tone(),
            'd' => self.apply_d_bar(),
            'a' => self.apply_circumflex('a'),
            'e' => self.apply_circumflex('e'),
            'o' => self.apply_circumflex('o'),
            'w' => {
                if self.apply_horn_or_breve() {
                    true
                } else {
                    let mut u = Letter::from_ascii(if c.is_ascii_uppercase() { 'U' } else { 'u' });
                    u.shape = VowelShape::Horn;
                    self.letters.push(u);
                    self.reposition_existing_tone();
                    true
                }
            }
            _ => false,
        }
    }
}

impl InputEngine for TelexEngine {
    fn process_char(&mut self, c: char) -> Option<Vec<OutputItem>> {
        if !c.is_ascii_alphabetic() {
            self.reset();
            return None;
        }

        let old_rendered = self.rendered.clone();
        let transformed = self.try_transform(c);

        if !transformed {
            self.letters.push(Letter::from_ascii(c));
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
        }

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

    fn simulate_typing(engine: &mut TelexEngine, input: &str) -> String {
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
    fn telex_complex_sentences_and_numbers() {
        let mut engine = TelexEngine::new(ToneStyle::New);
        assert_eq!(simulate_typing(&mut engine, "tieesng vieejt"), "tiếng việt");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "traau"), "trâu");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "Traau"), "Trâu");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "duwowcj"), "dược");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "dduwowcj"), "được");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "duwowcjd"), "được");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "dduowcj"), "được");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "khuya"), "khuya");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "toios "), "tối ");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "w"), "ư");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "hoaf"), "hoà");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "thuys"), "thuý");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "toiosz"), "tôi");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "nawm 2026"), "năm 2026");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "1234567890"), "1234567890");
    }

    #[test]
    fn telex_old_tone_style() {
        let mut engine = TelexEngine::new(ToneStyle::Old);
        assert_eq!(simulate_typing(&mut engine, "hoaf"), "hòa");
        engine.reset();
        assert_eq!(simulate_typing(&mut engine, "thuys"), "thúy");
    }

    #[test]
    fn minimal_common_prefix_diff_for_toios() {
        let mut engine = TelexEngine::new(ToneStyle::New);
        assert_eq!(engine.process_char('t'), Some(vec![OutputItem::AsciiChar('t')]));
        assert_eq!(engine.process_char('o'), Some(vec![OutputItem::AsciiChar('o')]));
        assert_eq!(engine.process_char('i'), Some(vec![OutputItem::AsciiChar('i')]));
        assert_eq!(
            engine.process_char('o'),
            Some(vec![
                OutputItem::Backspace,
                OutputItem::Backspace,
                OutputItem::VietnameseChar('ô'),
                OutputItem::AsciiChar('i'),
            ])
        );
        assert_eq!(
            engine.process_char('s'),
            Some(vec![
                OutputItem::Backspace,
                OutputItem::Backspace,
                OutputItem::VietnameseChar('ố'),
                OutputItem::AsciiChar('i'),
            ])
        );
    }
}
