pub mod phonology;
pub mod vni;

use crate::input::emitter::OutputItem;
use crate::xkb_gen::lookup_vn_char;

/// Trait implemented by Vietnamese input method engines with Word History Restoration.
pub trait InputEngine: Send {
    fn process_char(&mut self, c: char) -> Option<Vec<OutputItem>>;
    /// Commits the active word and separator into history.
    /// Returns `Some(batch)` when end-of-word phonology validation auto-restores a non-Vietnamese word
    /// (the caller MUST enqueue this batch BEFORE the separator key itself on the same FIFO queue).
    fn process_separator(&mut self, sep: char) -> Option<Vec<OutputItem>>;
    /// Returns `None` when raw physical Backspace can pass through directly via FIFO,
    /// or `Some(batch)` when deleting a character causes a tone mark to shift position.
    fn process_backspace(&mut self) -> Option<Vec<OutputItem>>;
    /// Strict context invalidation: clears BOTH active word and the entire history stack.
    fn clear_all(&mut self);
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
