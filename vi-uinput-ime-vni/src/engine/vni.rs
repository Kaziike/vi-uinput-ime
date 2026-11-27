use crate::config::ToneStyle;
use crate::engine::{diff_words_to_output, InputEngine};
use crate::input::emitter::OutputItem;
use std::collections::VecDeque;

/// Maximum number of word/separator entries retained in the history ring buffer.
pub const MAX_HISTORY_ENTRIES: usize = 15;

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
    /// Repeated VNI modifier undid the previous diacritic/tone; caller emits undo diff + literal digit.
    Undone,
    /// Digit did not match any vowel/`d` in the current word; caller passes the digit through verbatim.
    NoMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Complete internal state of a single Vietnamese syllable/word (case, vowel shapes, tones, rendered text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyllableState {
    letters: Vec<Letter>,
    rendered: String,
    disabled_for_word: bool,
}

impl SyllableState {
    fn new() -> Self {
        Self {
            letters: Vec::with_capacity(16),
            rendered: String::with_capacity(32),
            disabled_for_word: false,
        }
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

    fn pick_tone_target(&self, tone_style: ToneStyle) -> Option<usize> {
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
            ('o', 'a') | ('o', 'e') | ('u', 'y') => match tone_style {
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

    fn reposition_existing_tone(&mut self, tone_style: ToneStyle) {
        let tone = self.current_tone();
        if tone == Tone::None {
            return;
        }
        self.clear_all_tones();
        if let Some(idx) = self.pick_tone_target(tone_style) {
            self.letters[idx].tone = tone;
        }
    }

    fn apply_tone(&mut self, tone: Tone, tone_style: ToneStyle) -> TransformOutcome {
        let Some(target) = self.pick_tone_target(tone_style) else {
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

    /// Applies circumflex (`6` -> `â`, `ê`, `ô`), allowing overwrite from `ă`/`ơ` (e.g. `Năng`+`6` -> `Nâng`).
    fn apply_circumflex(&mut self, tone_style: ToneStyle) -> TransformOutcome {
        for i in (0..self.letters.len()).rev() {
            if matches!(self.letters[i].base, 'a' | 'e' | 'o') {
                if self.letters[i].shape == VowelShape::Circumflex {
                    self.letters[i].shape = VowelShape::Plain;
                    self.reposition_existing_tone(tone_style);
                    self.disabled_for_word = true;
                    return TransformOutcome::Undone;
                }
                if self.letters[i].base == 'o'
                    && i > 0
                    && self.letters[i - 1].base == 'u'
                    && self.letters[i - 1].shape == VowelShape::Horn
                {
                    self.letters[i - 1].shape = VowelShape::Plain;
                }
                self.letters[i].shape = VowelShape::Circumflex;
                self.reposition_existing_tone(tone_style);
                return TransformOutcome::Applied;
            }
        }
        TransformOutcome::NoMatch
    }

    /// Applies horn (`7` -> `ơ`, `ư`, `ươ`), allowing overwrite from `ô` (e.g. `cô`+`7` -> `cơ`, `buôn`+`7` -> `bươn`).
    fn apply_horn(&mut self, tone_style: ToneStyle) -> TransformOutcome {
        let len = self.letters.len();

        if len >= 2 {
            for i in (0..len - 1).rev() {
                if self.letters[i].base == 'u' && self.letters[i + 1].base == 'o' {
                    let is_qu = i > 0 && self.letters[i - 1].base == 'q';
                    if is_qu {
                        if self.letters[i + 1].shape == VowelShape::Horn {
                            self.letters[i + 1].shape = VowelShape::Plain;
                            self.reposition_existing_tone(tone_style);
                            self.disabled_for_word = true;
                            return TransformOutcome::Undone;
                        }
                        self.letters[i + 1].shape = VowelShape::Horn;
                        self.reposition_existing_tone(tone_style);
                        return TransformOutcome::Applied;
                    }

                    let has_trailing = i + 2 < len;
                    if has_trailing {
                        if self.letters[i].shape == VowelShape::Horn
                            && self.letters[i + 1].shape == VowelShape::Horn
                        {
                            self.letters[i].shape = VowelShape::Plain;
                            self.letters[i + 1].shape = VowelShape::Plain;
                            self.reposition_existing_tone(tone_style);
                            self.disabled_for_word = true;
                            return TransformOutcome::Undone;
                        }
                        self.letters[i].shape = VowelShape::Horn;
                        self.letters[i + 1].shape = VowelShape::Horn;
                        self.reposition_existing_tone(tone_style);
                        return TransformOutcome::Applied;
                    } else {
                        // Open `uo` (e.g. `thuo73` -> `thuở`): horn `o` only; auto-expands `u` -> `ư` on trailing char.
                        if self.letters[i + 1].shape == VowelShape::Horn {
                            self.letters[i].shape = VowelShape::Plain;
                            self.letters[i + 1].shape = VowelShape::Plain;
                            self.reposition_existing_tone(tone_style);
                            self.disabled_for_word = true;
                            return TransformOutcome::Undone;
                        }
                        self.letters[i + 1].shape = VowelShape::Horn;
                        self.reposition_existing_tone(tone_style);
                        return TransformOutcome::Applied;
                    }
                }
            }
        }

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
                    self.reposition_existing_tone(tone_style);
                    self.disabled_for_word = true;
                    return TransformOutcome::Undone;
                }
                self.letters[i].shape = VowelShape::Horn;
                self.reposition_existing_tone(tone_style);
                return TransformOutcome::Applied;
            }
        }

        TransformOutcome::NoMatch
    }

    /// Applies breve (`8` -> `ă`), allowing overwrite from `â` (e.g. `Nâng`+`8` -> `Năng`).
    fn apply_breve(&mut self, tone_style: ToneStyle) -> TransformOutcome {
        for i in (0..self.letters.len()).rev() {
            if self.letters[i].base == 'a' {
                if self.letters[i].shape == VowelShape::Breve {
                    self.letters[i].shape = VowelShape::Plain;
                    self.reposition_existing_tone(tone_style);
                    self.disabled_for_word = true;
                    return TransformOutcome::Undone;
                }
                self.letters[i].shape = VowelShape::Breve;
                self.reposition_existing_tone(tone_style);
                return TransformOutcome::Applied;
            }
        }
        TransformOutcome::NoMatch
    }

    fn try_vni_digit(&mut self, c: char, tone_style: ToneStyle) -> TransformOutcome {
        if self.disabled_for_word || self.letters.is_empty() {
            return TransformOutcome::NoMatch;
        }
        match c {
            '1' => self.apply_tone(Tone::Acute, tone_style),
            '2' => self.apply_tone(Tone::Grave, tone_style),
            '3' => self.apply_tone(Tone::HookAbove, tone_style),
            '4' => self.apply_tone(Tone::Tilde, tone_style),
            '5' => self.apply_tone(Tone::DotBelow, tone_style),
            '0' => self.remove_tone(),
            '6' => self.apply_circumflex(tone_style),
            '7' => self.apply_horn(tone_style),
            '8' => self.apply_breve(tone_style),
            '9' => self.apply_d_bar(),
            _ => TransformOutcome::NoMatch,
        }
    }
}

/// Entry stored in the bounded history ring buffer for Word History Restoration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryEntry {
    Word(SyllableState),
    Separator(char),
}

/// Pure VNI Vietnamese input engine with Word History Restoration & vowel diacritic overwrite:
/// - `1`=sắc, `2`=huyền, `3`=hỏi, `4`=ngã, `5`=nặng, `0`=xóa dấu
/// - `6`=mũ (â, ê, ô), `7`=móc (ơ, ư, uo→ươ), `8`=trăng (ă), `9`=đ
pub struct VniEngine {
    tone_style: ToneStyle,
    active_word: Option<SyllableState>,
    history: VecDeque<HistoryEntry>,
}

impl VniEngine {
    pub fn new(tone_style: ToneStyle) -> Self {
        Self {
            tone_style,
            active_word: None,
            history: VecDeque::with_capacity(MAX_HISTORY_ENTRIES),
        }
    }

    #[allow(dead_code)]
    pub fn set_tone_style(&mut self, tone_style: ToneStyle) {
        self.tone_style = tone_style;
        if let Some(word) = &mut self.active_word {
            word.reposition_existing_tone(tone_style);
            word.rendered = word.render_buffer();
        }
    }

    #[allow(dead_code)]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    fn push_history(&mut self, entry: HistoryEntry) {
        while self.history.len() >= MAX_HISTORY_ENTRIES {
            self.history.pop_front();
        }
        self.history.push_back(entry);
    }
}

impl InputEngine for VniEngine {
    fn process_char(&mut self, c: char) -> Option<Vec<OutputItem>> {
        if c == ' ' {
            self.process_separator(' ');
            return None;
        }

        // Handle VNI modifier digits `0..9`
        if matches!(c, '0'..='9') {
            let tone_style = self.tone_style;
            let Some(word) = self.active_word.as_mut() else {
                // Standalone digit emitted on screen -> invalidate history so Backspace stays in sync.
                self.clear_all();
                return None;
            };
            let old_rendered = word.rendered.clone();
            match word.try_vni_digit(c, tone_style) {
                TransformOutcome::Applied => {
                    let new_rendered = word.render_buffer();
                    word.rendered = new_rendered.clone();
                    return Some(diff_words_to_output(&old_rendered, &new_rendered));
                }
                TransformOutcome::Undone => {
                    let new_rendered = word.render_buffer();
                    let mut batch = diff_words_to_output(&old_rendered, &new_rendered);
                    batch.push(OutputItem::AsciiChar(c));
                    self.clear_all();
                    return Some(batch);
                }
                TransformOutcome::NoMatch => {
                    self.clear_all();
                    return None;
                }
            }
        }

        if !c.is_ascii_alphabetic() {
            self.clear_all();
            return None;
        }

        let tone_style = self.tone_style;
        let word = self.active_word.get_or_insert_with(SyllableState::new);
        let old_rendered = word.rendered.clone();
        word.letters.push(Letter::from_ascii(c));

        // Auto-expand `ưo + trailing` and non-`qu` `uơ + trailing` -> `ươ + trailing`
        let len = word.letters.len();
        if len >= 3 {
            if word.letters[len - 3].base == 'u'
                && word.letters[len - 3].shape == VowelShape::Horn
                && word.letters[len - 2].base == 'o'
                && word.letters[len - 2].shape == VowelShape::Plain
            {
                word.letters[len - 2].shape = VowelShape::Horn;
            }
            let is_qu = len >= 4 && word.letters[len - 4].base == 'q';
            if !is_qu
                && word.letters[len - 3].base == 'u'
                && word.letters[len - 3].shape == VowelShape::Plain
                && word.letters[len - 2].base == 'o'
                && word.letters[len - 2].shape == VowelShape::Horn
            {
                word.letters[len - 3].shape = VowelShape::Horn;
            }
        }
        word.reposition_existing_tone(tone_style);

        let new_rendered = word.render_buffer();
        word.rendered = new_rendered.clone();
        Some(diff_words_to_output(&old_rendered, &new_rendered))
    }

    fn process_separator(&mut self, sep: char) {
        if let Some(word) = self.active_word.take() {
            if !word.letters.is_empty() {
                self.push_history(HistoryEntry::Word(word));
            }
        }
        self.push_history(HistoryEntry::Separator(sep));
    }

    fn process_backspace(&mut self) -> Option<Vec<OutputItem>> {
        // Case A: active_word currently has characters.
        if let Some(mut word) = self.active_word.take() {
            let old_rendered = word.rendered.clone();
            let mut expected_if_raw_bs = old_rendered.clone();
            expected_if_raw_bs.pop();

            word.letters.pop();

            if word.letters.is_empty() {
                // Active word is now empty; leave active_word = None WITHOUT popping history
                // because the preceding separator is still on screen.
                return None;
            }

            word.reposition_existing_tone(self.tone_style);
            let new_rendered = word.render_buffer();
            word.rendered = new_rendered.clone();
            self.active_word = Some(word);

            if new_rendered == expected_if_raw_bs {
                return None;
            }

            return Some(diff_words_to_output(&old_rendered, &new_rendered));
        }

        // Case B: active_word is None — pop separator and restore preceding Word if directly underneath.
        if let Some(HistoryEntry::Separator(_)) = self.history.pop_back() {
            if matches!(self.history.back(), Some(HistoryEntry::Word(_))) {
                if let Some(HistoryEntry::Word(restored)) = self.history.pop_back() {
                    self.active_word = Some(restored);
                }
            }
        }
        None
    }

    fn clear_all(&mut self) {
        self.active_word = None;
        self.history.clear();
    }

    fn current_word(&self) -> &str {
        self.active_word
            .as_ref()
            .map(|w| w.rendered.as_str())
            .unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_batch(screen: &mut Vec<char>, items: &[OutputItem]) {
        for item in items {
            match *item {
                OutputItem::Backspace => {
                    screen.pop();
                }
                OutputItem::VietnameseChar(c) | OutputItem::AsciiChar(c) => {
                    screen.push(c);
                }
                OutputItem::PassThrough { .. } | OutputItem::SyncCapsLock(_) => {}
            }
        }
    }

    fn simulate_typing(engine: &mut VniEngine, input: &str) -> String {
        let mut screen: Vec<char> = Vec::new();
        for ch in input.chars() {
            match engine.process_char(ch) {
                Some(items) => apply_batch(&mut screen, &items),
                None => screen.push(ch),
            }
        }
        screen.into_iter().collect()
    }

    fn sim_backspace(engine: &mut VniEngine, screen: &mut Vec<char>) -> Option<Vec<OutputItem>> {
        let res = engine.process_backspace();
        match &res {
            Some(items) => apply_batch(screen, items),
            None => {
                screen.pop();
            }
        }
        res
    }

    fn sim_char(engine: &mut VniEngine, screen: &mut Vec<char>, ch: char) -> Option<Vec<OutputItem>> {
        let res = engine.process_char(ch);
        match &res {
            Some(items) => apply_batch(screen, items),
            None => screen.push(ch),
        }
        res
    }

    #[test]
    fn vni_vietnamese_words_and_tones() {
        let mut engine = VniEngine::new(ToneStyle::New);
        assert_eq!(simulate_typing(&mut engine, "tie6ng1 vie6t5"), "tiếng việt");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "tra6u"), "trâu");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "Tra6u"), "Trâu");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "d9u7o7c5"), "được");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "d9uo7c5"), "được");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "toa1n"), "toán");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "hoa2"), "hoà");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "thuy1"), "thuý");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "to6i10"), "tôi");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "na8m 2026"), "năm 2026");
    }

    #[test]
    fn vni_old_tone_style_and_backspace_tone_reposition() {
        let mut engine = VniEngine::new(ToneStyle::Old);
        assert_eq!(simulate_typing(&mut engine, "hoa2"), "hòa");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "thuy1"), "thúy");
        engine.clear_all();

        let mut screen: Vec<char> = Vec::new();
        for ch in "toan1".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(screen.iter().collect::<String>(), "toán");
        let diff = sim_backspace(&mut engine, &mut screen);
        assert_eq!(
            diff,
            Some(vec![
                OutputItem::Backspace,
                OutputItem::Backspace,
                OutputItem::Backspace,
                OutputItem::VietnameseChar('ó'),
                OutputItem::AsciiChar('a'),
            ])
        );
        assert_eq!(screen.iter().collect::<String>(), "tóa");
    }

    #[test]
    fn vni_standalone_numbers_non_applicable_digits_and_double_tap_undo() {
        let mut engine = VniEngine::new(ToneStyle::New);
        assert_eq!(simulate_typing(&mut engine, "1234567890"), "1234567890");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "2026"), "2026");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "b1"), "b1");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "t6"), "t6");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "k9"), "k9");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "a0"), "a0");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "a11"), "a1");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "a66"), "a6");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "u77"), "u7");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "a88"), "a8");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "d99"), "d9");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "user1123"), "user123");
    }

    #[test]
    fn vni_vowel_diacritic_overwrite_both_directions() {
        let mut engine = VniEngine::new(ToneStyle::New);
        assert_eq!(simulate_typing(&mut engine, "Na8ng6"), "Nâng");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "Na6ng8"), "Năng");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "buo7n6"), "buôn");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "buo6n7"), "bươn");
    }

    #[test]
    fn history_test_1_nang_space_bs_6_produces_nang_circumflex_with_minimal_diff() {
        let mut engine = VniEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();

        for ch in "Na8ng".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(screen.iter().collect::<String>(), "Năng");

        sim_char(&mut engine, &mut screen, ' ');
        assert_eq!(screen.iter().collect::<String>(), "Năng ");
        assert_eq!(engine.history_len(), 2);

        // Backspace deletes space and restores "Năng"
        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        assert_eq!(screen.iter().collect::<String>(), "Năng");
        assert_eq!(engine.current_word(), "Năng");

        // Typing '6' overwrites `ă` -> `â` with exact minimal diff: 3 BS + "âng"
        let batch = sim_char(&mut engine, &mut screen, '6');
        assert_eq!(
            batch,
            Some(vec![
                OutputItem::Backspace,
                OutputItem::Backspace,
                OutputItem::Backspace,
                OutputItem::VietnameseChar('â'),
                OutputItem::AsciiChar('n'),
                OutputItem::AsciiChar('g'),
            ])
        );
        assert_eq!(screen.iter().collect::<String>(), "Nâng");
    }

    #[test]
    fn history_test_2_double_space_single_bs_does_not_restore() {
        let mut engine = VniEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();

        for ch in "Na8ng  ".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        assert_eq!(engine.current_word(), "");

        // Typing 'a' starts a brand new word ("Năng a")
        let batch = sim_char(&mut engine, &mut screen, 'a');
        assert_eq!(batch, Some(vec![OutputItem::AsciiChar('a')]));
        assert_eq!(screen.iter().collect::<String>(), "Năng a");
    }

    #[test]
    fn history_test_3_double_space_double_bs_restores_word() {
        let mut engine = VniEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();

        for ch in "Na8ng  ".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        assert_eq!(engine.current_word(), "");
        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        assert_eq!(engine.current_word(), "Năng");

        sim_char(&mut engine, &mut screen, '6');
        assert_eq!(screen.iter().collect::<String>(), "Nâng");
    }

    #[test]
    fn history_test_4_delete_entire_second_word_and_space_restores_first_word() {
        let mut engine = VniEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();

        for ch in "To6i Na8ng".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(screen.iter().collect::<String>(), "Tôi Năng");

        for _ in 0..4 {
            assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        }
        assert_eq!(screen.iter().collect::<String>(), "Tôi ");
        assert_eq!(engine.current_word(), "");

        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        assert_eq!(screen.iter().collect::<String>(), "Tôi");
        assert_eq!(engine.current_word(), "Tôi");

        sim_char(&mut engine, &mut screen, '1');
        assert_eq!(screen.iter().collect::<String>(), "Tối");
    }

    #[test]
    fn history_test_5_clear_all_invalidates_history() {
        let mut engine = VniEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();

        for ch in "Na8ng ".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(engine.history_len(), 2);

        engine.clear_all();
        assert_eq!(engine.history_len(), 0);

        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        // '6' must pass through verbatim (None) without modifying "Năng"
        assert_eq!(sim_char(&mut engine, &mut screen, '6'), None);
        assert_eq!(screen.iter().collect::<String>(), "Năng6");
    }

    #[test]
    fn history_ring_buffer_respects_max_capacity() {
        let mut engine = VniEngine::new(ToneStyle::New);
        for i in 0..20 {
            let word = if i % 2 == 0 { "an" } else { "em" };
            for ch in word.chars() {
                engine.process_char(ch);
            }
            engine.process_separator(' ');
        }
        assert_eq!(engine.history_len(), MAX_HISTORY_ENTRIES);
    }

    #[test]
    fn vni_clusters_ua_qua_oa_uo_and_undo() {
        let cases = [
            ("sua73", "sửa"),
            ("sua37", "sửa"),
            ("su7a3", "sửa"),
            ("su73a", "sửa"),
            ("cua73", "cửa"),
            ("lua73", "lửa"),
            ("mua7", "mưa"),
            ("chua7", "chưa"),
            ("d9ua7", "đưa"),
            ("ngua75", "ngựa"),
            ("qua8n", "quăn"),
            ("qua8ng", "quăng"),
            ("hoa8c5", "hoặc"),
            ("xoa8n", "xoăn"),
            ("quo73", "quở"),
            ("thuo73", "thuở"),
            ("thuo7ng", "thương"),
            ("nuo7c1", "nước"),
            ("nuoc71", "nước"),
            ("tuo7i", "tươi"),
            ("tuoi7", "tươi"),
            ("d9uo7c5", "được"),
            ("d9uoc75", "được"),
            ("sua77", "sua7"),
        ];

        for (raw, expected) in cases {
            let mut engine = VniEngine::new(ToneStyle::New);
            let mut screen: Vec<char> = Vec::new();
            for ch in raw.chars() {
                sim_char(&mut engine, &mut screen, ch);
            }
            let got: String = screen.into_iter().collect();
            assert_eq!(got, expected, "VNI input '{}' failed", raw);
        }
    }
}
