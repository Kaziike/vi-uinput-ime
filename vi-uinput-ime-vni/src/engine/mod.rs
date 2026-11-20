pub mod vni;

use crate::input::emitter::OutputItem;
use crate::xkb_gen::lookup_vn_char;

/// Trait implemented by Vietnamese input method engines.
pub trait InputEngine: Send {
    fn process_char(&mut self, c: char) -> Option<Vec<OutputItem>>;
    fn process_backspace(&mut self);
    fn reset(&mut self);
    #[allow(dead_code)]
    fn current_word(&self) -> &str;
}

/// Compute the minimal sequence of `OutputItem`s required to transform `old_word` into `new_word`
/// using Longest Common Prefix (LCP).
pub fn diff_words_to_output(old_word: &str, new_word: &str) -> Vec<OutputItem> {
    let old_chars: Vec<char> = old_word.chars().collect();
    let new_chars: Vec<char> = new_word.chars().collect();

    let lcp_len = old_chars
        .iter()
        .zip(new_chars.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let backspaces = old_chars.len() - lcp_len;
    let tail = &new_chars[lcp_len..];

    let mut batch = Vec::with_capacity(backspaces + tail.len());
    for _ in 0..backspaces {
        batch.push(OutputItem::Backspace);
    }
    for &ch in tail {
        if lookup_vn_char(ch).is_some() {
            batch.push(OutputItem::VietnameseChar(ch));
        } else {
            batch.push(OutputItem::AsciiChar(ch));
        }
    }
    batch
}
