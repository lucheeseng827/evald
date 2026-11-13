//! Languages and scripts, for multilingual evaluation: which language a text is in (an embedded
//! trigram detector, `whatlang`, MIT, no model files and no network), which scripts its letters
//! are written in, and the Unicode normalisation that makes `exact_match` and `levenshtein` fair
//! across compatibility forms and case.
//!
//! **Malay and Indonesian are one language here.** No detector measured tells them apart: lingua's
//! own accuracy report puts its high-accuracy mode at 28% on Malay sentences (69% read as
//! Indonesian), and whatlang has no Malay at all. ISO 639 already has the honest grouping: `ms` is
//! the Malay *macrolanguage*, of which Indonesian (`id`) and Standard Malay (`zsm`) are members.
//! So a detected Malay or Indonesian text is labelled `ms`, and satisfies an expected `ms`, `id`
//! or `zsm` alike: the check catches an answer in English to a question in Malay, which is the
//! failure that matters, and does not claim a distinction it cannot make.

use unicode_normalization::UnicodeNormalization;

/// What the detector found.
#[derive(Debug, Clone, PartialEq)]
pub struct Detected {
    /// ISO 639-1 where there is one (`en`, `ms`, `th`), else ISO 639-3.
    pub code: &'static str,
    pub name: &'static str,
    pub confidence: f64,
    /// Whether to act on it: the detector was confident, or the text has at least
    /// [`MIN_LETTERS`] letters ([`detect`]).
    pub reliable: bool,
}

/// The letters below which a detection the detector is not confident about is not acted on.
/// The detector's own confidence is a poor guide for Latin-script text (correct Malay and
/// English sentences score 0.01-0.5), while every wrong detection measured was shorter than
/// this: "Thank you for waiting." read as Norwegian, "Paris" as Latin. Text in a script that
/// names its language (Thai, Tamil, Han) is confident at any length.
pub const MIN_LETTERS: usize = 20;

/// ISO 639-3 (whatlang's codes) to ISO 639-1. Indonesian maps to `ms`, the Malay macrolanguage
/// it belongs to (module docs).
const ISO_639_1: &[(&str, &str)] = &[
    ("afr", "af"),
    ("aka", "ak"),
    ("amh", "am"),
    ("ara", "ar"),
    ("aze", "az"),
    ("bel", "be"),
    ("ben", "bn"),
    ("bul", "bg"),
    ("cat", "ca"),
    ("ces", "cs"),
    ("cmn", "zh"),
    ("dan", "da"),
    ("deu", "de"),
    ("ell", "el"),
    ("eng", "en"),
    ("epo", "eo"),
    ("est", "et"),
    ("fin", "fi"),
    ("fra", "fr"),
    ("guj", "gu"),
    ("heb", "he"),
    ("hin", "hi"),
    ("hrv", "hr"),
    ("hun", "hu"),
    ("hye", "hy"),
    ("ind", "ms"),
    ("ita", "it"),
    ("jav", "jv"),
    ("jpn", "ja"),
    ("kan", "kn"),
    ("kat", "ka"),
    ("khm", "km"),
    ("kor", "ko"),
    ("lat", "la"),
    ("lav", "lv"),
    ("lit", "lt"),
    ("mal", "ml"),
    ("mar", "mr"),
    ("mkd", "mk"),
    ("mya", "my"),
    ("nep", "ne"),
    ("nld", "nl"),
    ("nob", "nb"),
    ("ori", "or"),
    ("pan", "pa"),
    ("pes", "fa"),
    ("pol", "pl"),
    ("por", "pt"),
    ("ron", "ro"),
    ("rus", "ru"),
    ("sin", "si"),
    ("slk", "sk"),
    ("slv", "sl"),
    ("sna", "sn"),
    ("spa", "es"),
    ("srp", "sr"),
    ("swe", "sv"),
    ("tam", "ta"),
    ("tel", "te"),
    ("tgl", "tl"),
    ("tha", "th"),
    ("tuk", "tk"),
    ("tur", "tr"),
    ("ukr", "uk"),
    ("urd", "ur"),
    ("uzb", "uz"),
    ("vie", "vi"),
    ("yid", "yi"),
    ("zul", "zu"),
];

/// The language of `text`, or `None` when the detector cannot say (too little text, no
/// letters).
pub fn detect(text: &str) -> Option<Detected> {
    let info = whatlang::detect(text)?;
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    let lang = info.lang();
    let code3 = lang.code();
    let code = ISO_639_1
        .iter()
        .find(|(three, _)| *three == code3)
        .map_or(code3, |(_, one)| *one);
    Some(Detected {
        code,
        name: if code == "ms" {
            "Malay/Indonesian"
        } else {
            lang.eng_name()
        },
        confidence: info.confidence(),
        reliable: info.is_reliable() || letters >= MIN_LETTERS,
    })
}

/// A language code as this module compares it: lower-case ISO 639-1 where one exists, with the
/// Malay macrolanguage's members (`id`, `ind`, `zsm`, `msa`, `may`) and the Chinese codes
/// (`cmn`, `zho`, `chi`) folded to `ms` and `zh`. A region or script subtag is dropped
/// (`ms-MY`, `zh-Hans`).
pub fn canonical(code: &str) -> String {
    let code = code.trim().to_ascii_lowercase();
    let base = code.split(['-', '_']).next().unwrap_or_default();
    match base {
        "id" | "ind" | "zsm" | "msa" | "may" => "ms".to_string(),
        "cmn" | "zho" | "chi" => "zh".to_string(),
        three if three.len() == 3 => ISO_639_1
            .iter()
            .find(|(t, _)| *t == three)
            .map_or_else(|| three.to_string(), |(_, one)| one.to_string()),
        other => other.to_string(),
    }
}

/// Whether `detected` is the `expected` language ([`canonical`] on both sides).
pub fn same_language(expected: &str, detected: &Detected) -> bool {
    canonical(expected) == canonical(detected.code)
}

/// A writing system, by Unicode block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Script {
    Latin,
    Greek,
    Cyrillic,
    Armenian,
    Hebrew,
    Arabic,
    Devanagari,
    Bengali,
    Gurmukhi,
    Gujarati,
    Oriya,
    Tamil,
    Telugu,
    Kannada,
    Malayalam,
    Sinhala,
    Thai,
    Lao,
    Myanmar,
    Georgian,
    Hangul,
    Ethiopic,
    Khmer,
    Hiragana,
    Katakana,
    Han,
}

/// Each script's ranges. Letters outside them have no script here and count against the
/// expected one.
const RANGES: &[(Script, u32, u32)] = &[
    (Script::Latin, 0x0041, 0x005A),
    (Script::Latin, 0x0061, 0x007A),
    (Script::Latin, 0x00C0, 0x024F),
    (Script::Latin, 0x1E00, 0x1EFF), // Latin Extended Additional: Vietnamese
    (Script::Latin, 0x2C60, 0x2C7F),
    (Script::Latin, 0xA720, 0xA7FF),
    (Script::Latin, 0xFF21, 0xFF3A),
    (Script::Latin, 0xFF41, 0xFF5A),
    (Script::Greek, 0x0370, 0x03FF),
    (Script::Greek, 0x1F00, 0x1FFF),
    (Script::Cyrillic, 0x0400, 0x052F),
    (Script::Armenian, 0x0530, 0x058F),
    (Script::Hebrew, 0x0590, 0x05FF),
    (Script::Arabic, 0x0600, 0x06FF), // Jawi, Malay in Arabic script, is here
    (Script::Arabic, 0x0750, 0x077F),
    (Script::Arabic, 0x08A0, 0x08FF),
    (Script::Arabic, 0xFB50, 0xFDFF),
    (Script::Arabic, 0xFE70, 0xFEFF),
    (Script::Devanagari, 0x0900, 0x097F),
    (Script::Bengali, 0x0980, 0x09FF),
    (Script::Gurmukhi, 0x0A00, 0x0A7F),
    (Script::Gujarati, 0x0A80, 0x0AFF),
    (Script::Oriya, 0x0B00, 0x0B7F),
    (Script::Tamil, 0x0B80, 0x0BFF),
    (Script::Telugu, 0x0C00, 0x0C7F),
    (Script::Kannada, 0x0C80, 0x0CFF),
    (Script::Malayalam, 0x0D00, 0x0D7F),
    (Script::Sinhala, 0x0D80, 0x0DFF),
    (Script::Thai, 0x0E00, 0x0E7F),
    (Script::Lao, 0x0E80, 0x0EFF),
    (Script::Myanmar, 0x1000, 0x109F),
    (Script::Georgian, 0x10A0, 0x10FF),
    (Script::Hangul, 0x1100, 0x11FF),
    (Script::Hangul, 0x3130, 0x318F),
    (Script::Hangul, 0xAC00, 0xD7AF),
    (Script::Ethiopic, 0x1200, 0x139F),
    (Script::Khmer, 0x1780, 0x17FF),
    (Script::Hiragana, 0x3040, 0x309F),
    (Script::Katakana, 0x30A0, 0x30FF),
    (Script::Katakana, 0x31F0, 0x31FF),
    (Script::Han, 0x3400, 0x4DBF),
    (Script::Han, 0x4E00, 0x9FFF),
    (Script::Han, 0xF900, 0xFAFF),
    (Script::Han, 0x20000, 0x2FA1F),
];

impl Script {
    pub fn name(self) -> &'static str {
        match self {
            Script::Latin => "Latin",
            Script::Greek => "Greek",
            Script::Cyrillic => "Cyrillic",
            Script::Armenian => "Armenian",
            Script::Hebrew => "Hebrew",
            Script::Arabic => "Arabic",
            Script::Devanagari => "Devanagari",
            Script::Bengali => "Bengali",
            Script::Gurmukhi => "Gurmukhi",
            Script::Gujarati => "Gujarati",
            Script::Oriya => "Oriya",
            Script::Tamil => "Tamil",
            Script::Telugu => "Telugu",
            Script::Kannada => "Kannada",
            Script::Malayalam => "Malayalam",
            Script::Sinhala => "Sinhala",
            Script::Thai => "Thai",
            Script::Lao => "Lao",
            Script::Myanmar => "Myanmar",
            Script::Georgian => "Georgian",
            Script::Hangul => "Hangul",
            Script::Ethiopic => "Ethiopic",
            Script::Khmer => "Khmer",
            Script::Hiragana => "Hiragana",
            Script::Katakana => "Katakana",
            Script::Han => "Han",
        }
    }

    /// A script by name, case-insensitive; `Jawi` is Arabic and `Chinese` is Han.
    pub fn parse(name: &str) -> Option<Script> {
        let n = name.trim().to_ascii_lowercase();
        match n.as_str() {
            "jawi" => return Some(Script::Arabic),
            "chinese" | "hanzi" | "kanji" => return Some(Script::Han),
            "korean" => return Some(Script::Hangul),
            _ => {}
        }
        RANGES
            .iter()
            .map(|(s, _, _)| *s)
            .find(|s| s.name().to_ascii_lowercase() == n)
    }

    /// The script of `c`, when it is in one of the blocks above.
    pub fn of(c: char) -> Option<Script> {
        let u = c as u32;
        RANGES
            .iter()
            .find(|(_, lo, hi)| (*lo..=*hi).contains(&u))
            .map(|(s, _, _)| *s)
    }
}

/// The scripts a language is usually written in. Malay's default is Latin (Rumi); give
/// `script: Jawi` for Malay in Arabic script.
pub fn scripts_for(language: &str) -> Option<&'static [Script]> {
    use Script::*;
    Some(match canonical(language).as_str() {
        "en" | "ms" | "vi" | "tl" | "es" | "fr" | "de" | "it" | "pt" | "nl" | "sv" | "da"
        | "nb" | "fi" | "pl" | "cs" | "sk" | "sl" | "hr" | "ro" | "hu" | "tr" | "et" | "lv"
        | "lt" | "ca" | "af" | "zu" | "sn" | "ak" | "az" | "uz" | "tk" | "eo" | "la" | "jv" => {
            &[Latin]
        }
        "th" => &[Thai],
        "ta" => &[Tamil],
        "zh" => &[Han],
        "ja" => &[Han, Hiragana, Katakana],
        "ko" => &[Hangul, Han],
        "hi" | "mr" | "ne" => &[Devanagari],
        "ar" | "fa" | "ur" => &[Arabic],
        "ru" | "uk" | "bg" | "sr" | "mk" | "be" => &[Cyrillic],
        "el" => &[Greek],
        "he" | "yi" => &[Hebrew],
        "bn" => &[Bengali],
        "te" => &[Telugu],
        "ml" => &[Malayalam],
        "kn" => &[Kannada],
        "gu" => &[Gujarati],
        "pa" => &[Gurmukhi],
        "or" => &[Oriya],
        "si" => &[Sinhala],
        "my" => &[Myanmar],
        "km" => &[Khmer],
        "lo" => &[Lao],
        "ka" => &[Georgian],
        "hy" => &[Armenian],
        "am" => &[Ethiopic],
        _ => return None,
    })
}

/// The share of `text`'s letters written in one of `expected`, or `None` when it has no
/// letters. Digits, punctuation and spaces do not count either way; a letter in another script
/// (or none of these) counts against.
pub fn script_share(text: &str, expected: &[Script]) -> Option<f64> {
    let (mut letters, mut matching) = (0usize, 0usize);
    for c in text.chars() {
        let script = Script::of(c);
        if !(c.is_alphabetic() || script.is_some()) {
            continue;
        }
        letters += 1;
        if script.is_some_and(|s| expected.contains(&s)) {
            matching += 1;
        }
    }
    (letters > 0).then(|| matching as f64 / letters as f64)
}

/// The words of a prompt as a span recorded it: a chat request's message contents when it is
/// JSON (OpenInference `input.value` often is), else the text as it is. Detecting the language
/// of the raw JSON would read its English keys.
pub fn prompt_text(input: &str) -> String {
    fn collect(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Array(a) => a.iter().for_each(|x| collect(x, out)),
            serde_json::Value::Object(o) => {
                for (k, x) in o {
                    match (k.as_str(), x) {
                        ("content" | "text", serde_json::Value::String(s)) => out.push(s.clone()),
                        _ => collect(x, out),
                    }
                }
            }
            _ => {}
        }
    }
    let trimmed = input.trim_start();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(input) {
            let mut parts = Vec::new();
            collect(&v, &mut parts);
            if !parts.is_empty() {
                return parts.join("\n");
            }
        }
    }
    input.to_string()
}

/// `s` as `normalize: true` matching compares it: NFKC (full-width forms, ligatures and
/// compatibility characters to their plain equivalents), lower-cased (Unicode default case
/// conversion, not full case folding: `ß` stays `ß`), NFKC again, and trimmed.
pub fn normalize(s: &str) -> String {
    let lowered: String = s.nfkc().collect::<String>().to_lowercase();
    lowered.nfkc().collect::<String>().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_first_target_languages() {
        for (text, want) in [
            (
                "The quick brown fox jumps over the lazy dog near the river bank.",
                "en",
            ),
            (
                "Saya ingin membuat temu janji dengan doktor pada hari Isnin depan, boleh?",
                "ms",
            ),
            (
                "Saya ingin membuat janji dengan dokter pada hari Senin depan, bisakah?",
                "ms",
            ),
            ("ฉันต้องการนัดหมายกับแพทย์ในวันจันทร์หน้า ได้ไหมครับ", "th"),
            (
                "Tôi muốn đặt lịch hẹn với bác sĩ vào thứ Hai tuần tới được không?",
                "vi",
            ),
            ("அடுத்த திங்கட்கிழமை மருத்துவரைச் சந்திக்க விரும்புகிறேன்", "ta"),
            ("我想预约下周一看医生，可以吗？", "zh"),
        ] {
            let d = detect(text).unwrap_or_else(|| panic!("nothing for {text}"));
            assert_eq!(d.code, want, "{text}: {d:?}");
        }
        assert_eq!(detect(""), None);
    }

    /// What [`MIN_LETTERS`] rests on: everyday support answers, long and short, in the first
    /// target languages. Every detection acted on is right, and the ones that would be wrong
    /// are not acted on.
    #[test]
    fn a_detection_is_acted_on_only_when_it_can_be_trusted() {
        let placed_and_right = [
            (
                "en",
                "The clinic opens at nine in the morning on Saturdays and closes at noon.",
            ),
            (
                "en",
                "Your order has been shipped and should arrive within three working days.",
            ),
            (
                "en",
                "Please reset your password using the link we sent to your email.",
            ),
            ("en", "I cannot help with that request, sorry."),
            (
                "ms",
                "Boleh saya tahu waktu operasi klinik ini pada hari Sabtu?",
            ),
            (
                "ms",
                "Pesanan anda telah dihantar dan akan tiba dalam masa tiga hari bekerja.",
            ),
            (
                "ms",
                "Sila tetapkan semula kata laluan anda menggunakan pautan yang kami hantar.",
            ),
            ("ms", "Terima kasih kerana menunggu."),
            (
                "ms",
                "Maaf, saya tidak dapat membantu dengan permintaan itu.",
            ),
            (
                "ms",
                "Pesanan Anda telah dikirim dan akan tiba dalam tiga hari kerja.",
            ),
            ("ms", "Terima kasih sudah menunggu."),
            ("th", "ขอบคุณที่รอครับ"),
            ("th", "ใช่ ถูกต้องครับ"),
            ("vi", "Cảm ơn bạn đã chờ đợi."),
            ("vi", "Vâng, đúng vậy."),
            ("ta", "காத்திருந்ததற்கு நன்றி."),
            ("zh", "谢谢您的耐心等待。"),
            ("zh", "是的，没错。"),
            ("ja", "お待たせしました。"),
        ];
        for (want, text) in placed_and_right {
            let d = detect(text).unwrap();
            assert!(d.reliable, "{text}: {d:?}");
            assert_eq!(d.code, want, "{text}: {d:?}");
        }
        // Too little Latin-script text: the detector would be wrong, so it is not acted on.
        for text in [
            "Thank you for waiting.",
            "Paris",
            "OK",
            "42 km",
            "Ya, itu betul.",
        ] {
            assert!(!detect(text).is_some_and(|d| d.reliable), "{text}");
        }
    }

    #[test]
    fn malay_and_indonesian_are_one_macrolanguage() {
        let d =
            detect("Saya ingin membuat temu janji dengan doktor pada hari Isnin depan.").unwrap();
        for expected in ["ms", "id", "zsm", "ms-MY", "IND", "msa"] {
            assert!(same_language(expected, &d), "{expected}");
        }
        assert!(!same_language("en", &d));
        assert_eq!(canonical("zh-Hans"), "zh");
        assert_eq!(canonical("cmn"), "zh");
        assert_eq!(canonical("tha"), "th");
        assert_eq!(canonical("xyz"), "xyz");
    }

    #[test]
    fn script_share_counts_letters_only() {
        // A Thai answer that names a product in Latin letters: mostly Thai.
        let s = script_share("ราคา iPhone 15 คือ 39,900 บาท", &[Script::Thai]).unwrap();
        assert!(s > 0.6 && s < 1.0, "{s}");
        assert_eq!(script_share("ภาษาไทย", &[Script::Thai]), Some(1.0));
        assert_eq!(script_share("in English", &[Script::Thai]), Some(0.0));
        assert_eq!(
            script_share("12 + 3 = 15 !", &[Script::Latin]),
            None,
            "no letters"
        );
        // Vietnamese diacritics are Latin.
        assert_eq!(script_share("Tiếng Việt", &[Script::Latin]), Some(1.0));
        // Japanese mixes three scripts.
        let ja = scripts_for("ja").unwrap();
        assert_eq!(script_share("日本語のテキスト", ja), Some(1.0));
        assert_eq!(Script::parse("jawi"), Some(Script::Arabic));
        assert_eq!(Script::parse("TAMIL"), Some(Script::Tamil));
        assert_eq!(Script::parse("klingon"), None);
        assert_eq!(scripts_for("id"), Some(&[Script::Latin][..]));
        assert_eq!(scripts_for("tlh"), None);
    }

    #[test]
    fn a_prompt_is_read_from_its_messages() {
        let json = r#"[{"role": "system", "content": "Jawab dalam bahasa Melayu."},
                       {"role": "user", "content": [{"type": "text", "text": "Apa khabar?"}]}]"#;
        assert_eq!(prompt_text(json), "Jawab dalam bahasa Melayu.\nApa khabar?");
        assert_eq!(prompt_text("plain text"), "plain text");
        assert_eq!(prompt_text("[not json"), "[not json");
        assert_eq!(
            prompt_text(r#"{"no": 1}"#),
            r#"{"no": 1}"#,
            "no message text: as it is"
        );
    }

    #[test]
    fn normalization_folds_width_compatibility_and_case() {
        assert_eq!(normalize("  ＡＢＣ１２３ "), "abc123", "full-width forms");
        assert_eq!(normalize("ﬁle"), "file", "a ligature");
        assert_eq!(normalize("Ｋuala LUMPUR"), "kuala lumpur");
        // Composed and decomposed forms of the same Vietnamese letter compare equal.
        assert_eq!(normalize("Vie\u{0302}\u{0323}t"), normalize("Việt"));
        assert_eq!(normalize("Straße"), "straße", "not full case folding");
    }
}
