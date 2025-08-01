use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fmt::Display;

use harper_brill::{Chunker, Tagger, brill_tagger, burn_chunker};
use paste::paste;

use crate::expr::{Expr, ExprExt, FirstMatchOf, Repeating, SequenceExpr};
use crate::parsers::{Markdown, MarkdownOptions, Parser, PlainEnglish};
use crate::patterns::WordSet;
use crate::punctuation::Punctuation;
use crate::spell::{Dictionary, FstDictionary};
use crate::vec_ext::VecExt;
use crate::{FatStringToken, FatToken, Lrc, Token, TokenKind, TokenStringExt};
use crate::{OrdinalSuffix, Span};

/// A document containing some amount of lexed and parsed English text.
#[derive(Debug, Clone)]
pub struct Document {
    source: Lrc<Vec<char>>,
    tokens: Vec<Token>,
}

impl Default for Document {
    fn default() -> Self {
        Self::new("", &PlainEnglish, &FstDictionary::curated())
    }
}

impl Document {
    /// Locate all the tokens that intersect a provided span.
    ///
    /// Desperately needs optimization.
    pub fn token_indices_intersecting(&self, span: Span<char>) -> Vec<usize> {
        self.tokens()
            .enumerate()
            .filter_map(|(idx, tok)| tok.span.overlaps_with(span).then_some(idx))
            .collect()
    }

    /// Locate all the tokens that intersect a provided span and convert them to [`FatToken`]s.
    ///
    /// Desperately needs optimization.
    pub fn fat_tokens_intersecting(&self, span: Span<char>) -> Vec<FatToken> {
        let indices = self.token_indices_intersecting(span);

        indices
            .into_iter()
            .map(|i| self.tokens[i].to_fat(&self.source))
            .collect()
    }

    /// Lexes and parses text to produce a document using a provided language
    /// parser and dictionary.
    pub fn new(text: &str, parser: &impl Parser, dictionary: &impl Dictionary) -> Self {
        let source: Vec<_> = text.chars().collect();

        Self::new_from_vec(Lrc::new(source), parser, dictionary)
    }

    /// Lexes and parses text to produce a document using a provided language
    /// parser and the included curated dictionary.
    pub fn new_curated(text: &str, parser: &impl Parser) -> Self {
        let source: Vec<_> = text.chars().collect();

        Self::new_from_vec(Lrc::new(source), parser, &FstDictionary::curated())
    }

    /// Lexes and parses text to produce a document using a provided language
    /// parser and dictionary.
    pub fn new_from_vec(
        source: Lrc<Vec<char>>,
        parser: &impl Parser,
        dictionary: &impl Dictionary,
    ) -> Self {
        let tokens = parser.parse(&source);

        let mut document = Self { source, tokens };
        document.parse(dictionary);

        document
    }

    /// Parse text to produce a document using the built-in [`PlainEnglish`]
    /// parser and curated dictionary.
    pub fn new_plain_english_curated(text: &str) -> Self {
        Self::new(text, &PlainEnglish, &FstDictionary::curated())
    }

    /// Parse text to produce a document using the built-in [`PlainEnglish`]
    /// parser and a provided dictionary.
    pub fn new_plain_english(text: &str, dictionary: &impl Dictionary) -> Self {
        Self::new(text, &PlainEnglish, dictionary)
    }

    /// Parse text to produce a document using the built-in [`Markdown`] parser
    /// and curated dictionary.
    pub fn new_markdown_curated(text: &str, markdown_options: MarkdownOptions) -> Self {
        Self::new(
            text,
            &Markdown::new(markdown_options),
            &FstDictionary::curated(),
        )
    }

    /// Parse text to produce a document using the built-in [`Markdown`] parser
    /// and curated dictionary with the default Markdown configuration.
    pub fn new_markdown_default_curated(text: &str) -> Self {
        Self::new_markdown_curated(text, MarkdownOptions::default())
    }

    /// Parse text to produce a document using the built-in [`PlainEnglish`]
    /// parser and the curated dictionary.
    pub fn new_markdown(
        text: &str,
        markdown_options: MarkdownOptions,
        dictionary: &impl Dictionary,
    ) -> Self {
        Self::new(text, &Markdown::new(markdown_options), dictionary)
    }

    /// Parse text to produce a document using the built-in [`PlainEnglish`]
    /// parser and the curated dictionary with the default Markdown configuration.
    pub fn new_markdown_default(text: &str, dictionary: &impl Dictionary) -> Self {
        Self::new_markdown(text, MarkdownOptions::default(), dictionary)
    }

    /// Re-parse important language constructs.
    ///
    /// Should be run after every change to the underlying [`Self::source`].
    fn parse(&mut self, dictionary: &impl Dictionary) {
        self.condense_spaces();
        self.condense_newlines();
        self.newlines_to_breaks();
        self.condense_contractions();
        self.condense_dotted_initialisms();
        self.condense_number_suffixes();
        self.condense_ellipsis();
        self.condense_latin();
        self.condense_filename_extensions();
        self.match_quotes();

        let chunker = burn_chunker();
        let tagger = brill_tagger();

        for sent in self.tokens.iter_sentences_mut() {
            let token_strings: Vec<_> = sent
                .iter()
                .filter(|t| !t.kind.is_whitespace())
                .map(|t| t.span.get_content_string(&self.source))
                .collect();

            let token_tags = tagger.tag_sentence(&token_strings);
            let np_flags = chunker.chunk_sentence(&token_strings, &token_tags);

            let mut i = 0;

            // Annotate word metadata
            for token in sent.iter_mut() {
                if let TokenKind::Word(meta) = &mut token.kind {
                    let word_source = token.span.get_content(&self.source);
                    let mut found_meta = dictionary.get_word_metadata(word_source).cloned();

                    if let Some(inner) = &mut found_meta {
                        inner.pos_tag = token_tags[i].or_else(|| inner.infer_pos_tag());
                        inner.np_member = Some(np_flags[i]);
                    }

                    *meta = found_meta;
                    i += 1;
                } else if !token.kind.is_whitespace() {
                    i += 1;
                }
            }
        }
    }

    /// Convert all sets of newlines greater than 2 to paragraph breaks.
    fn newlines_to_breaks(&mut self) {
        for token in &mut self.tokens {
            if let TokenKind::Newline(n) = token.kind {
                if n >= 2 {
                    token.kind = TokenKind::ParagraphBreak;
                }
            }
        }
    }

    /// Given a list of indices, this function removes the subsequent
    /// `stretch_len - 1` elements after each index.
    ///
    /// Will extend token spans to include removed elements.
    /// Assumes condensed tokens are contiguous in source text.
    fn condense_indices(&mut self, indices: &[usize], stretch_len: usize) {
        // Update spans
        for idx in indices {
            let end_tok = self.tokens[idx + stretch_len - 1].clone();
            let start_tok = &mut self.tokens[*idx];

            start_tok.span.end = end_tok.span.end;
        }

        // Trim
        let old = self.tokens.clone();
        self.tokens.clear();

        // Keep first chunk.
        self.tokens
            .extend_from_slice(&old[0..indices.first().copied().unwrap_or(indices.len())]);

        let mut iter = indices.iter().peekable();

        while let (Some(a_idx), b) = (iter.next(), iter.peek()) {
            self.tokens.push(old[*a_idx].clone());

            if let Some(b_idx) = b {
                self.tokens
                    .extend_from_slice(&old[a_idx + stretch_len..**b_idx]);
            }
        }

        // Keep last chunk.
        self.tokens.extend_from_slice(
            &old[indices
                .last()
                .map(|v| v + stretch_len)
                .unwrap_or(indices.len())..],
        );
    }

    pub fn get_token_at_char_index(&self, char_index: usize) -> Option<&Token> {
        let index = self
            .tokens
            .binary_search_by(|t| {
                if t.span.overlaps_with(Span::new_with_len(char_index, 1)) {
                    Ordering::Equal
                } else {
                    t.span.start.cmp(&char_index)
                }
            })
            .ok()?;

        Some(&self.tokens[index])
    }

    /// Defensively attempt to grab a specific token.
    pub fn get_token(&self, index: usize) -> Option<&Token> {
        self.tokens.get(index)
    }

    /// Get a token at a signed offset from a base index, or None if out of bounds.
    pub fn get_token_offset(&self, base: usize, offset: isize) -> Option<&Token> {
        match base.checked_add_signed(offset) {
            None => None,
            Some(idx) => self.get_token(idx),
        }
    }

    /// Get an iterator over all the tokens contained in the document.
    pub fn tokens(&self) -> impl Iterator<Item = &Token> + '_ {
        self.tokens.iter()
    }

    pub fn iter_nominal_phrases(&self) -> impl Iterator<Item = &[Token]> {
        fn is_np_member(t: &Token) -> bool {
            t.kind
                .as_word()
                .and_then(|x| x.as_ref())
                .and_then(|w| w.np_member)
                .unwrap_or(false)
        }

        fn trim(slice: &[Token]) -> &[Token] {
            let mut start = 0;
            let mut end = slice.len();
            while start < end && slice[start].kind.is_whitespace() {
                start += 1;
            }
            while end > start && slice[end - 1].kind.is_whitespace() {
                end -= 1;
            }
            &slice[start..end]
        }

        self.tokens
            .as_slice()
            .split(|t| !(is_np_member(t) || t.kind.is_whitespace()))
            .filter_map(|s| {
                let s = trim(s);
                if s.iter().any(is_np_member) {
                    Some(s)
                } else {
                    None
                }
            })
    }

    /// Get an iterator over all the tokens contained in the document.
    pub fn fat_tokens(&self) -> impl Iterator<Item = FatToken> + '_ {
        self.tokens().map(|token| token.to_fat(&self.source))
    }

    /// Get the next or previous word token relative to a base index, if separated by whitespace.
    /// Returns None if the next/previous token is not a word or does not exist.
    pub fn get_next_word_from_offset(&self, base: usize, offset: isize) -> Option<&Token> {
        // Look for whitespace at the expected offset
        if !self.get_token_offset(base, offset)?.kind.is_whitespace() {
            return None;
        }
        // Now look beyond the whitespace for a word token
        let word_token = self.get_token_offset(base, offset + offset.signum());
        let word_token = word_token?;
        word_token.kind.is_word().then_some(word_token)
    }

    /// Get an iterator over all the tokens contained in the document.
    pub fn fat_string_tokens(&self) -> impl Iterator<Item = FatStringToken> + '_ {
        self.fat_tokens().map(|t| t.into())
    }

    pub fn get_span_content(&self, span: &Span<char>) -> &[char] {
        span.get_content(&self.source)
    }

    pub fn get_span_content_str(&self, span: &Span<char>) -> String {
        String::from_iter(self.get_span_content(span))
    }

    pub fn get_full_string(&self) -> String {
        self.get_span_content_str(&Span::new(0, self.source.len()))
    }

    pub fn get_full_content(&self) -> &[char] {
        &self.source
    }

    pub fn get_source(&self) -> &[char] {
        &self.source
    }

    pub fn get_tokens(&self) -> &[Token] {
        &self.tokens
    }

    /// Searches for quotation marks and fills the
    /// [`Punctuation::Quote::twin_loc`] field. This is on a best-effort
    /// basis.
    ///
    /// Current algorithm is basic and could use some work.
    fn match_quotes(&mut self) {
        let quote_indices: Vec<usize> = self.tokens.iter_quote_indices().collect();

        for i in 0..quote_indices.len() / 2 {
            let a_i = quote_indices[i * 2];
            let b_i = quote_indices[i * 2 + 1];

            {
                let a = self.tokens[a_i].kind.as_mut_quote().unwrap();
                a.twin_loc = Some(b_i);
            }

            {
                let b = self.tokens[b_i].kind.as_mut_quote().unwrap();
                b.twin_loc = Some(a_i);
            }
        }
    }

    /// Searches for number suffixes and condenses them down into single tokens
    fn condense_number_suffixes(&mut self) {
        if self.tokens.len() < 2 {
            return;
        }

        let mut replace_starts = Vec::new();

        for idx in 0..self.tokens.len() - 1 {
            let b = &self.tokens[idx + 1];
            let a = &self.tokens[idx];

            // TODO: Allow spaces between `a` and `b`

            if let (TokenKind::Number(..), TokenKind::Word(..)) = (&a.kind, &b.kind) {
                if let Some(found_suffix) =
                    OrdinalSuffix::from_chars(self.get_span_content(&b.span))
                {
                    self.tokens[idx].kind.as_mut_number().unwrap().suffix = Some(found_suffix);
                    replace_starts.push(idx);
                }
            }
        }

        self.condense_indices(&replace_starts, 2);
    }

    /// Searches for multiple sequential space tokens and condenses them down
    /// into one.
    fn condense_spaces(&mut self) {
        let mut cursor = 0;
        let copy = self.tokens.clone();

        let mut remove_these = VecDeque::new();

        while cursor < self.tokens.len() {
            // Locate a stretch of one or more newline tokens.
            let start_tok = &mut self.tokens[cursor];

            if let TokenKind::Space(start_count) = &mut start_tok.kind {
                loop {
                    cursor += 1;

                    if cursor >= copy.len() {
                        break;
                    }

                    let child_tok = &copy[cursor];

                    // Only condense adjacent spans
                    if start_tok.span.end != child_tok.span.start {
                        break;
                    }

                    if let TokenKind::Space(n) = child_tok.kind {
                        *start_count += n;
                        start_tok.span.end = child_tok.span.end;
                        remove_these.push_back(cursor);
                        cursor += 1;
                    } else {
                        break;
                    };
                }
            }

            cursor += 1;
        }

        self.tokens.remove_indices(remove_these);
    }

    thread_local! {
        static LATIN_EXPR: Lrc<FirstMatchOf> = Document::uncached_latin_expr();
    }

    fn uncached_latin_expr() -> Lrc<FirstMatchOf> {
        Lrc::new(FirstMatchOf::new(vec![
            Box::new(
                SequenceExpr::default()
                    .then(WordSet::new(&["etc", "vs"]))
                    .then_period(),
            ),
            Box::new(
                SequenceExpr::aco("et")
                    .then_whitespace()
                    .t_aco("al")
                    .then_period(),
            ),
        ]))
    }

    /// Assumes that the first matched token is the canonical one to be condensed into.
    /// Takes a callback that can be used to retroactively edit the canonical token afterwards.
    fn condense_expr<F>(&mut self, expr: &impl Expr, edit: F)
    where
        F: Fn(&mut Token),
    {
        let matches = expr.iter_matches_in_doc(self).collect::<Vec<_>>();

        let mut remove_indices = VecDeque::with_capacity(matches.len());

        for m in matches {
            remove_indices.extend(m.start + 1..m.end);
            self.tokens[m.start].span = self.tokens[m.into_iter()].span().unwrap();
            edit(&mut self.tokens[m.start]);
        }

        self.tokens.remove_indices(remove_indices);
    }

    fn condense_latin(&mut self) {
        self.condense_expr(&Self::LATIN_EXPR.with(|v| v.clone()), |_| {})
    }

    /// Searches for multiple sequential newline tokens and condenses them down
    /// into one.
    fn condense_newlines(&mut self) {
        let mut cursor = 0;
        let copy = self.tokens.clone();

        let mut remove_these = VecDeque::new();

        while cursor < self.tokens.len() {
            // Locate a stretch of one or more newline tokens.
            let start_tok = &mut self.tokens[cursor];

            if let TokenKind::Newline(start_count) = &mut start_tok.kind {
                loop {
                    cursor += 1;

                    if cursor >= copy.len() {
                        break;
                    }

                    let child_tok = &copy[cursor];
                    if let TokenKind::Newline(n) = child_tok.kind {
                        *start_count += n;
                        start_tok.span.end = child_tok.span.end;
                        remove_these.push_back(cursor);
                        cursor += 1;
                    } else {
                        break;
                    };
                }
            }

            cursor += 1;
        }

        self.tokens.remove_indices(remove_these);
    }

    /// Condenses words like "i.e.", "e.g." and "N.S.A." down to single words
    /// using a state machine.
    fn condense_dotted_initialisms(&mut self) {
        if self.tokens.len() < 2 {
            return;
        }

        let mut to_remove = VecDeque::new();

        let mut cursor = 1;

        let mut initialism_start = None;

        loop {
            let a = &self.tokens[cursor - 1];
            let b = &self.tokens[cursor];

            let is_initialism_chunk = a.kind.is_word() && a.span.len() == 1 && b.kind.is_period();

            if is_initialism_chunk {
                if initialism_start.is_none() {
                    initialism_start = Some(cursor - 1);
                } else {
                    to_remove.push_back(cursor - 1);
                }

                to_remove.push_back(cursor);
                cursor += 1;
            } else {
                if let Some(start) = initialism_start {
                    let end = self.tokens[cursor - 2].span.end;
                    let start_tok: &mut Token = &mut self.tokens[start];
                    start_tok.span.end = end;
                }

                initialism_start = None;
            }

            cursor += 1;

            if cursor >= self.tokens.len() - 1 {
                break;
            }
        }

        self.tokens.remove_indices(to_remove);
    }

    /// Condenses likely filename extensions down to single tokens.
    fn condense_filename_extensions(&mut self) {
        if self.tokens.len() < 2 {
            return;
        }

        let mut to_remove = VecDeque::new();

        let mut cursor = 1;

        let mut ext_start = None;

        loop {
            // left context, dot, extension, right context
            let l = self.get_token_offset(cursor, -2);
            let d = &self.tokens[cursor - 1];
            let x = &self.tokens[cursor];
            let r = self.get_token_offset(cursor, 1);

            let is_ext_chunk = d.kind.is_period()
                && x.kind.is_word()
                && x.span.len() <= 3
                && ((l.is_none_or(|t| t.kind.is_whitespace())
                    && r.is_none_or(|t| t.kind.is_whitespace()))
                    || (l.is_some_and(|t| t.kind.is_open_round())
                        && r.is_some_and(|t| t.kind.is_close_round())))
                && {
                    let ext_chars = x.span.get_content(&self.source);
                    ext_chars.iter().all(|c| c.is_ascii_lowercase())
                        || ext_chars.iter().all(|c| c.is_ascii_uppercase())
                };

            if is_ext_chunk {
                if ext_start.is_none() {
                    ext_start = Some(cursor - 1);
                    self.tokens[cursor - 1].kind = TokenKind::Unlintable;
                } else {
                    to_remove.push_back(cursor - 1);
                }

                to_remove.push_back(cursor);
                cursor += 1;
            } else {
                if let Some(start) = ext_start {
                    let end = self.tokens[cursor - 2].span.end;
                    let start_tok: &mut Token = &mut self.tokens[start];
                    start_tok.span.end = end;
                }

                ext_start = None;
            }

            cursor += 1;

            if cursor >= self.tokens.len() {
                break;
            }
        }

        self.tokens.remove_indices(to_remove);
    }

    fn uncached_ellipsis_pattern() -> Lrc<Repeating> {
        let period = SequenceExpr::default().then_period();
        Lrc::new(Repeating::new(Box::new(period), 2))
    }

    thread_local! {
        static ELLIPSIS_EXPR: Lrc<Repeating> = Document::uncached_ellipsis_pattern();
    }

    fn condense_ellipsis(&mut self) {
        let expr = Self::ELLIPSIS_EXPR.with(|v| v.clone());
        self.condense_expr(&expr, |tok| {
            tok.kind = TokenKind::Punctuation(Punctuation::Ellipsis)
        });
    }

    fn uncached_contraction_expr() -> Lrc<SequenceExpr> {
        Lrc::new(
            SequenceExpr::default()
                .then_any_word()
                .then_apostrophe()
                .then_any_word(),
        )
    }

    thread_local! {
        static CONTRACTION_EXPR: Lrc<SequenceExpr> = Document::uncached_contraction_expr();
    }

    /// Searches for contractions and condenses them down into single
    /// tokens.
    fn condense_contractions(&mut self) {
        let expr = Self::CONTRACTION_EXPR.with(|v| v.clone());

        self.condense_expr(&expr, |_| {})
    }
}

/// Creates functions necessary to implement [`TokenStringExt]` on a document.
macro_rules! create_fns_on_doc {
    ($thing:ident) => {
        paste! {
            fn [< first_ $thing >](&self) -> Option<&Token> {
                self.tokens.[< first_ $thing >]()
            }

            fn [< last_ $thing >](&self) -> Option<&Token> {
                self.tokens.[< last_ $thing >]()
            }

            fn [< last_ $thing _index>](&self) -> Option<usize> {
                self.tokens.[< last_ $thing _index >]()
            }

            fn [<iter_ $thing _indices>](&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
                self.tokens.[< iter_ $thing _indices >]()
            }

            fn [<iter_ $thing s>](&self) -> impl Iterator<Item = &Token> + '_ {
                self.tokens.[< iter_ $thing s >]()
            }
        }
    };
}

impl TokenStringExt for Document {
    create_fns_on_doc!(adjective);
    create_fns_on_doc!(apostrophe);
    create_fns_on_doc!(at);
    create_fns_on_doc!(chunk_terminator);
    create_fns_on_doc!(comma);
    create_fns_on_doc!(conjunction);
    create_fns_on_doc!(currency);
    create_fns_on_doc!(ellipsis);
    create_fns_on_doc!(hostname);
    create_fns_on_doc!(likely_homograph);
    create_fns_on_doc!(noun);
    create_fns_on_doc!(number);
    create_fns_on_doc!(paragraph_break);
    create_fns_on_doc!(pipe);
    create_fns_on_doc!(preposition);
    create_fns_on_doc!(punctuation);
    create_fns_on_doc!(quote);
    create_fns_on_doc!(sentence_terminator);
    create_fns_on_doc!(space);
    create_fns_on_doc!(unlintable);
    create_fns_on_doc!(verb);
    create_fns_on_doc!(word);
    create_fns_on_doc!(word_like);

    fn first_sentence_word(&self) -> Option<&Token> {
        self.tokens.first_sentence_word()
    }

    fn first_non_whitespace(&self) -> Option<&Token> {
        self.tokens.first_non_whitespace()
    }

    fn span(&self) -> Option<Span<char>> {
        self.tokens.span()
    }

    fn iter_linking_verb_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.tokens.iter_linking_verb_indices()
    }

    fn iter_linking_verbs(&self) -> impl Iterator<Item = &Token> + '_ {
        self.tokens.iter_linking_verbs()
    }

    fn iter_chunks(&self) -> impl Iterator<Item = &'_ [Token]> + '_ {
        self.tokens.iter_chunks()
    }

    fn iter_paragraphs(&self) -> impl Iterator<Item = &'_ [Token]> + '_ {
        self.tokens.iter_paragraphs()
    }

    fn iter_sentences(&self) -> impl Iterator<Item = &'_ [Token]> + '_ {
        self.tokens.iter_sentences()
    }

    fn iter_sentences_mut(&mut self) -> impl Iterator<Item = &'_ mut [Token]> + '_ {
        self.tokens.iter_sentences_mut()
    }
}

impl Display for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for token in &self.tokens {
            write!(f, "{}", self.get_span_content_str(&token.span))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;

    use super::Document;
    use crate::{Span, parsers::MarkdownOptions};

    fn assert_condensed_contractions(text: &str, final_tok_count: usize) {
        let document = Document::new_plain_english_curated(text);

        assert_eq!(document.tokens.len(), final_tok_count);

        let document = Document::new_markdown_curated(text, MarkdownOptions::default());

        assert_eq!(document.tokens.len(), final_tok_count);
    }

    #[test]
    fn simple_contraction() {
        assert_condensed_contractions("isn't", 1);
    }

    #[test]
    fn simple_contraction2() {
        assert_condensed_contractions("wasn't", 1);
    }

    #[test]
    fn simple_contraction3() {
        assert_condensed_contractions("There's", 1);
    }

    #[test]
    fn medium_contraction() {
        assert_condensed_contractions("isn't wasn't", 3);
    }

    #[test]
    fn medium_contraction2() {
        assert_condensed_contractions("There's no way", 5);
    }

    #[test]
    fn selects_token_at_char_index() {
        let text = "There were three little pigs. They built three little homes.";
        let document = Document::new_plain_english_curated(text);

        let got = document.get_token_at_char_index(19).unwrap();

        assert!(got.kind.is_word());
        assert_eq!(got.span, Span::new(17, 23));
    }

    fn assert_token_count(source: &str, count: usize) {
        let document = Document::new_plain_english_curated(source);

        dbg!(document.tokens().map(|t| t.kind.clone()).collect_vec());
        assert_eq!(document.tokens.len(), count);
    }

    #[test]
    fn condenses_number_suffixes() {
        assert_token_count("1st", 1);
        assert_token_count("This is the 2nd test", 9);
        assert_token_count("This is the 3rd test", 9);
        assert_token_count(
            "It works even with weird capitalization like this: 600nD",
            18,
        );
    }

    #[test]
    fn condenses_ie() {
        assert_token_count("There is a thing (i.e. that one)", 15);
        assert_token_count("We are trying to condense \"i.e.\"", 13);
        assert_token_count(r#"Condenses words like "i.e.", "e.g." and "N.S.A.""#, 20);
    }

    #[test]
    fn condenses_eg() {
        assert_token_count("We are trying to condense \"e.g.\"", 13);
        assert_token_count(r#"Condenses words like "i.e.", "e.g." and "N.S.A.""#, 20);
    }

    #[test]
    fn condenses_nsa() {
        assert_token_count(r#"Condenses words like "i.e.", "e.g." and "N.S.A.""#, 20);
    }

    #[test]
    fn parses_ellipsis() {
        assert_token_count("...", 1);
    }

    #[test]
    fn parses_long_ellipsis() {
        assert_token_count(".....", 1);
    }

    #[test]
    fn parses_short_ellipsis() {
        assert_token_count("..", 1);
    }

    #[test]
    fn selects_token_at_offset() {
        let doc = Document::new_plain_english_curated("Foo bar baz");

        let tok = doc.get_token_offset(1, -1).unwrap();

        assert_eq!(tok.span, Span::new(0, 3));
    }

    #[test]
    fn cant_select_token_before_start() {
        let doc = Document::new_plain_english_curated("Foo bar baz");

        let tok = doc.get_token_offset(0, -1);

        assert!(tok.is_none());
    }

    #[test]
    fn select_next_word_pos_offset() {
        let doc = Document::new_plain_english_curated("Foo bar baz");

        let bar = doc.get_next_word_from_offset(0, 1).unwrap();
        let bar = doc.get_span_content(&bar.span);
        assert_eq!(bar, ['b', 'a', 'r']);
    }

    #[test]
    fn select_next_word_neg_offset() {
        let doc = Document::new_plain_english_curated("Foo bar baz");

        let bar = doc.get_next_word_from_offset(2, -1).unwrap();
        let bar = doc.get_span_content(&bar.span);
        assert_eq!(bar, ['F', 'o', 'o']);
    }

    #[test]
    fn cant_select_next_word_not_from_whitespace() {
        let doc = Document::new_plain_english_curated("Foo bar baz");

        let tok = doc.get_next_word_from_offset(0, 2);

        assert!(tok.is_none());
    }

    #[test]
    fn cant_select_next_word_before_start() {
        let doc = Document::new_plain_english_curated("Foo bar baz");

        let tok = doc.get_next_word_from_offset(0, -1);

        assert!(tok.is_none());
    }

    #[test]
    fn cant_select_next_word_with_punctuation_instead_of_whitespace() {
        let doc = Document::new_plain_english_curated("Foo, bar, baz");

        let tok = doc.get_next_word_from_offset(0, 1);

        assert!(tok.is_none());
    }

    #[test]
    fn cant_select_next_word_with_punctuation_after_whitespace() {
        let doc = Document::new_plain_english_curated("Foo \"bar\", baz");

        let tok = doc.get_next_word_from_offset(0, 1);

        assert!(tok.is_none());
    }

    #[test]
    fn condenses_filename_extensions() {
        let doc = Document::new_plain_english_curated(".c and .exe and .js");
        assert!(doc.tokens[0].kind.is_unlintable());
        assert!(doc.tokens[4].kind.is_unlintable());
        assert!(doc.tokens[8].kind.is_unlintable());
    }

    #[test]
    fn condense_filename_extension_ok_at_start_and_end() {
        let doc = Document::new_plain_english_curated(".c and .EXE");
        assert!(doc.tokens.len() == 5);
        assert!(doc.tokens[0].kind.is_unlintable());
        assert!(doc.tokens[4].kind.is_unlintable());
    }

    #[test]
    fn doesnt_condense_filename_extensions_with_mixed_case() {
        let doc = Document::new_plain_english_curated(".c and .Exe");
        assert!(doc.tokens.len() == 6);
        assert!(doc.tokens[0].kind.is_unlintable());
        assert!(doc.tokens[4].kind.is_punctuation());
        assert!(doc.tokens[5].kind.is_word());
    }

    #[test]
    fn doesnt_condense_filename_extensions_with_non_letters() {
        let doc = Document::new_plain_english_curated(".COM and .C0M");
        assert!(doc.tokens.len() == 6);
        assert!(doc.tokens[0].kind.is_unlintable());
        assert!(doc.tokens[4].kind.is_punctuation());
        assert!(doc.tokens[5].kind.is_word());
    }

    #[test]
    fn doesnt_condense_filename_extensions_longer_than_three() {
        let doc = Document::new_plain_english_curated(".dll and .dlls");
        assert!(doc.tokens.len() == 6);
        assert!(doc.tokens[0].kind.is_unlintable());
        assert!(doc.tokens[4].kind.is_punctuation());
        assert!(doc.tokens[5].kind.is_word());
    }

    #[test]
    fn condense_filename_extension_in_parens() {
        let doc = Document::new_plain_english_curated(
            "true for the manual installation when trying to run the executable(.exe) after a manual download",
        );
        assert!(doc.tokens.len() > 23);
        assert!(doc.tokens[21].kind.is_open_round());
        assert!(doc.tokens[22].kind.is_unlintable());
        assert!(doc.tokens[23].kind.is_close_round());
    }
}
