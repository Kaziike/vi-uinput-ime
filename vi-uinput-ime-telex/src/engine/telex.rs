use crate::config::ToneStyle;
use crate::engine::phonology::{
    validate_final, validate_partial, Letter, Tone, VowelShape,
};
use crate::engine::{diff_words_to_output, InputEngine};
use crate::input::emitter::OutputItem;
use std::collections::VecDeque;

/// Maximum number of word/separator entries retained in the history ring buffer.
pub const MAX_HISTORY_ENTRIES: usize = 15;

/// Complete internal state of a single Vietnamese syllable/word (case, vowel shapes, tones, rendered text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyllableState {
    letters: Vec<Letter>,
    /// Exact raw ASCII keystrokes typed by the user (preserving original capitalization)
    /// so non-Vietnamese words can be restored cleanly without losing characters or case.
    raw_keys: Vec<char>,
    rendered: String,
    disabled_for_word: bool,
    /// True immediately after a Telex tone double-tap undo (`ff`/`rr`/`ss`/`xx`/`jj`),
    /// so continuing to type (e.g. `buffer`, `error`, `array`, `coffee`, `password`)
    /// restores both doubled consonants cleanly without breaking standalone undo (`buf `, `suaww`).
    tone_undone: bool,
}

impl SyllableState {
    fn new() -> Self {
        Self {
            letters: Vec::with_capacity(16),
            raw_keys: Vec::with_capacity(24),
            rendered: String::with_capacity(32),
            disabled_for_word: false,
            tone_undone: false,
        }
    }

    fn render_buffer(&self) -> String {
        self.letters.iter().map(Letter::render).collect()
    }

    fn raw_string(&self) -> String {
        self.raw_keys.iter().collect()
    }

    fn has_any_diacritic(&self) -> bool {
        self.letters
            .iter()
            .any(|l| l.shape != VowelShape::Plain || l.tone != Tone::None || l.d_bar)
    }

    /// Restores the syllable to its exact raw ASCII keystrokes and disables further
    /// Vietnamese transforms for this word (including across Space + Backspace history restore).
    ///
    /// Returns `None` if `self.rendered` already equals `raw_keys` (zero redraw / zero flicker).
    pub fn restore_to_raw(&mut self) -> Option<Vec<OutputItem>> {
        let raw_str = self.raw_string();
        self.disabled_for_word = true;
        self.tone_undone = false;
        if self.rendered == raw_str {
            return None;
        }
        let old_rendered = std::mem::replace(&mut self.rendered, raw_str.clone());
        self.letters = self.raw_keys.iter().copied().map(Letter::from_ascii).collect();
        Some(diff_words_to_output(&old_rendered, &raw_str))
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

    fn apply_tone(&mut self, tone: Tone, tone_style: ToneStyle) -> bool {
        let Some(target) = self.pick_tone_target(tone_style) else {
            return false;
        };
        let existing = self.current_tone();
        if existing == tone {
            self.clear_all_tones();
            self.disabled_for_word = true;
            self.tone_undone = true;
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

    /// Applies circumflex (`â`, `ê`, `ô`), allowing overwrite from `ă`/`ơ` (e.g. `Năng`+`a` -> `Nâng`).
    fn apply_circumflex(&mut self, target_base: char, tone_style: ToneStyle) -> bool {
        for i in (0..self.letters.len()).rev() {
            if self.letters[i].base == target_base {
                if self.letters[i].shape == VowelShape::Circumflex {
                    self.letters[i].shape = VowelShape::Plain;
                    self.reposition_existing_tone(tone_style);
                    self.disabled_for_word = true;
                    return false;
                }
                // Converting `ươ` -> `uô` (e.g. `bươn` + `o` -> `buôn`) clears horn on preceding `u`.
                if target_base == 'o'
                    && i > 0
                    && self.letters[i - 1].base == 'u'
                    && self.letters[i - 1].shape == VowelShape::Horn
                {
                    self.letters[i - 1].shape = VowelShape::Plain;
                }
                self.letters[i].shape = VowelShape::Circumflex;
                self.reposition_existing_tone(tone_style);
                return true;
            }
        }
        false
    }

    /// Applies horn (`ơ`, `ư`, `ươ`) or breve (`ă`), allowing overwrite from `â`/`ô` (e.g. `Nâng`+`w` -> `Năng`).
    /// Does NOT turn standalone `w` into `ư`.
    /// Priority order:
    ///   1. `uo` cluster: `qu+o` -> `quơ`; `uo` with trailing coda/vowel -> `ươ`; open `uo` -> `uơ` (auto-expands on trailing char).
    ///   2. `ua` cluster: non-`qu` `ua` -> `ưa` (`suawr` -> `sửa`); `qu+a` -> `quă` (`quawn` -> `quăn`).
    ///   3. `oa` cluster: `o` stays plain, `a` -> `ă` (`hoawcj` -> `hoặc`, `xoawn` -> `xoăn`).
    ///   4. Fallback reverse scan on single vowels `a`, `o`, `u`.
    fn apply_horn_or_breve(&mut self, tone_style: ToneStyle) -> bool {
        let len = self.letters.len();

        // 1. `uo` cluster
        if len >= 2 {
            for i in (0..len - 1).rev() {
                if self.letters[i].base == 'u' && self.letters[i + 1].base == 'o' {
                    let is_qu = i > 0 && self.letters[i - 1].base == 'q';
                    if is_qu {
                        if self.letters[i + 1].shape == VowelShape::Horn {
                            self.letters[i + 1].shape = VowelShape::Plain;
                            self.reposition_existing_tone(tone_style);
                            self.disabled_for_word = true;
                            return false;
                        }
                        self.letters[i + 1].shape = VowelShape::Horn;
                        self.reposition_existing_tone(tone_style);
                        return true;
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
                            return false;
                        }
                        self.letters[i].shape = VowelShape::Horn;
                        self.letters[i + 1].shape = VowelShape::Horn;
                        self.reposition_existing_tone(tone_style);
                        return true;
                    } else {
                        // Open `uo` (e.g. `thuowr` -> `thuở`): horn `o` only; if a trailing
                        // coda/vowel is typed later (e.g. `thuowng`), `process_char` auto-horns `u`.
                        if self.letters[i + 1].shape == VowelShape::Horn {
                            self.letters[i].shape = VowelShape::Plain;
                            self.letters[i + 1].shape = VowelShape::Plain;
                            self.reposition_existing_tone(tone_style);
                            self.disabled_for_word = true;
                            return false;
                        }
                        self.letters[i + 1].shape = VowelShape::Horn;
                        self.reposition_existing_tone(tone_style);
                        return true;
                    }
                }
            }
        }

        // 2. `ua` cluster (`suawr` -> `sửa`, `quawn` -> `quăn`)
        if len >= 2 {
            for i in (0..len - 1).rev() {
                if self.letters[i].base == 'u' && self.letters[i + 1].base == 'a' {
                    let is_qu = i > 0 && self.letters[i - 1].base == 'q';
                    if is_qu {
                        if self.letters[i + 1].shape == VowelShape::Breve {
                            self.letters[i + 1].shape = VowelShape::Plain;
                            self.reposition_existing_tone(tone_style);
                            self.disabled_for_word = true;
                            return false;
                        }
                        self.letters[i + 1].shape = VowelShape::Breve;
                        self.reposition_existing_tone(tone_style);
                        return true;
                    } else {
                        if self.letters[i].shape == VowelShape::Horn {
                            self.letters[i].shape = VowelShape::Plain;
                            self.reposition_existing_tone(tone_style);
                            self.disabled_for_word = true;
                            return false;
                        }
                        self.letters[i].shape = VowelShape::Horn;
                        self.letters[i + 1].shape = VowelShape::Plain;
                        self.reposition_existing_tone(tone_style);
                        return true;
                    }
                }
            }
        }

        // 3. `oa` cluster (`hoawcj` -> `hoặc`, `xoawn` -> `xoăn`)
        if len >= 2 {
            for i in (0..len - 1).rev() {
                if self.letters[i].base == 'o' && self.letters[i + 1].base == 'a' {
                    if self.letters[i + 1].shape == VowelShape::Breve {
                        self.letters[i + 1].shape = VowelShape::Plain;
                        self.reposition_existing_tone(tone_style);
                        self.disabled_for_word = true;
                        return false;
                    }
                    self.letters[i].shape = VowelShape::Plain;
                    self.letters[i + 1].shape = VowelShape::Breve;
                    self.reposition_existing_tone(tone_style);
                    return true;
                }
            }
        }

        // 4. Single-vowel fallback
        for i in (0..len).rev() {
            match self.letters[i].base {
                'a' => {
                    if self.letters[i].shape == VowelShape::Breve {
                        self.letters[i].shape = VowelShape::Plain;
                        self.reposition_existing_tone(tone_style);
                        self.disabled_for_word = true;
                        return false;
                    }
                    self.letters[i].shape = VowelShape::Breve;
                    self.reposition_existing_tone(tone_style);
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
                        self.reposition_existing_tone(tone_style);
                        self.disabled_for_word = true;
                        return false;
                    }
                    self.letters[i].shape = VowelShape::Horn;
                    self.reposition_existing_tone(tone_style);
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    fn try_transform(&mut self, c: char, tone_style: ToneStyle) -> bool {
        if self.disabled_for_word {
            return false;
        }
        match c.to_ascii_lowercase() {
            's' => self.apply_tone(Tone::Acute, tone_style),
            'f' => self.apply_tone(Tone::Grave, tone_style),
            'r' => self.apply_tone(Tone::HookAbove, tone_style),
            'x' => self.apply_tone(Tone::Tilde, tone_style),
            'j' => self.apply_tone(Tone::DotBelow, tone_style),
            'z' => self.remove_tone(),
            'd' => self.apply_d_bar(),
            'a' => self.apply_circumflex('a', tone_style),
            'e' => self.apply_circumflex('e', tone_style),
            'o' => self.apply_circumflex('o', tone_style),
            // No standalone `w -> ư` shorthand: `w` only modifies existing `a`, `o`, `u`, `uo`.
            'w' => self.apply_horn_or_breve(tone_style),
            _ => false,
        }
    }
}

/// Entry stored in the bounded history ring buffer for Word History Restoration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryEntry {
    Word(SyllableState),
    Separator(char),
}

/// Pure Telex Vietnamese input engine with Word History Restoration & vowel diacritic overwrite.
/// Only consumes ASCII letters (`a..z`, `A..Z`); digits (`0..9`) and symbols are never intercepted.
pub struct TelexEngine {
    tone_style: ToneStyle,
    auto_restore_non_vn: bool,
    active_word: Option<SyllableState>,
    history: VecDeque<HistoryEntry>,
}

impl TelexEngine {
    pub fn new(tone_style: ToneStyle) -> Self {
        Self {
            tone_style,
            auto_restore_non_vn: true,
            active_word: None,
            history: VecDeque::with_capacity(MAX_HISTORY_ENTRIES),
        }
    }

    pub fn set_auto_restore_non_vn(&mut self, enabled: bool) {
        self.auto_restore_non_vn = enabled;
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

impl InputEngine for TelexEngine {
    fn process_char(&mut self, c: char) -> Option<Vec<OutputItem>> {
        if c == ' ' {
            if let Some(mut batch) = self.process_separator(' ') {
                batch.push(OutputItem::AsciiChar(' '));
                return Some(batch);
            }
            return None;
        }
        if !c.is_ascii_alphabetic() {
            self.clear_all();
            return None;
        }

        let tone_style = self.tone_style;
        let auto_restore = self.auto_restore_non_vn;
        let word = self.active_word.get_or_insert_with(SyllableState::new);
        let old_rendered = word.rendered.clone();

        if word.disabled_for_word {
            word.raw_keys.push(c);
            if auto_restore && word.tone_undone {
                // Continuing to type after a tone double-tap (`b-u-f-f` + `e` -> `buffer`,
                // `e-r-r` + `o` -> `error`, `p-a-s-s` + `w` -> `password`): restore the doubled letter.
                let batch = word
                    .restore_to_raw()
                    .unwrap_or_else(|| diff_words_to_output(&old_rendered, &word.rendered));
                return Some(batch);
            }
            word.letters.push(Letter::from_ascii(c));
            let new_rendered = word.render_buffer();
            word.rendered = new_rendered.clone();
            return Some(diff_words_to_output(&old_rendered, &new_rendered));
        }

        word.raw_keys.push(c);
        let transformed = word.try_transform(c, tone_style);

        if !transformed {
            word.letters.push(Letter::from_ascii(c));
            if word.disabled_for_word {
                // For non-tone double-tap undo (e.g. `suaww` -> `suaw`, `buowwno` -> `buowno`),
                // sync `raw_keys` 1-to-1 with `letters`. For tone double-tap undo (`tone_undone`),
                // retain full `raw_keys` so continuing to type (`buffer`, `error`) restores both letters.
                if !word.tone_undone {
                    word.raw_keys = word.letters.iter().map(Letter::render).collect();
                }
            } else {
                let len = word.letters.len();
                if len >= 3 {
                    // Auto-expand `ưo + trailing` -> `ươ + trailing` (e.g. `dduwocj` -> `được`)
                    if word.letters[len - 3].base == 'u'
                        && word.letters[len - 3].shape == VowelShape::Horn
                        && word.letters[len - 2].base == 'o'
                        && word.letters[len - 2].shape == VowelShape::Plain
                    {
                        word.letters[len - 2].shape = VowelShape::Horn;
                    }
                    // Auto-expand non-`qu` `uơ + trailing` -> `ươ + trailing` (e.g. `thuowng` -> `thương`, `nuowcs` -> `nước`, `tuowi` -> `tươi`)
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

                // Immediate phonology check: only runs when the typed key appended a plain letter.
                if auto_restore && !validate_partial(&word.letters) {
                    let batch = word
                        .restore_to_raw()
                        .unwrap_or_else(|| diff_words_to_output(&old_rendered, &word.rendered));
                    return Some(batch);
                }
            }
        }

        let new_rendered = word.render_buffer();
        word.rendered = new_rendered.clone();

        Some(diff_words_to_output(&old_rendered, &new_rendered))
    }

    fn process_separator(&mut self, sep: char) -> Option<Vec<OutputItem>> {
        let mut restore_batch = None;
        if let Some(mut word) = self.active_word.take() {
            if !word.letters.is_empty() {
                if word.tone_undone {
                    word.tone_undone = false;
                    word.raw_keys = word.letters.iter().map(Letter::render).collect();
                }
                if sep == ' '
                    && self.auto_restore_non_vn
                    && !word.disabled_for_word
                    && !validate_final(&word.letters)
                {
                    restore_batch = word.restore_to_raw();
                }
                self.push_history(HistoryEntry::Word(word));
            }
        }
        self.push_history(HistoryEntry::Separator(sep));
        restore_batch
    }

    fn process_backspace(&mut self) -> Option<Vec<OutputItem>> {
        // Case A: active_word currently has characters.
        if let Some(mut word) = self.active_word.take() {
            let old_rendered = word.rendered.clone();
            let mut expected_if_raw_bs = old_rendered.clone();
            expected_if_raw_bs.pop();

            let popped = word.letters.pop();

            if word.letters.is_empty() {
                // Active word is now empty; leave active_word = None WITHOUT popping history
                // because the preceding separator is still on screen.
                return None;
            }

            if word.tone_undone {
                word.tone_undone = false;
                word.raw_keys = word.letters.iter().map(Letter::render).collect();
            } else if word.disabled_for_word {
                word.raw_keys.pop();
            } else if !word.has_any_diacritic() {
                word.raw_keys = word.letters.iter().map(Letter::render).collect();
            } else if let Some(popped_letter) = popped {
                if let Some(pos) = word
                    .raw_keys
                    .iter()
                    .rposition(|ch| ch.to_ascii_lowercase() == popped_letter.base)
                {
                    word.raw_keys.remove(pos);
                }
            }

            if !word.disabled_for_word {
                word.reposition_existing_tone(self.tone_style);
            }
            let new_rendered = word.render_buffer();
            word.rendered = new_rendered.clone();
            self.active_word = Some(word);

            if new_rendered == expected_if_raw_bs {
                // Smooth raw Backspace passthrough (no redraw, ideal for key repeat).
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

    fn simulate_typing(engine: &mut TelexEngine, input: &str) -> String {
        let mut screen: Vec<char> = Vec::new();
        for ch in input.chars() {
            match engine.process_char(ch) {
                Some(items) => apply_batch(&mut screen, &items),
                None => screen.push(ch),
            }
        }
        screen.into_iter().collect()
    }

    fn sim_backspace(engine: &mut TelexEngine, screen: &mut Vec<char>) -> Option<Vec<OutputItem>> {
        let res = engine.process_backspace();
        match &res {
            Some(items) => apply_batch(screen, items),
            None => {
                screen.pop();
            }
        }
        res
    }

    fn sim_char(engine: &mut TelexEngine, screen: &mut Vec<char>, ch: char) -> Option<Vec<OutputItem>> {
        let res = engine.process_char(ch);
        match &res {
            Some(items) => apply_batch(screen, items),
            None => screen.push(ch),
        }
        res
    }

    #[test]
    fn telex_complex_sentences_numbers_and_no_w_shorthand() {
        let mut engine = TelexEngine::new(ToneStyle::New);
        assert_eq!(simulate_typing(&mut engine, "tieesng vieejt"), "tiếng việt");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "traau"), "trâu");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "Traau"), "Trâu");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "duwowcj"), "dược");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "dduwowcj"), "được");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "duwowcjd"), "được");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "dduowcj"), "được");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "khuya"), "khuya");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "toios "), "tối ");
        engine.clear_all();
        // Standalone `w` shorthand is disabled: `w`, `win`, `web`, `www` stay literal,
        // while `uw` -> `ư`, `ow` -> `ơ`, `aw` -> `ă` still work normally.
        assert_eq!(simulate_typing(&mut engine, "w"), "w");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "W"), "W");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "win"), "win");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "web"), "web");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "www"), "www");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "uw"), "ư");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "ow"), "ơ");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "aw"), "ă");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "hoaf"), "hoà");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "thuys"), "thuý");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "toiosz"), "tôi");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "nawm 2026"), "năm 2026");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "1234567890"), "1234567890");
    }

    #[test]
    fn telex_old_tone_style_and_backspace_tone_reposition() {
        let mut engine = TelexEngine::new(ToneStyle::Old);
        assert_eq!(simulate_typing(&mut engine, "hoaf"), "hòa");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "thuys"), "thúy");
        engine.clear_all();

        // Typing `toans` -> `toán`; deleting `n` in Old style repositions acute to `o` (`tóa`).
        let mut screen: Vec<char> = Vec::new();
        for ch in "toans".chars() {
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

    #[test]
    fn vowel_diacritic_overwrite_both_directions() {
        let mut engine = TelexEngine::new(ToneStyle::New);
        // "Năng" + 'a' -> "Nâng"
        assert_eq!(simulate_typing(&mut engine, "Nawnga"), "Nâng");
        engine.clear_all();
        // "Nâng" + 'w' -> "Năng"
        assert_eq!(simulate_typing(&mut engine, "Naangw"), "Năng");
        engine.clear_all();
        // "bươn" + 'o' -> "buôn" and "buôn" + 'w' -> "bươn"
        assert_eq!(simulate_typing(&mut engine, "buowwno"), "buowno"); // double w undoes
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "buowno"), "buôn");
        engine.clear_all();
        assert_eq!(simulate_typing(&mut engine, "buoonw"), "bươn");
    }

    #[test]
    fn history_test_1_nang_space_bs_a_produces_nang_circumflex_with_minimal_diff() {
        let mut engine = TelexEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();

        for ch in "Nawng".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(screen.iter().collect::<String>(), "Năng");

        // Press Space
        sim_char(&mut engine, &mut screen, ' ');
        assert_eq!(screen.iter().collect::<String>(), "Năng ");
        assert_eq!(engine.current_word(), "");
        assert_eq!(engine.history_len(), 2);

        // Press Backspace -> deletes space & restores "Năng" into active_word
        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        assert_eq!(screen.iter().collect::<String>(), "Năng");
        assert_eq!(engine.current_word(), "Năng");

        // Press 'a' -> overwrites `ă` to `â` with exact minimal diff: 3 BS + "âng"
        let batch = sim_char(&mut engine, &mut screen, 'a');
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
        let mut engine = TelexEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();

        for ch in "Nawng  ".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(screen.iter().collect::<String>(), "Năng  ");

        // Single Backspace only removes the 2nd space; 1st space remains on top -> do NOT restore
        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        assert_eq!(engine.current_word(), "");

        let batch = sim_char(&mut engine, &mut screen, 'a');
        assert_eq!(batch, Some(vec![OutputItem::AsciiChar('a')]));
        assert_eq!(screen.iter().collect::<String>(), "Năng a");
    }

    #[test]
    fn history_test_3_double_space_double_bs_restores_word() {
        let mut engine = TelexEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();

        for ch in "Nawng  ".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        assert_eq!(engine.current_word(), "");
        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        assert_eq!(engine.current_word(), "Năng");

        sim_char(&mut engine, &mut screen, 'a');
        assert_eq!(screen.iter().collect::<String>(), "Nâng");
    }

    #[test]
    fn history_test_4_delete_entire_second_word_and_space_restores_first_word() {
        let mut engine = TelexEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();

        for ch in "Tooi Nawng".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(screen.iter().collect::<String>(), "Tôi Năng");

        // 4 Backspaces delete "Năng" (raw passthrough each time, no premature history pop)
        for _ in 0..4 {
            assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        }
        assert_eq!(screen.iter().collect::<String>(), "Tôi ");
        assert_eq!(engine.current_word(), "");

        // 5th Backspace deletes Space and restores "Tôi"
        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        assert_eq!(screen.iter().collect::<String>(), "Tôi");
        assert_eq!(engine.current_word(), "Tôi");

        // Typing 's' transforms "Tôi" -> "Tối"
        sim_char(&mut engine, &mut screen, 's');
        assert_eq!(screen.iter().collect::<String>(), "Tối");
    }

    #[test]
    fn history_test_5_clear_all_invalidates_history() {
        let mut engine = TelexEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();

        for ch in "Nawng ".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(engine.history_len(), 2);

        // Simulate IPC focus change or mouse click
        engine.clear_all();
        assert_eq!(engine.history_len(), 0);
        assert_eq!(engine.current_word(), "");

        // Backspace + 'a' must NOT modify "Năng"
        assert_eq!(sim_backspace(&mut engine, &mut screen), None);
        let batch = sim_char(&mut engine, &mut screen, 'a');
        assert_eq!(batch, Some(vec![OutputItem::AsciiChar('a')]));
        assert_eq!(screen.iter().collect::<String>(), "Nănga");
    }

    #[test]
    fn history_ring_buffer_respects_max_capacity() {
        let mut engine = TelexEngine::new(ToneStyle::New);
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
    fn telex_w_clusters_ua_qua_oa_uo_and_undo() {
        let cases = [
            // `ua` + `w` in every tone order -> `ưa`
            ("suawr", "sửa"),
            ("suarw", "sửa"),
            ("suwar", "sửa"),
            ("suwra", "sửa"),
            ("cuawr", "cửa"),
            ("luawr", "lửa"),
            ("muaw", "mưa"),
            ("nuawr", "nửa"),
            ("chuaw", "chưa"),
            ("dduaw", "đưa"),
            ("nguawj", "ngựa"),
            ("ruawr", "rửa"),
            ("thuaw", "thưa"),
            ("truaw", "trưa"),
            ("vuawf", "vừa"),
            ("xuaw", "xưa"),
            // Double-tap `w` on `ưa` undoes horn and appends literal 'w'
            ("suaww", "suaw"),
            // `qu` + `a` + `w` -> `quă`
            ("quawn", "quăn"),
            ("quawng", "quăng"),
            ("quawtj", "quặt"),
            // `oa` + `w` -> `oă`
            ("hoawcj", "hoặc"),
            ("xoawn", "xoăn"),
            // `uo` + `w`: quow, open uo, closed/triphthong uo
            ("quowr", "quở"),
            ("thuowr", "thuở"),
            ("thuowng", "thương"),
            ("nuowcs", "nước"),
            ("nuocws", "nước"),
            ("tuowi", "tươi"),
            ("tuoiw", "tươi"),
            ("dduowcj", "được"),
            ("dduocwj", "được"),
        ];

        for (raw, expected) in cases {
            let mut engine = TelexEngine::new(ToneStyle::New);
            let mut screen: Vec<char> = Vec::new();
            for ch in raw.chars() {
                sim_char(&mut engine, &mut screen, ch);
            }
            let got: String = screen.into_iter().collect();
            assert_eq!(got, expected, "Telex input '{}' failed", raw);
        }
    }

    #[test]
    fn auto_restore_suite_1_leading_f_j_w_z() {
        let words = [
            "fix", "file", "from", "for", "function", "window", "with", "just", "json", "zip",
            "Fix", "Window", "JSON",
        ];
        for w in words {
            let mut engine = TelexEngine::new(ToneStyle::New);
            assert_eq!(simulate_typing(&mut engine, w), w, "leading f/j/w/z failed for '{}'", w);
        }
    }

    #[test]
    fn auto_restore_suite_2_english_onset_clusters() {
        let words = [
            "struct", "string", "print", "class", "float", "switch", "branch", "clone", "shell",
            "script", "Struct", "Branch", "Script",
        ];
        for w in words {
            let mut engine = TelexEngine::new(ToneStyle::New);
            assert_eq!(simulate_typing(&mut engine, w), w, "onset cluster failed for '{}'", w);
        }
    }

    #[test]
    fn auto_restore_suite_3_immediate_restore_bad_coda_or_vowel_after_coda() {
        let words = [
            "server", "config", "code", "return", "import", "export", "buffer", "local", "linux",
            "github", "kernel", "driver", "wayland", "systemd", "Server", "Config", "Kernel",
            "Buffer", "error", "array", "coffee", "password", "ERROR",
        ];
        for w in words {
            let mut engine = TelexEngine::new(ToneStyle::New);
            assert_eq!(simulate_typing(&mut engine, w), w, "immediate restore failed for '{}'", w);
        }
    }

    #[test]
    fn auto_restore_suite_4_deferred_restore_on_space() {
        let cases = [
            ("port ", "port "),
            ("text ", "text "),
            ("case ", "case "),
            ("base ", "base "),
            ("main ", "main "),
            ("masin ", "masin "),
            ("PORT ", "PORT "),
            ("Text ", "Text "),
            ("Case ", "Case "),
            ("saw ", "saw "),
            ("boot ", "boot "),
            ("root ", "root "),
            ("loop ", "loop "),
        ];
        for (raw, expected) in cases {
            let mut engine = TelexEngine::new(ToneStyle::New);
            assert_eq!(
                simulate_typing(&mut engine, raw),
                expected,
                "deferred space restore failed for '{}'",
                raw
            );
        }

        // Verify `process_separator(' ')` explicitly returns the restore diff batch before Space
        let mut engine = TelexEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();
        for ch in "port".chars() {
            sim_char(&mut engine, &mut screen, ch);
        }
        assert_eq!(screen.iter().collect::<String>(), "pỏt");
        let batch = engine
            .process_separator(' ')
            .expect("port must emit restore batch before Space");
        assert_eq!(
            batch,
            vec![
                OutputItem::Backspace,
                OutputItem::Backspace,
                OutputItem::AsciiChar('o'),
                OutputItem::AsciiChar('r'),
                OutputItem::AsciiChar('t'),
            ]
        );
        apply_batch(&mut screen, &batch);
        screen.push(' ');
        assert_eq!(screen.iter().collect::<String>(), "port ");
    }

    #[test]
    fn auto_restore_suite_5_vietnamese_free_order_and_word_history_non_regression() {
        // Full Vietnamese sentence including `test` -> `tét` and shorthand `ko`
        let mut engine = TelexEngine::new(ToneStyle::New);
        assert_eq!(
            simulate_typing(
                &mut engine,
                "test cofn looix gif khoong neeus cofn thif suawr tieeps suawr"
            ),
            "tét còn lỗi gì không nếu còn thì sửa tiếp sửa"
        );

        let vn_free_order = [
            ("tieens ", "tiến "),
            ("tienes ", "tiến "),
            ("nuowcs ", "nước "),
            ("nuocws ", "nước "),
            ("nuoswc ", "nước "),
            ("dduowcj ", "được "),
            ("duongwf ", "dường "),
            ("suawr ", "sửa "),
            ("suarw ", "sửa "),
            ("quawng ", "quăng "),
            ("giuowngf ", "giường "),
            ("khoong ", "không "),
            ("huyeefn ", "huyền "),
            ("nguowif ", "người "),
            ("thuowr ", "thuở "),
            ("quowr ", "quở "),
            ("quyts ", "quýt "),
            ("quyeefn ", "quyền "),
            ("ko ", "ko "),
            ("k ", "k "),
        ];
        for (raw, expected) in vn_free_order {
            let mut eng = TelexEngine::new(ToneStyle::New);
            assert_eq!(
                simulate_typing(&mut eng, raw),
                expected,
                "Vietnamese free-order regression on '{}'",
                raw
            );
        }

        // `Nawng Space Backspace a` -> `"Nâng"`
        let mut eng = TelexEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();
        for ch in "Nawng ".chars() {
            sim_char(&mut eng, &mut screen, ch);
        }
        sim_backspace(&mut eng, &mut screen);
        sim_char(&mut eng, &mut screen, 'a');
        assert_eq!(screen.iter().collect::<String>(), "Nâng");

        // `config Space Backspace s` -> `"configs"` (disabled_for_word preserved in HistoryEntry::Word)
        let mut eng = TelexEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();
        for ch in "config ".chars() {
            sim_char(&mut eng, &mut screen, ch);
        }
        assert_eq!(screen.iter().collect::<String>(), "config ");
        sim_backspace(&mut eng, &mut screen);
        sim_char(&mut eng, &mut screen, 's');
        assert_eq!(screen.iter().collect::<String>(), "configs");

        // `port Space Backspace s` -> `"ports"` (deferred restore also locks disabled_for_word in history)
        let mut eng = TelexEngine::new(ToneStyle::New);
        let mut screen: Vec<char> = Vec::new();
        for ch in "port ".chars() {
            sim_char(&mut eng, &mut screen, ch);
        }
        assert_eq!(screen.iter().collect::<String>(), "port ");
        sim_backspace(&mut eng, &mut screen);
        sim_char(&mut eng, &mut screen, 's');
        assert_eq!(screen.iter().collect::<String>(), "ports");

        // `auto_restore_non_vn = false` disables phonology auto-restore
        let mut eng = TelexEngine::new(ToneStyle::New);
        eng.set_auto_restore_non_vn(false);
        assert_eq!(simulate_typing(&mut eng, "port "), "pỏt ");
    }
}
