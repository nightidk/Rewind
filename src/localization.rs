use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Serialize;
use windows::Win32::Globalization::GetUserDefaultUILanguage;

static ENGLISH: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../ui/strings-en.json"))
        .expect("the bundled English catalog is valid JSON")
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Locale {
    Ru,
    En,
}

impl Locale {
    pub fn system() -> Self {
        // The lower 10 bits of the Win32 LANGID identify the primary language.
        if unsafe { GetUserDefaultUILanguage() } & 0x03ff == 0x19 {
            Self::Ru
        } else {
            Self::En
        }
    }

    pub fn text<'a>(self, text: &'a str) -> Cow<'a, str> {
        if self == Self::Ru {
            return Cow::Borrowed(text);
        }
        if let Some(english) = ENGLISH.get(text) {
            return Cow::Borrowed(english);
        }
        if let Some((lead, rest)) = text.split_once(": ") {
            let heading = self.text(lead);
            let detail = self.text(rest);
            if heading != lead || detail != rest {
                return Cow::Owned(format!("{heading}: {detail}"));
            }
        }
        // Match the most specific prefix, then retain the dynamic path or diagnostic detail.
        if let Some((key, english)) = ENGLISH.iter()
            .filter(|(key, _)| matches!(text.strip_prefix(key.as_str()), Some(rest) if rest.starts_with([' ', ':', '\n'])))
            .max_by_key(|(key, _)| key.len())
        {
            return Cow::Owned(format!("{english}{}", &text[key.len()..]));
        }
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
#[test]
fn english_preserves_diagnostic_details() {
    assert_eq!(
        Locale::En.text("Не удалось создать папку D:\\Clips: Access denied"),
        "Could not create the directory D:\\Clips: Access denied"
    );
    assert_eq!(
        Locale::En.text("Аудиоустройство недоступно. Проверьте настройки звука."),
        "Audio device unavailable. Check your sound settings."
    );
}
