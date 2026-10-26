//! Каталог треков в памяти и нечёткий поиск по голосовому запросу.
//!
//! Алиса распознаёт речь кириллицей: «Рэдиохэд», «Эминем», «Линкин Парк», а в библиотеке
//! названия латиницей. Поэтому оба текста приводятся к общей «фонетической» записи
//! (транслитерация + упрощение английской орфографии) и сравниваются по ней и по каркасу из согласных.

use crate::intent::{Kind, Query};
use crate::util::nullable;
use serde::Deserialize;
use std::collections::HashMap;

/// Строка `/api/library`; лишние поля сервера игнорируются.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct Row {
    pub id: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub artist: String,
    #[serde(default, deserialize_with = "nullable")]
    pub artists: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub album_artist: String,
    #[serde(default, deserialize_with = "nullable")]
    pub title: String,
    #[serde(default, deserialize_with = "nullable")]
    pub album: String,
    #[serde(default, deserialize_with = "nullable")]
    #[allow(dead_code)]
    pub duration: f64,
    /// Сервер отдаёт `ready:false` для треков, которые ещё не обработаны.
    #[serde(default = "yes")]
    pub ready: bool,
}
fn yes() -> bool {
    true
}

// ---------------------------------------------------------------- фонетика

fn cyrillic(c: char) -> Option<&'static str> {
    Some(match c {
        'а' => "a",
        'б' => "b",
        'в' => "v",
        'г' => "g",
        'д' => "d",
        'е' | 'э' => "e",
        'ё' => "e",
        'ж' => "zh",
        'з' => "z",
        'и' | 'ы' | 'й' | 'і' | 'ї' => "i",
        'к' => "k",
        'л' => "l",
        'м' => "m",
        'н' => "n",
        'о' => "o",
        'п' => "p",
        'р' => "r",
        'с' => "s",
        'т' => "t",
        'у' => "u",
        'ф' => "f",
        'х' => "h",
        'ц' => "ts",
        'ч' => "ch",
        'ш' | 'щ' => "sh",
        'ъ' | 'ь' => "",
        'ю' => "iu",
        'я' => "ia",
        _ => return None,
    })
}

fn fold_accent(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
        'ç' | 'ć' | 'č' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'ē' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
        'ñ' | 'ń' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' => 'u',
        'ý' | 'ÿ' => 'y',
        'š' | 'ś' => 's',
        'ž' | 'ź' | 'ż' => 'z',
        'ł' => 'l',
        _ => c,
    }
}

/// Слова: нижний регистр, кириллица → латиница, прочие знаки → пробелы.
fn raw_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        let c = fold_accent(c);
        if c == '\'' || c == '’' || c == '`' {
            continue;
        }
        if let Some(latin) = cyrillic(c) {
            current.push_str(latin);
        } else if c.is_ascii_alphanumeric() {
            current.push(c);
        } else if c == 'ß' {
            current.push_str("ss");
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// Упрощение английской орфографии до звуков, близких к русской транскрипции.
fn canonical_word(word: &str) -> String {
    let chars: Vec<char> = word.chars().collect();
    let mut out = String::with_capacity(word.len());
    let mut i = 0;
    let at = |k: usize| chars.get(k).copied();
    while i < chars.len() {
        let c = chars[i];
        let next = at(i + 1);
        let third = at(i + 2);
        let (emit, advance): (&str, usize) = match (c, next, third) {
            ('s', Some('c'), Some('h')) => ("sh", 3),
            ('t', Some('c'), Some('h')) => ("ch", 3),
            ('d', Some('z'), Some('h')) => ("zh", 3),
            ('c', Some('h'), _) => ("ch", 2),
            ('s', Some('h'), _) => ("sh", 2),
            ('z', Some('h'), _) => ("zh", 2),
            ('p', Some('h'), _) => ("f", 2),
            ('c', Some('k'), _) => ("k", 2),
            ('k', Some('h'), _) => ("h", 2),
            ('g', Some('h'), _) => ("g", 2),
            ('t', Some('h'), _) => ("t", 2),
            ('w', Some('h'), _) => ("v", 2),
            ('q', Some('u'), _) => ("ku", 2),
            ('e', Some('e'), _) => ("i", 2),
            ('o', Some('o'), _) => ("u", 2),
            ('c', Some('e' | 'i' | 'y'), _) => ("s", 1),
            ('c', _, _) => ("k", 1),
            ('x', _, _) => ("ks", 1),
            ('q', _, _) => ("k", 1),
            ('w', _, _) => ("v", 1),
            ('j', _, _) => ("zh", 1),
            ('y', _, _) => ("i", 1),
            _ => ("", 0),
        };
        if advance > 0 {
            out.push_str(emit);
            i += advance;
        } else {
            out.push(c);
            i += 1;
        }
    }
    // «ll» → «l»: удвоенные буквы не слышны.
    let mut dedup = String::with_capacity(out.len());
    let mut prev = None;
    for c in out.chars() {
        if Some(c) != prev || c.is_ascii_digit() {
            dedup.push(c);
        }
        prev = Some(c);
    }
    dedup
}

/// Каркас слова: согласные без озвончённости s/z, гласные выброшены.
fn skeleton_word(word: &str) -> String {
    word.chars().filter(|c| !is_vowel(*c)).map(|c| if c == 'z' { 's' } else { c }).collect()
}

/// Фонетический ключ текста.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Key {
    /// Слова через пробел в упрощённой латинской записи.
    pub phon: String,
    /// Согласные всех слов подряд.
    pub skel: String,
    pub tokens: Vec<String>,
}

pub fn key(text: &str) -> Key {
    let mut tokens: Vec<String> = raw_words(text).iter().map(|w| canonical_word(w)).filter(|w| !w.is_empty()).collect();
    // Артикль и «зэ» в начале названия не помогают: «The Beatles» ≈ «Битлз».
    if tokens.len() > 1 && matches!(tokens[0].as_str(), "te" | "ze" | "dze" | "the" | "a") {
        tokens.remove(0);
    }
    let phon = tokens.join(" ");
    let skel: String = tokens.iter().map(|t| skeleton_word(t)).collect();
    Key { phon, skel, tokens }
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn ratio(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let longest = ca.len().max(cb.len());
    if longest == 0 {
        return 0.0;
    }
    1.0 - levenshtein(&ca, &cb) as f32 / longest as f32
}

/// Похожесть двух слов (0..1): по звукам, а для длинных слов ещё и по каркасу из согласных.
fn word_similarity(a: &str, b: &str) -> f32 {
    let phon = ratio(a, b);
    let (sa, sb) = (skeleton_word(a), skeleton_word(b));
    if sa.chars().count() >= 3 && sb.chars().count() >= 3 {
        phon.max(0.96 * ratio(&sa, &sb))
    } else {
        phon
    }
}

/// Похожесть запроса на кандидата, 0..1.
pub fn similarity(q: &Key, c: &Key) -> f32 {
    if q.phon.is_empty() || c.phon.is_empty() {
        return 0.0;
    }
    if q.phon == c.phon {
        return 1.0;
    }
    let mut best = ratio(&q.phon, &c.phon);
    if q.skel.chars().count() >= 4 && c.skel.chars().count() >= 4 {
        best = best.max(0.96 * ratio(&q.skel, &c.skel));
    }
    // Запрос — часть названия: «Creep» в «Creep (Live)». Каждое слово запроса должно найтись.
    if c.tokens.len() > q.tokens.len() {
        let mut total = 0.0;
        let mut all = true;
        for qt in &q.tokens {
            let found = c.tokens.iter().map(|ct| word_similarity(qt, ct)).fold(0.0f32, f32::max);
            if found < 0.8 {
                all = false;
                break;
            }
            total += found;
        }
        if all && !q.tokens.is_empty() {
            let avg = total / q.tokens.len() as f32;
            // Чем больше лишнего в названии, тем слабее совпадение, но не ниже 0.84·avg.
            let coverage = q.tokens.len() as f32 / c.tokens.len() as f32;
            best = best.max(avg * (0.84 + 0.1 * coverage));
        }
    }
    best
}

// ---------------------------------------------------------------- каталог

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Artist(String),
    Album { artist: String, album: String },
    Track(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub target: Target,
    pub score: f32,
}

struct TrackEntry {
    id: i64,
    title: Key,
    with_artist: Key,
}
struct ArtistEntry {
    name: String,
    key: Key,
}
struct AlbumEntry {
    artist: String,
    album: String,
    key: Key,
    with_artist: Key,
}

pub struct Catalog {
    rows: Vec<Row>,
    tracks: Vec<TrackEntry>,
    artists: Vec<ArtistEntry>,
    albums: Vec<AlbumEntry>,
}

impl Catalog {
    pub fn from_rows(rows: Vec<Row>) -> Self {
        let rows: Vec<Row> = rows.into_iter().filter(|r| r.ready).collect();
        let mut tracks = Vec::with_capacity(rows.len());
        let mut artist_names: HashMap<String, String> = HashMap::new();
        let mut album_names: HashMap<(String, String), (String, String)> = HashMap::new();
        for row in &rows {
            let artist_text = if row.artist.is_empty() { row.album_artist.as_str() } else { row.artist.as_str() };
            tracks.push(TrackEntry {
                id: row.id,
                title: key(&row.title),
                with_artist: key(&format!("{artist_text} {}", row.title)),
            });
            let mut names: Vec<&str> = row.artists.iter().map(String::as_str).collect();
            names.push(row.artist.as_str());
            if !row.album_artist.is_empty() {
                names.push(row.album_artist.as_str());
            }
            for name in names {
                let name = name.trim();
                if !name.is_empty() {
                    artist_names.entry(name.to_lowercase()).or_insert_with(|| name.to_owned());
                }
            }
            if !row.album.trim().is_empty() {
                let owner = if row.album_artist.is_empty() { row.artist.clone() } else { row.album_artist.clone() };
                album_names
                    .entry((owner.to_lowercase(), row.album.to_lowercase()))
                    .or_insert_with(|| (owner, row.album.trim().to_owned()));
            }
        }
        let artists = artist_names.into_values().map(|name| ArtistEntry { key: key(&name), name }).collect();
        let albums = album_names
            .into_values()
            .map(|(artist, album)| AlbumEntry {
                key: key(&album),
                with_artist: key(&format!("{artist} {album}")),
                artist,
                album,
            })
            .collect();
        Self { rows, tracks, artists, albums }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Лучшее совпадение с учётом того, что сказал пользователь (альбом/песня/исполнитель).
    pub fn search(&self, query: &Query, threshold: f32) -> Option<Match> {
        let q = key(&query.text);
        if q.phon.is_empty() {
            return None;
        }
        let mut best: Option<Match> = None;
        let mut offer = |target: Target, score: f32| {
            if score >= threshold && best.as_ref().is_none_or(|b| score > b.score) {
                best = Some(Match { target, score });
            }
        };
        if matches!(query.kind, None | Some(Kind::Artist)) {
            let top = self.artists.iter().map(|a| (similarity(&q, &a.key), a)).max_by(|x, y| x.0.total_cmp(&y.0));
            if let Some((score, a)) = top {
                offer(Target::Artist(a.name.clone()), score);
            }
        }
        if matches!(query.kind, None | Some(Kind::Album)) {
            let top = self
                .albums
                .iter()
                .map(|a| (similarity(&q, &a.key).max(similarity(&q, &a.with_artist)), a))
                .max_by(|x, y| x.0.total_cmp(&y.0));
            if let Some((score, a)) = top {
                offer(Target::Album { artist: a.artist.clone(), album: a.album.clone() }, score * 0.98);
            }
        }
        if matches!(query.kind, None | Some(Kind::Track)) {
            let top = self
                .tracks
                .iter()
                .map(|t| (similarity(&q, &t.title).max(similarity(&q, &t.with_artist)), t))
                .max_by(|x, y| x.0.total_cmp(&y.0));
            if let Some((score, t)) = top {
                // Явное «включи песню X» не штрафуется; без приставки трек уступает артисту и альбому при равенстве.
                let weight = if query.kind.is_some() { 1.0 } else { 0.96 };
                offer(Target::Track(t.id), score * weight);
            }
        }
        best
    }

    /// Идентификаторы треков артиста: те же правила, что у `/api/library?artist=` на сервере
    /// (имя, альбомный артист или любой из соавторов, без учёта регистра).
    pub fn artist_tracks(&self, name: &str) -> Vec<i64> {
        let want = name.trim().to_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                r.artist.trim().to_lowercase() == want
                    || (!r.album_artist.is_empty() && r.album_artist.trim().to_lowercase() == want)
                    || r.artists.iter().any(|a| a.trim().to_lowercase() == want)
            })
            .map(|r| r.id)
            .collect()
    }

    pub fn album_tracks(&self, artist: &str, album: &str) -> Vec<i64> {
        let (artist, album) = (artist.trim().to_lowercase(), album.trim().to_lowercase());
        self.rows
            .iter()
            .filter(|r| {
                r.album.trim().to_lowercase() == album
                    && (artist.is_empty()
                        || r.artist.trim().to_lowercase() == artist
                        || r.album_artist.trim().to_lowercase() == artist
                        || r.artists.iter().any(|a| a.trim().to_lowercase() == artist))
            })
            .map(|r| r.id)
            .collect()
    }

    pub fn row(&self, id: i64) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, artist: &str, title: &str, album: &str) -> Row {
        Row { id, artist: artist.into(), title: title.into(), album: album.into(), ready: true, ..Row::default() }
    }

    fn sample() -> Catalog {
        Catalog::from_rows(vec![
            row(1, "Radiohead", "Creep", "Pablo Honey"),
            row(2, "Radiohead", "No Surprises", "OK Computer"),
            row(3, "Radiohead", "Paranoid Android", "OK Computer"),
            row(4, "Eminem", "Lose Yourself", "8 Mile"),
            row(5, "Кино", "Группа крови", "Группа крови"),
            row(6, "Кино", "Звезда по имени Солнце", "Звезда по имени Солнце"),
            row(7, "Linkin Park", "Numb", "Meteora"),
            row(8, "Queen", "Bohemian Rhapsody", "A Night at the Opera"),
            row(9, "The Beatles", "Yesterday", "Help!"),
            row(10, "Nirvana", "Smells Like Teen Spirit", "Nevermind"),
            Row { ready: false, ..row(11, "Hidden", "Not Ready", "X") },
        ])
    }

    fn q(text: &str, kind: Option<Kind>) -> Query {
        Query { text: crate::intent::normalize(text), kind }
    }

    #[test]
    fn cyrillic_and_latin_spellings_share_a_key() {
        for (cyr, lat) in [
            ("рэдиохэд", "Radiohead"),
            ("эминем", "Eminem"),
            ("линкин парк", "Linkin Park"),
            ("нирвана", "Nirvana"),
            ("металлика", "Metallica"),
            ("битлз", "The Beatles"),
            ("куин", "Queen"),
        ] {
            let (a, b) = (key(cyr), key(lat));
            assert!(similarity(&a, &b) >= 0.8, "{cyr} ~ {lat}: {a:?} {b:?} = {}", similarity(&a, &b));
        }
    }

    #[test]
    fn different_names_do_not_match() {
        for (a, b) in [("кино", "Radiohead"), ("эминем", "Nirvana"), ("нирвана", "Queen"), ("баста", "Metallica")]
        {
            assert!(similarity(&key(a), &key(b)) < 0.72, "{a} !~ {b}");
        }
    }

    #[test]
    fn finds_artist_by_russian_transcription() {
        let c = sample();
        let m = c.search(&q("рэдиохэд", None), 0.72).unwrap();
        assert_eq!(m.target, Target::Artist("Radiohead".into()));
        let m = c.search(&q("эминем", None), 0.72).unwrap();
        assert_eq!(m.target, Target::Artist("Eminem".into()));
        let m = c.search(&q("кино", None), 0.72).unwrap();
        assert_eq!(m.target, Target::Artist("Кино".into()));
    }

    #[test]
    fn finds_tracks_albums_and_respects_the_kind() {
        let c = sample();
        let m = c.search(&q("креп", Some(Kind::Track)), 0.7).unwrap();
        assert_eq!(m.target, Target::Track(1));
        let m = c.search(&q("no surprises", None), 0.72).unwrap();
        assert_eq!(m.target, Target::Track(2));
        let m = c.search(&q("ок компьютер", Some(Kind::Album)), 0.72).unwrap();
        assert_eq!(m.target, Target::Album { artist: "Radiohead".into(), album: "OK Computer".into() });
        let m = c.search(&q("богемская рапсодия", Some(Kind::Track)), 0.6);
        assert!(m.is_none() || matches!(m.unwrap().target, Target::Track(8)));
        let m = c.search(&q("bohemian rhapsody", None), 0.72).unwrap();
        assert_eq!(m.target, Target::Track(8));
        let m = c.search(&q("звезда по имени солнце", Some(Kind::Track)), 0.72).unwrap();
        assert_eq!(m.target, Target::Track(6));
    }

    #[test]
    fn artist_and_title_together_find_the_track() {
        let c = sample();
        let m = c.search(&q("radiohead creep", None), 0.72).unwrap();
        assert_eq!(m.target, Target::Track(1));
        let m = c.search(&q("нирвана смэлс лайк тин спирит", None), 0.6).unwrap();
        assert_eq!(m.target, Target::Track(10));
    }

    #[test]
    fn unknown_queries_and_unready_tracks_find_nothing() {
        let c = sample();
        assert!(c.search(&q("абракадабра", None), 0.72).is_none());
        assert!(c.search(&q("hidden", None), 0.72).is_none());
        assert!(c.search(&q("", None), 0.72).is_none());
        assert_eq!(c.len(), 10);
    }

    #[test]
    fn collections_use_server_matching_rules() {
        let mut rows =
            vec![row(1, "Basta", "A", "Alb"), row(2, "Basta feat. Guf", "B", "Alb"), row(3, "Other", "C", "Alb2")];
        rows[1].artists = vec!["Basta".into(), "Guf".into()];
        rows[2].album_artist = "Basta".into();
        let c = Catalog::from_rows(rows);
        assert_eq!(c.artist_tracks("BASTA"), vec![1, 2, 3]);
        assert_eq!(c.artist_tracks("guf"), vec![2]);
        assert_eq!(c.album_tracks("Basta", "alb"), vec![1, 2]);
        assert_eq!(c.album_tracks("", "Alb2"), vec![3]);
    }

    #[test]
    fn library_json_with_nulls_parses() {
        let json = r#"[{"id":1,"artist":"A","title":null,"album":"X","duration":12.5,"cluster_id":3,"ready":true,"extra":1},
                       {"id":2,"artist":null,"artists":null,"title":"T","ready":false}]"#;
        let rows: Vec<Row> = serde_json::from_str(json).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "");
        assert!(!rows[1].ready);
    }

    #[test]
    fn levenshtein_basics() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(levenshtein(&c("kitten"), &c("sitting")), 3);
        assert_eq!(levenshtein(&c(""), &c("abc")), 3);
        assert_eq!(levenshtein(&c("abc"), &c("abc")), 0);
    }
}
