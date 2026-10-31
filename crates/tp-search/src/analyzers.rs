//! Анализатор текста индекса (SRS 4.6): слова, нижний регистр, «ё» → «е», стеммеры Snowball.
//!
//! Цепочка Tantivy: `SimpleTokenizer` (слова из букв и цифр Unicode) → отбрасывание слишком длинных слов
//! (base64, хэши) → `LowerCaser` → «ё» → «е» → `Stemmer(Russian)` → `Stemmer(English)`.
//! Стеммер выбирается по письменности слова: алгоритм Snowball каждого языка меняет только слова своего
//! алфавита (русский — кириллицу, английский — латиницу), поэтому слово проходит оба, а изменяет его один.
//! Один и тот же анализатор применяется при индексации и к тексту запроса.
//!
//! Изменение анализатора меняет термы индекса: требует увеличить [`crate::SCHEMA_VERSION`] (перестройка).

use tantivy::Index;
use tantivy::tokenizer::{
    Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, TextAnalyzer, Token,
    TokenFilter, TokenStream, Tokenizer,
};

/// Имя анализатора текстовых полей в менеджере токенизаторов индекса.
pub const TOKENIZER: &str = "tp_text";

/// Слова длиннее, байт, не индексируются (часть схемы индекса, не настройка).
const MAX_WORD_BYTES: usize = 64;

/// Анализатор текстовых полей индекса и текста запроса.
pub fn analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_WORD_BYTES))
        .filter(LowerCaser)
        .filter(YoToYe)
        .filter(Stemmer::new(Language::Russian))
        .filter(Stemmer::new(Language::English))
        .build()
}

/// Термы текста после анализатора, по порядку.
pub fn tokens(text: &str) -> Vec<String> {
    let mut analyzer = analyzer();
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

/// Регистрирует анализатор в индексе (после открытия: менеджер токенизаторов не сохраняется в каталоге).
pub(crate) fn register(index: &Index) {
    index.tokenizers().register(TOKENIZER, analyzer());
}

/// Фильтр «ё» → «е» (после нижнего регистра; «Ё» тоже заменяется).
#[derive(Debug, Clone, Copy)]
struct YoToYe;

impl TokenFilter for YoToYe {
    type Tokenizer<T: Tokenizer> = YoToYeFilter<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> YoToYeFilter<T> {
        YoToYeFilter(tokenizer)
    }
}

#[derive(Debug, Clone)]
struct YoToYeFilter<T>(T);

impl<T: Tokenizer> Tokenizer for YoToYeFilter<T> {
    type TokenStream<'a> = YoToYeStream<T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        YoToYeStream(self.0.token_stream(text))
    }
}

#[derive(Debug)]
struct YoToYeStream<S>(S);

impl<S: TokenStream> TokenStream for YoToYeStream<S> {
    fn advance(&mut self) -> bool {
        if !self.0.advance() {
            return false;
        }
        let text = &mut self.0.token_mut().text;
        if text.contains(['ё', 'Ё']) {
            *text = text.replace('ё', "е").replace('Ё', "Е");
        }
        true
    }

    fn token(&self) -> &Token {
        self.0.token()
    }

    fn token_mut(&mut self) -> &mut Token {
        self.0.token_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::{manager, scope};
    use crate::{IndexDoc, IndexOp};
    use tantivy::Term;
    use tantivy::collector::Count;
    use tantivy::query::TermQuery;
    use tantivy::schema::IndexRecordOption;
    use time::OffsetDateTime;
    use tp_contracts::{MailboxId, MessageId};

    #[test]
    fn words_case_yo_and_stems() {
        // Нижний регистр, разделители, «ё» → «е».
        assert_eq!(tokens("Ёлка, ЁЖ и ёлочка!"), tokens("елка еж и елочка"));
        // Формы слова сводятся к одной основе.
        let stem = tokens("письмо");
        for form in ["письма", "письмами", "Письмом", "ПИСЬМЕ"] {
            assert_eq!(tokens(form), stem, "{form}");
        }
        assert_eq!(tokens("reports"), tokens("report"));
        assert_eq!(tokens("running"), tokens("run"));
        // Стеммер по письменности: латиница не трогается русским, кириллица — английским.
        assert_eq!(
            tokens("письмами running"),
            [stem[0].clone(), "run".to_owned()]
        );
        assert_eq!(tokens("user1@example.com"), ["user1", "exampl", "com"]);
        // Длинные слова (base64) не индексируются.
        assert!(tokens(&"A".repeat(MAX_WORD_BYTES + 1)).is_empty());
    }

    #[test]
    fn letters_finds_letter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = manager(&dir);
        let index = m.open_or_create(scope()).expect("index");
        let doc = |body: &str| {
            IndexOp::Upsert(Box::new(IndexDoc {
                message: MessageId::new(),
                mailbox: MailboxId::new(),
                received_at: OffsetDateTime::UNIX_EPOCH,
                subject: String::new(),
                from: String::new(),
                to: String::new(),
                cc: String::new(),
                bcc: String::new(),
                body: body.to_owned(),
                attachments: String::new(),
                headers: Vec::new(),
            }))
        };
        index
            .apply(&[doc("Пришло письмо от бухгалтерии"), doc("Счёт на оплату")])
            .expect("apply");
        let body = index.fields().body;
        let hits = |word: &str| {
            let [t] = tokens(word).try_into().expect("one word");
            let q = TermQuery::new(Term::from_field_text(body, &t), IndexRecordOption::Basic);
            index.searcher().search(&q, &Count).expect("search")
        };
        assert_eq!(hits("письмами"), 1);
        assert_eq!(hits("счет"), 1);
        assert_eq!(hits("счетов"), 1);
        assert_eq!(hits("бухгалтерия"), 1);
        assert_eq!(hits("оплата"), 1);
        assert_eq!(hits("телеграмма"), 0);
    }
}
