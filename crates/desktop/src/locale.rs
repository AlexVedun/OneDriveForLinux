use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    Ukrainian,
}

impl Language {
    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Ukrainian => "uk",
        }
    }

    pub fn from_code(value: &str) -> Self {
        match value.trim() {
            "uk" => Self::Ukrainian,
            _ => Self::English,
        }
    }
}

fn preference_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("onedrive-linux").join("language"))
}

static LANGUAGE: OnceLock<Language> = OnceLock::new();

pub fn current() -> Language {
    *LANGUAGE.get_or_init(|| {
        preference_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .map(|value| Language::from_code(&value))
            .unwrap_or(Language::English)
    })
}

pub fn text(english: &'static str, ukrainian: &'static str) -> &'static str {
    match current() {
        Language::English => english,
        Language::Ukrainian => ukrainian,
    }
}

// Translate the daemon-derived summaries at the presentation boundary. Keeping
// their source strings stable avoids changing the diagnostics and queue tests.
pub fn summary(source: &str) -> String {
    summary_for(current(), source)
}

fn summary_for(language: Language, source: &str) -> String {
    const PHRASES: &[(&str, &str, &str)] = &[
        (
            "Локальных изменений для отправки нет",
            "No local changes to upload",
            "Немає локальних змін для надсилання",
        ),
        ("Черновики:", "Drafts:", "Чернетки:"),
        ("требуют внимания:", "need attention:", "потребують уваги:"),
        ("ошибки отправки:", "upload errors:", "помилки надсилання:"),
        (
            "Откройте полный список для проверки.",
            "Open the full list to review.",
            "Відкрийте повний список для перевірки.",
        ),
        (
            "Предыдущая фоновая отправка:",
            "Previous background upload:",
            "Попереднє фонове надсилання:",
        ),
        ("И ещё", "And", "І ще"),
        (
            "локальных изменений",
            "more local changes",
            "локальних змін",
        ),
        (
            "готов к отправке; версия в каталоге совпадает",
            "ready to upload; catalog version matches",
            "готовий до надсилання; версія в каталозі збігається",
        ),
        (
            "новый файл сохранён локально; ожидает отправки",
            "new file saved locally; awaiting upload",
            "новий файл збережено локально; очікує надсилання",
        ),
        (
            "пустой новый файл сохранён локально; для отправки нужен экспериментальный режим",
            "empty new file saved locally; experimental upload mode required",
            "новий порожній файл збережено локально; потрібен експериментальний режим надсилання",
        ),
        (
            "другая учётная запись или диск",
            "different account or drive",
            "інший обліковий запис або диск",
        ),
        (
            "каталог ещё не сверён",
            "catalog not yet reconciled",
            "каталог ще не звірено",
        ),
        (
            "файл отсутствует в каталоге",
            "file missing from catalog",
            "файлу немає в каталозі",
        ),
        (
            "объект больше не является файлом",
            "item is no longer a file",
            "об’єкт більше не є файлом",
        ),
        (
            "версия в каталоге изменилась",
            "catalog version changed",
            "версія в каталозі змінилася",
        ),
        (
            "в OneDrive уже есть объект с таким именем",
            "an item with this name already exists in OneDrive",
            "об’єкт із такою назвою вже існує в OneDrive",
        ),
        (
            "результат создания файла неизвестен; проверяется единственный объект с тем же именем",
            "file creation outcome unknown; checking the only item with the same name",
            "результат створення файлу невідомий; перевіряється єдиний об’єкт із такою назвою",
        ),
        (
            "результат отправки пустого файла неизвестен; требуется проверка в OneDrive",
            "empty-file upload outcome unknown; check OneDrive",
            "результат надсилання порожнього файлу невідомий; перевірте OneDrive",
        ),
        (
            "результат отправки файла неизвестен; требуется проверка содержимого в OneDrive",
            "upload outcome unknown; compare content in OneDrive",
            "результат надсилання файлу невідомий; порівняйте вміст у OneDrive",
        ),
        (
            "результат создания пустого файла неизвестен; требуется сравнение с OneDrive",
            "empty-file creation outcome unknown; compare with OneDrive",
            "результат створення порожнього файлу невідомий; порівняйте з OneDrive",
        ),
        ("неизвестное состояние", "unknown state", "невідомий стан"),
        (
            "нужен повторный вход",
            "sign-in required again",
            "потрібен повторний вхід",
        ),
        (
            "облачный файл изменился",
            "cloud file changed",
            "хмарний файл змінився",
        ),
        ("ошибка передачи", "transfer failed", "помилка передавання"),
        ("ошибка отправки", "upload failed", "помилка надсилання"),
        ("Папки:", "Folders:", "Теки:"),
        ("к отправке", "to upload", "до надсилання"),
        (
            "ждут родителя",
            "awaiting parent",
            "очікують батьківську теку",
        ),
        ("проверяются", "being checked", "перевіряються"),
        ("созданы", "created", "створено"),
        ("требуют сверки", "need review", "потребують звірки"),
        ("Папка «", "Folder “", "Тека «"),
        ("имя занято", "name already in use", "назву вже зайнято"),
        (
            "родитель не найден",
            "parent not found",
            "батьківську теку не знайдено",
        ),
        ("Операции:", "Operations:", "Операції:"),
        ("ожидают", "pending", "очікують"),
        ("выполнены", "completed", "виконано"),
        (
            "Массовое удаление приостановлено.",
            "Bulk deletion is paused.",
            "Масове видалення призупинено.",
        ),
        ("Конфликт:", "Conflict:", "Конфлікт:"),
        (
            "FUSE: отсутствует /dev/fuse. Проверьте модуль ядра и доступность устройства.",
            "FUSE: /dev/fuse is missing. Check the kernel module and device.",
            "FUSE: немає /dev/fuse. Перевірте модуль ядра та пристрій.",
        ),
        (
            "FUSE: нет доступа к /dev/fuse. Проверьте разрешения устройства.",
            "FUSE: no access to /dev/fuse. Check device permissions.",
            "FUSE: немає доступу до /dev/fuse. Перевірте права пристрою.",
        ),
        (
            "FUSE: не удалось проверить /dev/fuse.",
            "FUSE: could not check /dev/fuse.",
            "FUSE: не вдалося перевірити /dev/fuse.",
        ),
        (
            "FUSE: устройство доступно. Помощник fusermount3 не найден; он может потребоваться для подключения.",
            "FUSE: device available. fusermount3 was not found; mounting may need it.",
            "FUSE: пристрій доступний. fusermount3 не знайдено; він може знадобитися для підключення.",
        ),
        (
            "FUSE: устройство и помощник монтирования доступны.",
            "FUSE: device and mount helper available.",
            "FUSE: пристрій і помічник монтування доступні.",
        ),
        (
            "FUSE: неизвестное состояние устройства.",
            "FUSE: unknown device state.",
            "FUSE: невідомий стан пристрою.",
        ),
    ];
    let index = match language {
        Language::English => 1,
        Language::Ukrainian => 2,
    };
    let mut result = source.to_owned();
    for phrase in PHRASES {
        result = result.replace(phrase.0, if index == 1 { phrase.1 } else { phrase.2 });
    }
    result
}

pub fn save(language: Language) -> std::io::Result<()> {
    let path = preference_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "configuration directory not found",
        )
    })?;
    fs::create_dir_all(path.parent().expect("language preference has a parent"))?;
    fs::write(path, format!("{}\n", language.code()))
}

#[cfg(test)]
mod tests {
    use super::{Language, summary_for};

    #[test]
    fn english_is_default_and_ukrainian_is_explicit() {
        assert_eq!(Language::from_code("uk\n"), Language::Ukrainian);
        assert_eq!(Language::from_code("en"), Language::English);
        assert_eq!(Language::from_code("ru"), Language::English);
    }

    #[test]
    fn summary_translates_rendered_draft_status_without_changing_ids() {
        assert_eq!(
            summary_for(
                Language::English,
                "Черновики: 1 · требуют внимания: 1 · ошибки отправки: 0.\n• project.txt (E23) — версия в каталоге изменилась"
            ),
            "Drafts: 1 · need attention: 1 · upload errors: 0.\n• project.txt (E23) — catalog version changed"
        );
        assert!(summary_for(Language::Ukrainian, "Черновики: 1").starts_with("Чернетки:"));
    }
}
