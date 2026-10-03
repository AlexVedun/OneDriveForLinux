use std::sync::OnceLock;

static LANGUAGE: OnceLock<bool> = OnceLock::new();

pub fn text(english: &'static str, ukrainian: &'static str) -> &'static str {
    if *LANGUAGE.get_or_init(|| std::env::var("ONEDRIVE_LANGUAGE").is_ok_and(|value| value == "uk"))
    {
        ukrainian
    } else {
        english
    }
}
