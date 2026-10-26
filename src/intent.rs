//! Команды и разбор русских голосовых фраз Алисы.
//!
//! Алиса присылает окончательный текст распознавания. Здесь он превращается в команду музыкального
//! плеера или отбрасывается (тогда фразу обрабатывает сама Алиса: свет, таймеры, погода и т.д.).

use std::fmt;

/// Что именно искать по голосовому запросу.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Artist,
    Album,
    Track,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    /// Если пользователь сказал «песню», «альбом», «группу» — искать только этот вид.
    pub kind: Option<Kind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    Play(Query),
    /// Персональное радио сервера.
    Radio,
    /// Микс или полка сервера: по названию (`favorites`, `daily`, «микс дня»…).
    Mix(String),
    Resume,
    Pause,
    Next,
    Previous,
    Toggle,
    Volume(u8),
    Like,
    Dislike,
    Voice(bool),
    /// Освободить колонку для штатной музыки Алисы.
    Release,
    Retry,
}

impl fmt::Display for Intent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Intent::Play(q) => write!(f, "play «{}»", q.text),
            Intent::Radio => write!(f, "radio"),
            Intent::Mix(m) => write!(f, "mix «{m}»"),
            Intent::Resume => write!(f, "resume"),
            Intent::Pause => write!(f, "pause"),
            Intent::Next => write!(f, "next"),
            Intent::Previous => write!(f, "previous"),
            Intent::Toggle => write!(f, "toggle"),
            Intent::Volume(v) => write!(f, "volume {v}"),
            Intent::Like => write!(f, "like"),
            Intent::Dislike => write!(f, "dislike"),
            Intent::Voice(on) => write!(f, "voice {}", if *on { "on" } else { "off" }),
            Intent::Release => write!(f, "release"),
            Intent::Retry => write!(f, "retry"),
        }
    }
}

impl Intent {
    /// Команды, которые имеют смысл только если musik уже играет или стоит на паузе.
    /// Иначе фраза «следующий трек» относится к штатному плееру Алисы.
    pub fn needs_active_session(&self) -> bool {
        matches!(
            self,
            Intent::Pause | Intent::Next | Intent::Previous | Intent::Toggle | Intent::Like | Intent::Dislike
        )
    }
}

/// Нижний регистр, «ё» → «е», без знаков в конце, один пробел между словами, без обращения к Алисе.
pub fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase().replace('ё', "е");
    let trimmed = lowered.trim().trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ',' | '…' | '"' | '«' | '»'));
    let joined = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    for prefix in ["алиса, ", "алиса "] {
        if let Some(rest) = joined.strip_prefix(prefix) {
            return rest.trim().to_owned();
        }
    }
    joined
}

const PAUSE: &[&str] = &[
    "стоп",
    "пауза",
    "останови",
    "останови музыку",
    "выключи музыку",
    "поставь на паузу",
    "поставь музыку на паузу",
    "останови воспроизведение",
    "выключи трек",
];
const NEXT: &[&str] = &[
    "дальше",
    "следующий",
    "следующий трек",
    "следующая песня",
    "следующую песню",
    "переключи трек",
    "переключи песню",
    "пропусти",
    "пропусти трек",
    "пропусти песню",
];
const PREVIOUS: &[&str] =
    &["назад", "предыдущий трек", "предыдущая песня", "предыдущую песню", "верни предыдущий трек", "вернись назад"];
const RESUME: &[&str] = &[
    "продолжи",
    "продолжи музыку",
    "продолжай музыку",
    "продолжи воспроизведение",
    "возобнови",
    "возобнови музыку",
    "включи музыку",
];
const LIKE: &[&str] = &[
    "лайк",
    "поставь лайк",
    "мне нравится",
    "мне нравится эта песня",
    "мне нравится этот трек",
    "нравится",
    "нравится эта песня",
    "нравится этот трек",
    "добавь в избранное",
    "добавь в любимое",
    "добавь трек в избранное",
    "добавь песню в избранное",
    "добавь эту песню в избранное",
];
const DISLIKE: &[&str] = &[
    "дизлайк",
    "поставь дизлайк",
    "не нравится",
    "мне не нравится",
    "мне не нравится эта песня",
    "мне не нравится этот трек",
    "не нравится эта песня",
    "не нравится этот трек",
];
const PLAY_VERBS: &[&str] = &["включи ", "поставь ", "запусти ", "сыграй ", "воспроизведи "];
const RADIO: &[&str] = &[
    "радио",
    "мое радио",
    "мою волну",
    "мою радиоволну",
    "что-нибудь",
    "что нибудь",
    "что-нибудь послушать",
    "случайную музыку",
    "случайную песню",
    "любую музыку",
    "любую песню",
    "рандом",
    "музыку на мой вкус",
    "мою музыку",
    "персональное радио",
];
const FAVORITES: &[&str] = &[
    "любимое",
    "мое любимое",
    "избранное",
    "мое избранное",
    "мои любимые",
    "мои любимые песни",
    "мои любимые треки",
    "любимые песни",
    "любимые треки",
    "понравившееся",
    "лайкнутое",
];
const MIX_PREFIXES: &[&str] = &["мой микс ", "микс ", "мою подборку ", "подборку ", "мой плейлист ", "плейлист "];
/// После этих слов запрос точно не про наш каталог — оставляем Алисе.
const NOT_MUSIC: &[&str] = &[
    "свет",
    "освещени",
    "ламп",
    "люстр",
    "торшер",
    "ночник",
    "розетк",
    "телевизор",
    "тв",
    "кондиционер",
    "вентилятор",
    "обогревател",
    "пылесос",
    "робот",
    "чайник",
    "будильник",
    "таймер",
    "блютуз",
    "bluetooth",
    "режим",
    "экран",
    "микрофон",
    "громк",
    "звук",
    "отоплен",
    "увлажнител",
    "очистител",
    "зарядк",
    "сказк",
    "новост",
    "подкаст",
    "шум",
    "фильм",
    "сериал",
    "мультик",
    "канал",
    "youtube",
    "ютуб",
    "видео",
    "трансляц",
    "аудиокниг",
    "книг",
    "медитац",
    "засыпан",
    "радиостанц",
    "яндекс",
    "кинопоиск",
    "навык",
    "игр",
    "погод",
    "музыкальн",
    "радио",
];
/// Короткие основы совпадают с началом настоящих имён («Радиохед», «Игорь Тальков», «Шумов»),
/// поэтому для них слово должно совпасть целиком (с типичными падежами).
fn not_music_word(word: &str) -> bool {
    NOT_MUSIC.iter().any(|stem| {
        if stem.chars().count() > 5 {
            word.starts_with(stem)
        } else {
            word == *stem
                || ["а", "у", "ы", "е", "ой", "ом", "и"].iter().any(|end| word.strip_suffix(end) == Some(stem))
        }
    })
}
const EXPLICIT: &[(&str, Option<Kind>)] = &[
    ("песню ", Some(Kind::Track)),
    ("трек ", Some(Kind::Track)),
    ("композицию ", Some(Kind::Track)),
    ("альбом ", Some(Kind::Album)),
    ("группу ", Some(Kind::Artist)),
    ("исполнителя ", Some(Kind::Artist)),
    ("артиста ", Some(Kind::Artist)),
    ("певца ", Some(Kind::Artist)),
    ("певицу ", Some(Kind::Artist)),
    ("музыку ", None),
    ("песни ", None),
    ("треки ", None),
];

/// Разбирает фразу. `None` — это не наша команда, пусть отвечает Алиса.
pub fn from_speech(text: &str) -> Option<Intent> {
    let text = normalize(text);
    if text.is_empty() || text.len() > 512 {
        return None;
    }
    if PAUSE.contains(&text.as_str()) {
        return Some(Intent::Pause);
    }
    if NEXT.contains(&text.as_str()) {
        return Some(Intent::Next);
    }
    if PREVIOUS.contains(&text.as_str()) {
        return Some(Intent::Previous);
    }
    if RESUME.contains(&text.as_str()) {
        return Some(Intent::Resume);
    }
    if LIKE.contains(&text.as_str()) {
        return Some(Intent::Like);
    }
    if DISLIKE.contains(&text.as_str()) {
        return Some(Intent::Dislike);
    }
    let rest = PLAY_VERBS.iter().find_map(|p| text.strip_prefix(p))?.trim();
    if rest.is_empty() {
        return None;
    }
    if RADIO.contains(&rest) {
        return Some(Intent::Radio);
    }
    if FAVORITES.contains(&rest) {
        return Some(Intent::Mix("favorites".into()));
    }
    for prefix in MIX_PREFIXES {
        if let Some(name) = rest.strip_prefix(prefix) {
            let name = name.trim();
            if !name.is_empty() {
                return Some(Intent::Mix(name.to_owned()));
            }
        }
    }
    let (query, kind, explicit) = match EXPLICIT.iter().find_map(|(p, k)| rest.strip_prefix(p).map(|r| (r, *k))) {
        Some((query, kind)) => (query.trim(), kind, true),
        None => (rest, None, false),
    };
    let query = query.trim_matches(|c: char| matches!(c, '"' | '«' | '»')).trim();
    if query.is_empty() || query.len() > 400 {
        return None;
    }
    if !explicit && query.split_whitespace().any(not_music_word) {
        return None;
    }
    Some(Intent::Play(Query { text: query.to_owned(), kind }))
}

/// Фраза про штатную громкость Алисы («громче», «громкость пять»): мы её не перехватываем,
/// но запоминаем, что новый уровень Master надо принять, а не откатывать.
pub fn native_volume_request(text: &str) -> bool {
    let text = normalize(text);
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if matches!(first, "громче" | "тише" | "погромче" | "потише") {
        return true;
    }
    if first == "громкость" {
        return words.next().is_some();
    }
    matches!(
        first,
        "сделай"
            | "поставь"
            | "установи"
            | "измени"
            | "увеличь"
            | "уменьши"
            | "добавь"
            | "убавь"
            | "прибавь"
            | "включи"
            | "выключи"
            | "отключи"
    ) && words.next().is_some_and(|w| matches!(w, "звук" | "громкость" | "громче" | "тише" | "погромче" | "потише"))
}

/// Разбор строки команды локального сокета: `PLAY запрос`, `VOLUME 30`, `MIX daily` и т.п.
pub fn from_control(line: &str) -> Option<Intent> {
    let line = line.trim();
    let (word, arg) = match line.split_once(' ') {
        Some((w, a)) => (w, a.trim()),
        None => (line, ""),
    };
    let ok_text = |s: &str| !s.is_empty() && s.len() <= 400 && !s.contains(['\n', '\r', '\0']);
    Some(match (word, arg) {
        ("RESUME", "") => Intent::Resume,
        ("PAUSE", "") => Intent::Pause,
        ("TOGGLE", "") => Intent::Toggle,
        ("NEXT", "") => Intent::Next,
        ("PREVIOUS", "") => Intent::Previous,
        ("RADIO", "") => Intent::Radio,
        ("LIKE", "") => Intent::Like,
        ("DISLIKE", "") => Intent::Dislike,
        ("RELEASE", "") => Intent::Release,
        ("RETRY", "") => Intent::Retry,
        ("VOICE", "ON") => Intent::Voice(true),
        ("VOICE", "OFF") => Intent::Voice(false),
        ("VOLUME", n) => Intent::Volume(n.parse::<u8>().ok().filter(|n| *n <= 100)?),
        ("MIX", m) if ok_text(m) => Intent::Mix(m.to_owned()),
        ("PLAY", q) if ok_text(q) => {
            // Те же приставки, что и в голосе: `PLAY альбом OK Computer`.
            let normalized = normalize(q);
            let (text, kind) = EXPLICIT
                .iter()
                .find_map(|(p, k)| normalized.strip_prefix(p).filter(|_| k.is_some()).map(|r| (r.trim(), *k)))
                .unwrap_or((q, None));
            if text.is_empty() {
                return None;
            }
            Intent::Play(Query { text: text.to_owned(), kind })
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(text: &str, kind: Option<Kind>) -> Option<Intent> {
        Some(Intent::Play(Query { text: text.into(), kind }))
    }

    #[test]
    fn transport_phrases() {
        assert_eq!(from_speech("Останови музыку"), Some(Intent::Pause));
        assert_eq!(from_speech("пауза!"), Some(Intent::Pause));
        assert_eq!(from_speech("Следующий трек."), Some(Intent::Next));
        assert_eq!(from_speech("Алиса, пропусти песню"), Some(Intent::Next));
        assert_eq!(from_speech("предыдущий трек"), Some(Intent::Previous));
        assert_eq!(from_speech("Продолжи музыку"), Some(Intent::Resume));
        assert_eq!(from_speech("Включи музыку"), Some(Intent::Resume));
        assert_eq!(from_speech("мне нравится эта песня"), Some(Intent::Like));
        assert_eq!(from_speech("Поставь лайк"), Some(Intent::Like));
        assert_eq!(from_speech("Мне не нравится этот трек"), Some(Intent::Dislike));
    }

    #[test]
    fn play_requests_keep_kind_and_text() {
        assert_eq!(from_speech("Включи Radiohead"), play("radiohead", None));
        assert_eq!(from_speech("включи песню Всё идёт по плану"), play("все идет по плану", Some(Kind::Track)));
        assert_eq!(from_speech("включи альбом OK Computer"), play("ok computer", Some(Kind::Album)));
        assert_eq!(from_speech("поставь группу Кино"), play("кино", Some(Kind::Artist)));
        assert_eq!(from_speech("включи музыку Эминем"), play("эминем", None));
        assert_eq!(from_speech("запусти песни Баста"), play("баста", None));
        assert_eq!(from_speech("сыграй «Нирвана»"), play("нирвана", None));
    }

    #[test]
    fn radio_favorites_and_mixes() {
        assert_eq!(from_speech("включи радио"), Some(Intent::Radio));
        assert_eq!(from_speech("включи мою волну"), Some(Intent::Radio));
        assert_eq!(from_speech("включи что-нибудь"), Some(Intent::Radio));
        assert_eq!(from_speech("включи любимое"), Some(Intent::Mix("favorites".into())));
        assert_eq!(from_speech("включи мои любимые песни"), Some(Intent::Mix("favorites".into())));
        assert_eq!(from_speech("включи микс дня"), Some(Intent::Mix("дня".into())));
        assert_eq!(from_speech("запусти мою подборку для тебя"), Some(Intent::Mix("для тебя".into())));
    }

    #[test]
    fn named_radio_stations_and_household_stay_with_alice() {
        for text in [
            "включи свет",
            "включи свет в спальне",
            "поставь таймер на пять минут",
            "включи телевизор",
            "включи блютуз",
            "какая погода",
            "включи радио маяк",
            "включи сказку",
            "включи новости",
            "включи белый шум",
            "включи яндекс музыку",
            "включи фильм",
            "",
            "включи ",
        ] {
            assert_eq!(from_speech(text), None, "{text}");
        }
        // С явной приставкой название может быть любым.
        assert_eq!(from_speech("включи песню Свет"), play("свет", Some(Kind::Track)));
    }

    #[test]
    fn native_volume_phrases_are_recognized_but_music_is_not() {
        for text in [
            "громче",
            "тише!",
            "погромче",
            "громкость пять",
            "сделай потише",
            "увеличь громкость",
            "выключи звук",
            "Алиса, установи громкость 3",
        ] {
            assert!(native_volume_request(text), "{text}");
        }
        for text in
            ["какая погода", "какая сейчас громкость", "включи песню Тише", "включи Radiohead", "поставь таймер"]
        {
            assert!(!native_volume_request(text), "{text}");
        }
    }

    #[test]
    fn control_lines() {
        assert_eq!(from_control("PAUSE\n"), Some(Intent::Pause));
        assert_eq!(from_control("VOLUME 30"), Some(Intent::Volume(30)));
        assert_eq!(from_control("VOLUME 101"), None);
        assert_eq!(from_control("VOLUME -1"), None);
        assert_eq!(from_control("VOICE OFF"), Some(Intent::Voice(false)));
        assert_eq!(from_control("MIX daily"), Some(Intent::Mix("daily".into())));
        assert_eq!(from_control("PLAY no surprises"), play("no surprises", None));
        assert_eq!(from_control("PLAY альбом OK Computer"), play("ok computer", Some(Kind::Album)));
        assert_eq!(from_control("PLAY "), None);
        assert_eq!(from_control("PLAY a\nPAUSE"), None);
        assert_eq!(from_control("PAUSE now"), None);
        assert_eq!(from_control("RETRY extra"), None);
        assert_eq!(from_control("BOGUS"), None);
    }

    #[test]
    fn only_transport_needs_an_active_session() {
        assert!(Intent::Next.needs_active_session());
        assert!(Intent::Like.needs_active_session());
        assert!(!Intent::Radio.needs_active_session());
        assert!(!Intent::Resume.needs_active_session());
    }
}
