//! Presentation-only language; never changes Codex's virtual-device locale.
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub struct UiLanguage(AtomicBool);

impl UiLanguage {
    pub fn set(&self, language: &str) -> Result<(), String> {
        let russian = match language {
            "zh-CN" => false,
            "ru" => true,
            _ => return Err("Unsupported UI language".into()),
        };
        self.0.store(russian, Ordering::Relaxed);
        Ok(())
    }

    pub fn text<'a>(&self, chinese: &'a str, russian: &'a str) -> &'a str {
        if self.0.load(Ordering::Relaxed) {
            russian
        } else {
            chinese
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_languages_and_rejects_unknown_values() {
        let language = UiLanguage::default();
        assert_eq!(language.text("退出", "Выйти"), "退出");
        language.set("ru").unwrap();
        assert_eq!(language.text("退出", "Выйти"), "Выйти");
        assert!(language.set("invalid").is_err());
        assert_eq!(language.text("退出", "Выйти"), "Выйти");
        language.set("zh-CN").unwrap();
        assert_eq!(language.text("退出", "Выйти"), "退出");
    }
}
