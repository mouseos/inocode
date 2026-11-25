//! SentencePiece unigram tokenisation as the Firefox translation models
//! were trained with: the model file's normalisation rules, Viterbi
//! segmentation and byte fallback, following google/sentencepiece
//! (normalizer.cc, unigram_model.cc, sentencepiece_processor.cc).

use std::collections::HashMap;

const SPACE: &str = "\u{2581}";
/// What sentencepiece takes off the lowest piece score for an unknown
/// character.
const UNKNOWN_PENALTY: f32 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Normal,
    Unknown,
    Control,
    UserDefined,
    Unused,
    Byte,
}

#[derive(Debug)]
pub(crate) struct Vocabulary {
    pieces: Vec<(String, f32, Kind)>,
    ids: HashMap<String, u32>,
    /// The ids of `<0x00>`…`<0xFF>`, when the model falls back to bytes.
    bytes: Option<Vec<u32>>,
    longest_piece: usize,
    lowest_score: f32,
    unknown: u32,
    end: u32,
    unknown_surface: String,
    user_defined: Vec<String>,
    normalizer: Normalizer,
}

#[derive(Debug, Default)]
struct Normalizer {
    /// The double-array trie of the precompiled rules, as darts-clone
    /// stores it; empty when the text is kept as it is.
    trie: Vec<u32>,
    replacements: Vec<u8>,
    add_dummy_prefix: bool,
    remove_extra_whitespaces: bool,
    escape_whitespaces: bool,
}

impl Vocabulary {
    /// Reads a sentencepiece model file (the `ModelProto` message).
    pub(crate) fn parse(data: &[u8]) -> Result<Self, String> {
        let mut pieces = Vec::new();
        let mut model_type = 1;
        let mut byte_fallback = false;
        let mut unknown = 0u32;
        let mut end = 2u32;
        let mut unknown_surface = " \u{2047} ".to_string();
        let mut normalizer = Normalizer {
            add_dummy_prefix: true,
            remove_extra_whitespaces: true,
            escape_whitespaces: true,
            ..Normalizer::default()
        };
        for field in Fields::new(data) {
            let (number, value) = field?;
            match (number, value) {
                (1, Value::Bytes(piece)) => {
                    let (mut text, mut score, mut kind) = (String::new(), 0.0, 1);
                    for field in Fields::new(piece) {
                        match field? {
                            (1, Value::Bytes(bytes)) => text = utf8(bytes)?,
                            (2, Value::Fixed32(bits)) => score = f32::from_bits(bits),
                            (3, Value::Varint(value)) => kind = value,
                            _ => {}
                        }
                    }
                    let kind = match kind {
                        1 => Kind::Normal,
                        2 => Kind::Unknown,
                        3 => Kind::Control,
                        4 => Kind::UserDefined,
                        5 => Kind::Unused,
                        6 => Kind::Byte,
                        other => return Err(format!("unknown piece type {other}")),
                    };
                    if text.is_empty() {
                        return Err("a piece is empty".into());
                    }
                    pieces.push((text, score, kind));
                }
                (2, Value::Bytes(trainer)) => {
                    for field in Fields::new(trainer) {
                        match field? {
                            (3, Value::Varint(value)) => model_type = value,
                            (35, Value::Varint(value)) => byte_fallback = value != 0,
                            (40, Value::Varint(value)) => unknown = value as u32,
                            (42, Value::Varint(value)) => end = value as u32,
                            (44, Value::Bytes(bytes)) => unknown_surface = utf8(bytes)?,
                            _ => {}
                        }
                    }
                }
                (3, Value::Bytes(spec)) => {
                    for field in Fields::new(spec) {
                        match field? {
                            (2, Value::Bytes(map)) => {
                                (normalizer.trie, normalizer.replacements) = charsmap(map)?
                            }
                            (3, Value::Varint(value)) => normalizer.add_dummy_prefix = value != 0,
                            (4, Value::Varint(value)) => {
                                normalizer.remove_extra_whitespaces = value != 0
                            }
                            (5, Value::Varint(value)) => normalizer.escape_whitespaces = value != 0,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if model_type != 1 {
            return Err(format!(
                "sentencepiece model type {model_type} is not unigram"
            ));
        }
        if pieces.is_empty() {
            return Err("the vocabulary has no pieces".into());
        }
        let id = |name: &str, id: u32| -> Result<u32, String> {
            if (id as usize) < pieces.len() {
                Ok(id)
            } else {
                Err(format!("the {name} id {id} is not in the vocabulary"))
            }
        };
        let unknown = id("unknown", unknown)?;
        let end = id("end", end)?;
        let ids: HashMap<String, u32> = pieces
            .iter()
            .enumerate()
            .map(|(index, (text, _, _))| (text.clone(), index as u32))
            .collect();
        let bytes = if byte_fallback {
            Some(
                (0..=255u8)
                    .map(|byte| {
                        ids.get(&format!("<0x{byte:02X}>"))
                            .copied()
                            .ok_or_else(|| format!("no piece for byte {byte:#04x}"))
                    })
                    .collect::<Result<Vec<u32>, String>>()?,
            )
        } else {
            None
        };
        let lowest_score = pieces
            .iter()
            .filter(|(_, _, kind)| *kind == Kind::Normal)
            .map(|(_, score, _)| *score)
            .fold(f32::MAX, f32::min);
        let longest_piece = pieces
            .iter()
            .map(|(text, _, _)| text.len())
            .max()
            .unwrap_or(0);
        let user_defined = pieces
            .iter()
            .filter(|(_, _, kind)| *kind == Kind::UserDefined)
            .map(|(text, _, _)| text.clone())
            .collect();
        Ok(Self {
            pieces,
            ids,
            bytes,
            longest_piece,
            lowest_score,
            unknown,
            end,
            unknown_surface,
            user_defined,
            normalizer,
        })
    }

    pub(crate) fn end(&self) -> u32 {
        self.end
    }

    pub(crate) fn unknown(&self) -> u32 {
        self.unknown
    }

    pub(crate) fn len(&self) -> usize {
        self.pieces.len()
    }

    /// The piece ids of `text`, without the end of sentence.
    pub(crate) fn encode(&self, text: &str) -> Vec<u32> {
        let normalized = self.normalize(text);
        let mut out = Vec::new();
        let mut previous_unknown = false;
        for (piece, id) in self.segment(&normalized) {
            let unknown = id == self.unknown;
            match (&self.bytes, unknown) {
                (Some(bytes), true) => {
                    out.extend(piece.bytes().map(|byte| bytes[byte as usize]));
                }
                // Runs of unknown pieces are one unknown piece.
                (None, true) if previous_unknown => {}
                _ => out.push(id),
            }
            previous_unknown = unknown;
        }
        out
    }

    /// The text of piece `ids`.
    pub(crate) fn decode(&self, ids: &[u32]) -> String {
        let mut text = String::new();
        let mut bytes = Vec::new();
        let mut first = true;
        for &id in ids {
            let Some((piece, _, kind)) = self.pieces.get(id as usize) else {
                continue;
            };
            if *kind == Kind::Byte {
                if let Some(byte) = piece
                    .strip_prefix("<0x")
                    .and_then(|rest| rest.strip_suffix('>'))
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    bytes.push(byte);
                    continue;
                }
            }
            if !bytes.is_empty() {
                text.push_str(&String::from_utf8_lossy(&bytes));
                bytes.clear();
                first = false;
            }
            match kind {
                Kind::Control => {}
                Kind::Unknown => {
                    text.push_str(&self.unknown_surface);
                    first = false;
                }
                _ => {
                    let piece = if first {
                        piece.strip_prefix(SPACE).unwrap_or(piece)
                    } else {
                        piece
                    };
                    text.push_str(&piece.replace(SPACE, " "));
                    first = false;
                }
            }
        }
        text.push_str(&String::from_utf8_lossy(&bytes));
        text
    }

    /// `text` with the model's normalisation rules applied and spaces
    /// written as `▁` (normalizer.cc, `Normalizer::Normalize`).
    fn normalize(&self, text: &str) -> String {
        let rules = &self.normalizer;
        let space = if rules.escape_whitespaces { SPACE } else { " " };
        let mut input = text.as_bytes();
        if rules.remove_extra_whitespaces {
            while !input.is_empty() {
                let (piece, used) = self.normalize_prefix(input);
                if piece != b" " {
                    break;
                }
                input = &input[used..];
            }
        }
        let mut out = String::new();
        if input.is_empty() {
            return out;
        }
        if rules.add_dummy_prefix {
            out.push_str(space);
        }
        let mut previous_space = rules.remove_extra_whitespaces;
        let mut bytes = Vec::new();
        while !input.is_empty() {
            let (mut piece, used) = self.normalize_prefix(input);
            while previous_space && piece.first() == Some(&b' ') {
                piece = &piece[1..];
            }
            if !piece.is_empty() {
                for &byte in piece {
                    if byte == b' ' {
                        bytes.extend_from_slice(space.as_bytes());
                    } else {
                        bytes.push(byte);
                    }
                }
                previous_space = piece.last() == Some(&b' ');
            }
            input = &input[used..];
            if !rules.remove_extra_whitespaces {
                previous_space = false;
            }
        }
        out.push_str(&String::from_utf8_lossy(&bytes));
        if rules.remove_extra_whitespaces {
            while out.ends_with(space) {
                out.truncate(out.len() - space.len());
            }
        }
        out
    }

    /// What the start of `input` becomes, and how many bytes of it that
    /// used.
    fn normalize_prefix<'a>(&'a self, input: &'a [u8]) -> (&'a [u8], usize) {
        if let Some(symbol) = self
            .user_defined
            .iter()
            .filter(|symbol| input.starts_with(symbol.as_bytes()))
            .max_by_key(|symbol| symbol.len())
        {
            return (&input[..symbol.len()], symbol.len());
        }
        let rules = &self.normalizer;
        if let Some((length, at)) = longest_prefix(&rules.trie, input) {
            let at = at as usize;
            if at < rules.replacements.len() {
                let rest = &rules.replacements[at..];
                let end = rest
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(rest.len());
                return (&rest[..end], length);
            }
        }
        let length = char_length(input);
        match std::str::from_utf8(&input[..length]) {
            Ok(_) => (&input[..length], length),
            Err(_) => ("\u{FFFD}".as_bytes(), 1),
        }
    }

    /// The best-scoring cut of `normalized` into pieces (unigram_model.cc,
    /// `Model::EncodeOptimized`); a character no piece starts with is an
    /// unknown piece.
    fn segment<'a>(&self, normalized: &'a str) -> Vec<(&'a str, u32)> {
        #[derive(Clone, Copy)]
        struct Best {
            id: u32,
            score: f32,
            start: Option<usize>,
        }
        let size = normalized.len();
        let unknown_score = self.lowest_score - UNKNOWN_PENALTY;
        let mut best = vec![
            Best {
                id: 0,
                score: 0.0,
                start: None,
            };
            size + 1
        ];
        let mut start = 0;
        while start < size {
            let so_far = best[start].score;
            let char_end = start + normalized[start..].chars().next().map_or(1, char::len_utf8);
            let mut has_single = false;
            let longest = (start + self.longest_piece).min(size);
            for end in (start + 1..=longest).filter(|&end| normalized.is_char_boundary(end)) {
                let Some(&id) = self.ids.get(&normalized[start..end]) else {
                    continue;
                };
                let (_, score, kind) = &self.pieces[id as usize];
                let score = match kind {
                    Kind::Normal => *score,
                    Kind::UserDefined => 0.1 * (end - start - 1) as f32,
                    _ => continue,
                };
                let candidate = so_far + score;
                let target = &mut best[end];
                if target.start.is_none() || candidate > target.score {
                    *target = Best {
                        id,
                        score: candidate,
                        start: Some(start),
                    };
                }
                if end == char_end {
                    has_single = true;
                }
            }
            if !has_single {
                let candidate = so_far + unknown_score;
                let target = &mut best[char_end];
                if target.start.is_none() || candidate > target.score {
                    *target = Best {
                        id: self.unknown,
                        score: candidate,
                        start: Some(start),
                    };
                }
            }
            start = char_end;
        }
        let mut out = Vec::new();
        let mut end = size;
        while end > 0 {
            let node = best[end];
            let start = node.start.unwrap_or(0);
            out.push((&normalized[start..end], node.id));
            end = start;
        }
        out.reverse();
        out
    }
}

/// The byte length of the UTF-8 character `input` starts with, 1 when it
/// is not one.
fn char_length(input: &[u8]) -> usize {
    let length = match input[0] {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    };
    if length <= input.len() && std::str::from_utf8(&input[..length]).is_ok() {
        length
    } else {
        1
    }
}

/// The trie and replacement strings of a precompiled charsmap:
/// `<trie size: u32><trie><NUL-separated replacements>`.
fn charsmap(blob: &[u8]) -> Result<(Vec<u32>, Vec<u8>), String> {
    let size = blob
        .get(..4)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
        .ok_or("the normalization rules are cut short")?;
    if size >= blob.len() - 4 || size % 4 != 0 {
        return Err("the normalization rules are broken".into());
    }
    let trie = blob[4..4 + size]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|unit| u32::from_le_bytes(*unit))
        .collect();
    Ok((trie, blob[4 + size..].to_vec()))
}

/// The longest key of the darts-clone double array `units` that `key`
/// starts with: its length and value.
fn longest_prefix(units: &[u32], key: &[u8]) -> Option<(usize, u32)> {
    let offset = |unit: u32| ((unit >> 10) << ((unit & (1 << 9)) >> 6)) as usize;
    let label = |unit: u32| unit & ((1 << 31) | 0xFF);
    let has_leaf = |unit: u32| (unit >> 8) & 1 == 1;
    let value = |unit: u32| unit & ((1 << 31) - 1);
    let mut node = offset(*units.first()?);
    let mut found = None;
    for (index, &byte) in key.iter().enumerate() {
        node ^= byte as usize;
        let unit = *units.get(node)?;
        if label(unit) != byte as u32 {
            break;
        }
        node ^= offset(unit);
        if has_leaf(unit) {
            found = Some((index + 1, value(*units.get(node)?)));
        }
    }
    found
}

fn utf8(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|error| error.to_string())
}

enum Value<'a> {
    Varint(u64),
    Fixed32(u32),
    Fixed64,
    Bytes(&'a [u8]),
}

/// The fields of a protobuf message, in order.
struct Fields<'a> {
    data: &'a [u8],
}

impl<'a> Fields<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.data.split_first().ok_or("a number is cut short")?;
            self.data = rest;
            value |= u64::from(byte & 0x7F) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
        }
        Err("a number is too long".into())
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        if length > self.data.len() {
            return Err("a field is cut short".into());
        }
        let (taken, rest) = self.data.split_at(length);
        self.data = rest;
        Ok(taken)
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u64, Value<'a>), String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.data.is_empty() {
            return None;
        }
        let field = (|| {
            let key = self.varint()?;
            let value = match key & 7 {
                0 => Value::Varint(self.varint()?),
                1 => {
                    self.take(8)?;
                    Value::Fixed64
                }
                2 => {
                    let length = self.varint()? as usize;
                    Value::Bytes(self.take(length)?)
                }
                5 => Value::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().unwrap())),
                other => return Err(format!("unsupported protobuf wire type {other}")),
            };
            Ok((key >> 3, value))
        })();
        if field.is_err() {
            self.data = &[];
        }
        Some(field)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn bytes_field(number: u64, bytes: &[u8], out: &mut Vec<u8>) {
        varint(number << 3 | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }

    fn varint_field(number: u64, value: u64, out: &mut Vec<u8>) {
        varint(number << 3, out);
        varint(value, out);
    }

    /// A sentencepiece model file with `pieces` (text, score, type), the
    /// end at 0 and the unknown piece at 1, as the Firefox vocabularies.
    pub(crate) fn model(pieces: &[(&str, f32, u64)], byte_fallback: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for (text, score, kind) in pieces {
            let mut piece = Vec::new();
            bytes_field(1, text.as_bytes(), &mut piece);
            varint(2 << 3 | 5, &mut piece);
            piece.extend_from_slice(&score.to_le_bytes());
            varint_field(3, *kind, &mut piece);
            bytes_field(1, &piece, &mut out);
        }
        let mut trainer = Vec::new();
        varint_field(3, 1, &mut trainer);
        varint_field(35, byte_fallback as u64, &mut trainer);
        varint_field(40, 1, &mut trainer);
        varint_field(42, 0, &mut trainer);
        bytes_field(2, &trainer, &mut out);
        out
    }

    fn vocabulary(byte_fallback: bool) -> Vocabulary {
        let mut pieces = vec![
            ("</s>", 0.0, 3),
            ("<unk>", 0.0, 2),
            ("__source__", 0.0, 4),
            ("▁", -2.0, 1),
            ("▁he", -3.0, 1),
            ("llo", -3.0, 1),
            ("▁hello", -4.0, 1),
            ("h", -5.0, 1),
            ("e", -5.0, 1),
            ("l", -5.0, 1),
            ("o", -5.0, 1),
            ("▁w", -4.0, 1),
            ("orld", -4.0, 1),
        ];
        let names: Vec<String> = (0..=255).map(|byte| format!("<0x{byte:02X}>")).collect();
        if byte_fallback {
            pieces.extend(names.iter().map(|name| (name.as_str(), 0.0, 6)));
        }
        Vocabulary::parse(&model(&pieces, byte_fallback)).unwrap()
    }

    #[test]
    fn text_is_cut_into_the_best_pieces_and_put_back() {
        let vocabulary = vocabulary(false);
        assert_eq!(vocabulary.end(), 0);
        assert_eq!(vocabulary.normalize("  hello   world "), "▁hello▁world");
        let ids = vocabulary.encode("hello world");
        // "▁hello" (-4) beats "▁he" + "llo" (-6).
        assert_eq!(ids, [6, 11, 12]);
        assert_eq!(vocabulary.decode(&ids), "hello world");
        assert_eq!(vocabulary.encode("__source__ hello"), [3, 2, 6]);
        // Unknown characters in a row are one unknown piece.
        assert_eq!(vocabulary.encode("hé£"), [3, 7, 1]);
        assert_eq!(vocabulary.decode(&[6, 1, 0]), "hello ⁇ ");
    }

    #[test]
    fn unknown_characters_fall_back_to_bytes() {
        let vocabulary = vocabulary(true);
        let ids = vocabulary.encode("hé");
        let byte = |value: u8| 13 + value as u32;
        assert_eq!(ids, [3, 7, byte(0xC3), byte(0xA9)]);
        assert_eq!(vocabulary.decode(&ids), "hé");
    }

    #[test]
    fn a_precompiled_rule_replaces_the_longest_match() {
        // A darts-clone array for the keys "A" -> "a" and "AB" -> "x":
        // root offset 1; "A" (0x41) at 1^0x41 = 0x40, leaf of "A" at
        // 0x40^offset; "B" from there.
        let mut units = vec![0u32; 512];
        let unit =
            |label: u32, offset: u32, leaf: bool| label | (offset << 10) | ((leaf as u32) << 8);
        units[0] = unit(0, 1, false);
        // node "A": label 0x41, offset 0x100 -> children at 0x40^0x100 = 0x140.
        units[0x40] = unit(0x41, 0x100, true);
        // leaf value of "A" at 0x140 (label 0 child): value 0.
        units[0x140] = 1 << 31;
        // "B" child of "A" at 0x140^0x42 = 0x102, offset 0x3 -> 0x101 leaf.
        units[0x102] = unit(0x42, 0x3, true);
        units[0x101] = (1 << 31) | 2;
        assert_eq!(longest_prefix(&units, b"AC"), Some((1, 0)));
        assert_eq!(longest_prefix(&units, b"ABC"), Some((2, 2)));
        assert_eq!(longest_prefix(&units, b"C"), None);
    }
}
