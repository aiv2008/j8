use alloc::vec::Vec;

pub struct Lines<'s> {
    text: &'s str,
    starts: Vec<u32>,
}

impl<'s> From<&'s str> for Lines<'s> {
    fn from(text: &'s str) -> Self {
        let mut starts = alloc::vec![0];
        let mut chars = text.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            let after = match c {
                '\r' if chars.peek().is_some_and(|&(_, n)| n == '\n') => continue,
                '\n' | '\r' | '\u{2028}' | '\u{2029}' => i + c.len_utf8(),
                _ => continue,
            };
            starts.push(u32::try_from(after).unwrap_or(u32::MAX));
        }
        Self { text, starts }
    }
}

impl Lines<'_> {
    pub fn locate(&self, pos: u32) -> (u32, u32) {
        let line = self.starts.partition_point(|&s| s <= pos);
        let start = self.starts[line.saturating_sub(1)] as usize;
        let prefix = self.text.get(start..pos as usize).unwrap_or_default();
        let column = prefix.encode_utf16().count() + 1;
        (u32::try_from(line).unwrap_or(u32::MAX), u32::try_from(column).unwrap_or(u32::MAX))
    }
}
